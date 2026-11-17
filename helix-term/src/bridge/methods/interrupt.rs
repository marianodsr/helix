//! "Not yanking the user": idle tracking, the pending jump and `:bb-go`.
//!
//! Every request that moves the user's focus goes through [`jump`]. With
//! `interrupt: "whenIdle"` (the default), when the user is busy (insert mode,
//! a key pressed in the last 1500 ms, a key sequence half typed, or a prompt
//! or picker open) Helix does not move: the move is kept as the pending jump
//! (one slot, the newest wins), the status line says so, and `:bb-go`
//! performs it. `"always"` moves at once.
//!
//! The key path pays for one timestamp store: hooks on `PostCommand` and
//! `PostInsertChar` (which fire for every key bound to a command and every
//! character typed) record when the user last pressed a key. Keys the bridge
//! itself feeds (`editor/command`) are not counted, see [`as_bridge`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::bail;
use helix_core::command_line::{Args, Signature};
use helix_event::{register_hook, runtime_local};
use helix_view::document::Mode;
use helix_view::Editor;
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::bridge::convert::{doc_num, path_string};
use crate::bridge::dispatch::on_editor;
use crate::bridge::methods::command::{layers, Layer};
use crate::bridge::methods::editor::{open_on_main, view_from_num};
use crate::bridge::protocol::{OpenParams, RpcError, RpcResult};
use crate::commands::typed::{CommandCompleter, TypableCommand};
use crate::compositor::{self, Compositor};
use crate::events::{PostCommand, PostInsertChar};
use crate::job::Callback;
use crate::ui::{EditorView, PromptEvent};

/// A key pressed this recently means the user is typing.
pub const IDLE_WINDOW: Duration = Duration::from_millis(1500);
/// How long a deferred jump keeps re-showing its notice once the user pauses.
const REMIND_FOR: Duration = Duration::from_secs(60);

/// `:bb-go`.
pub const COMMANDS: &[TypableCommand] = &[TypableCommand {
    name: "bb-go",
    aliases: &[],
    doc: "Go where bb asked to take you while you were busy.",
    fun: bb_go,
    completer: CommandCompleter::none(),
    signature: Signature {
        positionals: (0, Some(0)),
        ..Signature::DEFAULT
    },
}];

/// The `interrupt` request parameter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Interrupt {
    /// Wait while the user is busy (the default).
    #[default]
    WhenIdle,
    /// Move at once.
    Always,
}

impl Interrupt {
    pub fn parse(value: Option<&str>) -> Result<Interrupt, RpcError> {
        match value {
            None | Some("whenIdle") => Ok(Interrupt::WhenIdle),
            Some("always") => Ok(Interrupt::Always),
            Some(_) => Err(RpcError::invalid_params(
                "interrupt",
                "interrupt must be \"whenIdle\" or \"always\"",
            )),
        }
    }
}

type Run = Box<dyn FnOnce(&mut Editor, &mut Compositor) + Send>;

struct Pending {
    run: Run,
}

/// Base of the key timestamps; shared by every runtime, it is only a zero.
static EPOCH: OnceLock<Instant> = OnceLock::new();

runtime_local! {
    /// Milliseconds after `EPOCH` of the user's last key press; 0 for none.
    static LAST_KEY: AtomicU64 = AtomicU64::new(0);
    /// Bumped whenever the pending slot changes, so stale reminders stop.
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
}

#[inline]
fn now_ms() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Hooks this module needs (registered when the bridge starts).
pub fn register_hooks() {
    register_hook!(move |_event: &mut PostCommand<'_, '_>| {
        LAST_KEY.store(now_ms(), Ordering::Relaxed);
        Ok(())
    });
    register_hook!(move |_event: &mut PostInsertChar<'_, '_>| {
        LAST_KEY.store(now_ms(), Ordering::Relaxed);
        Ok(())
    });
}

/// Time since the user last pressed a key, `None` if they have not yet.
pub fn since_last_key() -> Option<Duration> {
    match LAST_KEY.load(Ordering::Relaxed) {
        0 => None,
        at => Some(Duration::from_millis(now_ms().saturating_sub(at))),
    }
}

/// Run `f`, which feeds keys or runs commands for a client, without counting
/// them as the user's key presses.
pub fn as_bridge<R>(f: impl FnOnce() -> R) -> R {
    let saved = LAST_KEY.load(Ordering::Relaxed);
    let result = f();
    LAST_KEY.store(saved, Ordering::Relaxed);
    result
}

/// Why the user counts as busy, or `None` when a move may happen now.
pub fn busy(editor: &Editor, compositor: &mut Compositor) -> Option<&'static str> {
    if editor.mode() == Mode::Insert {
        return Some("in insert mode");
    }
    if since_last_key().is_some_and(|d| d < IDLE_WINDOW) {
        return Some("typing");
    }
    if compositor
        .find::<EditorView>()
        .is_some_and(|view| !view.keymaps.pending().is_empty())
    {
        return Some("in the middle of a key sequence");
    }
    if compositor.layer_count() > 1 {
        let modal = layers(compositor, |layers| {
            layers.iter().any(|layer| !matches!(layer, Layer::Popup))
        });
        if modal {
            return Some("in a prompt or picker");
        }
    }
    None
}

/// Perform `act` now, or keep it as the pending jump when `interrupt` is
/// `whenIdle` and the user is busy. `what` completes the status-line notice
/// "bb wants to <what> (:bb-go)", for example `show src/a.go:42`.
///
/// Returns `Ok(Some(result))` when it moved, `Ok(None)` when deferred. A
/// deferred `act` that fails later shows its error on the status line.
pub fn jump<R, F>(
    editor: &mut Editor,
    compositor: &mut Compositor,
    interrupt: Interrupt,
    what: String,
    act: F,
) -> Result<Option<R>, RpcError>
where
    F: FnOnce(&mut Editor, &mut Compositor) -> Result<R, RpcError> + Send + 'static,
{
    if interrupt == Interrupt::Always || busy(editor, compositor).is_none() {
        return act(editor, compositor).map(Some);
    }
    let notice = format!("bb wants to {what} (:bb-go)");
    let run: Run = Box::new(move |editor, compositor| {
        if let Err(err) = act(editor, compositor) {
            editor.set_error(err.message);
        }
    });
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    *PENDING.lock() = Some(Pending { run });
    editor.set_status(notice.clone());
    remind(generation, notice);
    Ok(None)
}

/// The user's next key clears the notice, and they were typing when it was
/// shown. Show it again once they pause, unless something else is on the
/// status line or the jump was replaced or performed.
fn remind(generation: u64, notice: String) {
    tokio::spawn(async move {
        let deadline = Instant::now() + REMIND_FOR;
        while Instant::now() < deadline {
            tokio::time::sleep(IDLE_WINDOW + Duration::from_millis(100)).await;
            let notice = notice.clone();
            let done = on_editor(move |editor, _| {
                if GENERATION.load(Ordering::Relaxed) != generation || PENDING.lock().is_none() {
                    return true;
                }
                if since_last_key().is_some_and(|d| d < IDLE_WINDOW) || editor.status_msg.is_some()
                {
                    return false;
                }
                editor.set_status(notice);
                true
            })
            .await
            .unwrap_or(true);
            if done {
                break;
            }
        }
    });
}

fn bb_go(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let Some(pending) = PENDING.lock().take() else {
        bail!("Nothing from bb is waiting");
    };
    GENERATION.fetch_add(1, Ordering::Relaxed);
    // Typed commands cannot reach the compositor; the job queue can.
    cx.jobs.callback(async move {
        let call = move |editor: &mut Editor, compositor: &mut Compositor| {
            (pending.run)(editor, compositor)
        };
        Ok(Callback::EditorCompositor(Box::new(call)))
    });
    Ok(())
}

/// `editor/open` with `interrupt`: a background open never waits; a
/// deferred one answers without touching the editor.
pub fn open(
    editor: &mut Editor,
    compositor: &mut Compositor,
    interrupt: Interrupt,
    path: PathBuf,
    exists: bool,
    params: OpenParams,
) -> RpcResult {
    if !params.focus {
        let result = open_on_main(editor, &path, exists, params)?;
        return Ok(serde_json::to_value(result).unwrap_or_default());
    }
    let open = editor
        .document_id_by_path(&path)
        .and_then(|id| editor.document(id));
    if !exists && open.is_none() {
        return Err(RpcError::not_found(format!("{} not found", path.display())));
    }
    if let Some(view) = params.view_id {
        if !editor.tree.contains(view_from_num(view)) {
            return Err(RpcError::not_found(format!("no view {view}")));
        }
    }
    let deferred = json!({
        "docId": open.map(|doc| doc_num(doc.id())),
        "path": path_string(Some(&path)),
        "version": open.map(|doc| doc.version()),
        "viewId": Value::Null,
        "deferred": true,
    });
    let what = format!("show {}", location(&path, params.line));
    let moved = jump(editor, compositor, interrupt, what, move |editor, _| {
        open_on_main(editor, &path, exists, params)
    })?;
    Ok(match moved {
        Some(result) => serde_json::to_value(result).unwrap_or_default(),
        None => deferred,
    })
}

/// `src/a.go:42` for the notice: relative to the working directory when
/// inside it.
pub fn location(path: &Path, line: Option<usize>) -> String {
    let path = helix_stdx::path::get_relative_path(path);
    match line {
        Some(line) => format!("{}:{line}", path.display()),
        None => path.display().to_string(),
    }
}
