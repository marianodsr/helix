//! `:bb-files`: the file tree.
//!
//! Inside bb a client subscribed to `files` shows its own tree: the command
//! only emits `event/files`. In an ordinary terminal Helix runs the file
//! manager (`[editor.bridge] file-manager`, yazi by default) itself, full
//! screen, as a chooser, and opens what it writes to the chooser file.
//!
//! Running a full-screen program needs the terminal, which the application
//! owns. The command only records what to run; the application hands the
//! terminal over at the start of its next frame ([`hand_over_terminal`]),
//! the way it does for its own suspend: restore the terminal (main screen,
//! cooked mode, no mouse capture, keyboard protocol popped), run the program
//! attached to it, claim it back and redraw everything. Helix's input
//! thread reads the terminal for as long as its event stream lives, so the
//! stream is dropped while the program runs ([`pausable_input`]) and made
//! again at the next poll; otherwise it would take the program's keys.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::bail;
use futures_util::Stream;
use helix_core::command_line::{Args, Signature};
use helix_event::runtime_local;
use helix_view::editor::Action;
use helix_view::graphics::CursorKind;
use helix_view::Editor;
use parking_lot::Mutex;
use serde::Serialize;
use tui::backend::Backend;
use tui::terminal::Terminal;

use crate::bridge::convert::{doc_num, open_path, path_string};
use crate::bridge::events::{self, broadcast, Kind};
use crate::commands::typed::{CommandCompleter, TypableCommand};
use crate::compositor::{self, Compositor};
use crate::ui::PromptEvent;

pub const COMMANDS: &[TypableCommand] = &[TypableCommand {
    name: "bb-files",
    aliases: &[],
    doc: "Open the file tree: bb's when bb is listening, else the file manager ([editor.bridge] file-manager) as a chooser.",
    fun: bb_files,
    completer: CommandCompleter::none(),
    signature: Signature {
        positionals: (0, Some(0)),
        ..Signature::DEFAULT
    },
}];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FilesEvent {
    path: Option<String>,
    doc_id: usize,
    cwd: String,
}

/// A file manager to run at the next frame.
struct Chooser {
    argv: Vec<String>,
    file: PathBuf,
}

runtime_local! {
    /// Something waits for the terminal: the only cost per frame.
    static WANTED: AtomicBool = AtomicBool::new(false);
    static CHOOSER: Mutex<Option<Chooser>> = Mutex::new(None);
    static SEQ: AtomicU64 = AtomicU64::new(0);
}

thread_local! {
    /// Drops the application's input stream (set by [`pausable_input`]).
    /// The stream is made, polled and dropped on the main thread only.
    static DROP_INPUT: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

fn bb_files(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let doc = doc!(cx.editor);
    let path = doc.path().map(Path::to_path_buf);
    let cwd = helix_stdx::env::current_working_dir();
    if events::wants_kind(Kind::Files) {
        if let Some(shared) = crate::bridge::shared() {
            broadcast(
                shared,
                Kind::Files,
                "event/files",
                FilesEvent {
                    path: path_string(path.as_deref()),
                    doc_id: doc_num(doc.id()),
                    cwd: cwd.to_string_lossy().into_owned(),
                },
                false,
            );
            return Ok(());
        }
    }
    let argv = cx.editor.config().bridge.file_manager.clone();
    let Some(program) = argv.first() else {
        bail!("[editor.bridge] file-manager is empty");
    };
    if helix_stdx::env::which(program).is_err() {
        bail!("{program} not found ([editor.bridge] file-manager)");
    }
    let file = std::env::temp_dir().join(format!(
        "helix-bb-chooser-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&file);
    let start = path.unwrap_or(cwd);
    let argv = argv
        .iter()
        .map(|arg| {
            arg.replace("{chooser}", &file.to_string_lossy())
                .replace("{path}", &start.to_string_lossy())
        })
        .collect();
    *CHOOSER.lock() = Some(Chooser { argv, file });
    WANTED.store(true, Ordering::Relaxed);
    helix_event::request_redraw();
    Ok(())
}

/// Called by the application at the start of every frame. Returns at once
/// unless `:bb-files` asked for the terminal.
pub fn hand_over_terminal<B: Backend>(
    terminal: &mut Terminal<B>,
    editor: &mut Editor,
    compositor: &mut Compositor,
) {
    if !WANTED.load(Ordering::Relaxed) {
        return;
    }
    WANTED.store(false, Ordering::Relaxed);
    let Some(chooser) = CHOOSER.lock().take() else {
        return;
    };
    // Helix's input thread must stop reading the terminal first
    let dropped = DROP_INPUT.with_borrow(|drop_input| drop_input.as_ref().map(|f| f()).is_some());
    if dropped {
        // its thread sees the stream gone and stops polling the terminal
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = terminal.backend_mut().show_cursor(CursorKind::Block);
    if let Err(err) = terminal.restore() {
        editor.set_error(format!("bb: cannot hand the terminal over: {err}"));
        reclaim(terminal, compositor);
        return;
    }
    let status = Command::new(&chooser.argv[0])
        .args(&chooser.argv[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    reclaim(terminal, compositor);
    let chosen = std::fs::read_to_string(&chooser.file).unwrap_or_default();
    let _ = std::fs::remove_file(&chooser.file);
    if let Err(err) = status {
        editor.set_error(format!("bb: {}: {err}", chooser.argv[0]));
        return;
    }
    open_chosen(editor, &chosen);
}

/// Take the terminal back, as after a suspend, and draw everything again.
fn reclaim<B: Backend>(terminal: &mut Terminal<B>, compositor: &mut Compositor) {
    for retries in 1..=10 {
        match terminal.claim() {
            Ok(()) => break,
            Err(err) if retries == 10 => log::error!("bridge: cannot claim the terminal: {err}"),
            Err(_) => continue,
        }
    }
    compositor.resize(terminal.size());
    let _ = terminal.clear();
    compositor.need_full_redraw();
}

/// Open every file the chooser wrote, one per line; the first one focused.
/// Nothing written is a cancel.
fn open_chosen(editor: &mut Editor, chosen: &str) {
    let cwd = helix_stdx::env::current_working_dir();
    let paths: Vec<PathBuf> = chosen
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty())
        .map(|line| helix_stdx::path::canonicalize(cwd.join(line)))
        .collect();
    let mut skipped = Vec::new();
    let mut first = true;
    let mut opened = 0;
    for path in &paths {
        if path.is_dir() {
            skipped.push(path);
            continue;
        }
        // the first file chosen is the one in front, the rest load behind it
        let action = if first { Action::Replace } else { Action::Load };
        // a buffer already open under another name of the same file
        let path = open_path(editor, path);
        match editor.open(&path, action) {
            Ok(_) => {
                first = false;
                opened += 1;
            }
            Err(err) => {
                editor.set_error(format!("bb: {}: {err}", path.display()));
                return;
            }
        }
    }
    if !skipped.is_empty() {
        editor.set_warning(format!(
            "bb: {} director{} not opened",
            skipped.len(),
            if skipped.len() == 1 { "y" } else { "ies" }
        ));
    } else if opened > 1 {
        editor.set_status(format!("Opened {opened} files"));
    }
}

/// The application's input stream, made by `make` whenever it is polled
/// after [`hand_over_terminal`] dropped it.
pub struct PausableInput<S, F> {
    make: F,
    slot: Rc<RefCell<Option<S>>>,
}

pub fn pausable_input<S, F>(make: F) -> PausableInput<S, F>
where
    S: 'static,
    F: Fn() -> S,
{
    let slot = Rc::new(RefCell::new(None));
    let shared = slot.clone();
    DROP_INPUT.set(Some(Box::new(move || drop(shared.borrow_mut().take()))));
    PausableInput { make, slot }
}

impl<S, F> Stream for PausableInput<S, F>
where
    S: Stream + Unpin,
    F: Fn() -> S + Unpin,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let mut slot = this.slot.borrow_mut();
        let stream = slot.get_or_insert_with(&this.make);
        Pin::new(stream).poll_next(cx)
    }
}
