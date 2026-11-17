//! locations/set, locations/clear, the `:bb-locations` picker and
//! `:bb-next`/`:bb-prev` (step 2a).
//!
//! Lists live in a mutex beside the editor, not in it: the bridge task
//! replaces them, the typed commands and the note hook read them. Every
//! jump a client asks for goes through [`request_jump`], the one place the
//! "not yanking the user" rule ([`interrupt::jump`]) applies; jumps the user asks for
//! (`:bb-next`, the picker) call [`jump`] directly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::bail;
use futures_util::future::BoxFuture;
use helix_core::command_line::{Args, Signature};
use helix_core::Selection;
use helix_event::{register_hook, runtime_local};
use helix_view::{align_view, editor::Action, Align, Editor};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::bridge::convert::{
    char_after_line, char_from_pos, focus_view_of, open_path, resolve_path,
};
use crate::bridge::dispatch::on_editor;
use crate::bridge::methods::interrupt::{self, Interrupt};
use crate::bridge::protocol::{parse_params, Position, RpcError, RpcResult};
use crate::bridge::server::Ctx;
use crate::commands::typed::{CommandCompleter, TypableCommand};
use crate::compositor::{self, Compositor};
use crate::events::PostCommand;
use crate::job::Callback;
use crate::ui::{self, overlay::overlaid, PromptEvent};

pub const CAPABILITY: Option<&str> = Some("locations");

/// Most items one list holds.
pub const MAX_ITEMS: usize = 2000;

pub const COMMANDS: &[TypableCommand] = &[
    TypableCommand {
        name: "bb-locations",
        aliases: &[],
        doc: "Pick from a bb locations list (default: the active one) and make it active.",
        fun: bb_locations,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "bb-next",
        aliases: &[],
        doc: "Go to the next item of the active bb locations list.",
        fun: bb_next,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "bb-prev",
        aliases: &[],
        doc: "Go to the previous item of the active bb locations list.",
        fun: bb_prev,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
];

// --- state -------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Item {
    pub path: PathBuf,
    pub line: usize,
    pub col: Option<usize>,
    pub end_line: Option<usize>,
    pub label: Option<String>,
    pub note: Option<String>,
}

impl Item {
    fn last_line(&self) -> usize {
        self.end_line.unwrap_or(self.line).max(self.line)
    }
}

struct List {
    name: String,
    title: String,
    items: Vec<Item>,
    /// Item indices by path, for the note hook.
    by_path: HashMap<PathBuf, Vec<usize>>,
    /// The item last visited or stood on.
    current: Option<usize>,
}

impl List {
    fn has_notes(&self) -> bool {
        self.items.iter().any(|i| i.note.is_some())
    }

    /// `tour 2/5: <note>`, falling back to the label, then the place.
    fn status(&self, index: usize) -> String {
        let item = &self.items[index];
        let text = match (&item.note, &item.label) {
            (Some(note), _) => note.clone(),
            (None, Some(label)) => label.clone(),
            (None, None) => format!(
                "{}:{}",
                helix_stdx::path::get_relative_path(&item.path).display(),
                item.line
            ),
        };
        format!("{} {}/{}: {text}", self.name, index + 1, self.items.len())
    }
}

#[derive(Default)]
struct State {
    lists: Vec<List>,
    active: Option<String>,
    /// The status text the note hook last showed, so it only replaces its
    /// own message, never one a command just put there.
    shown: Option<String>,
}

impl State {
    fn list(&self, name: &str) -> Option<&List> {
        self.lists.iter().find(|l| l.name == name)
    }

    fn list_mut(&mut self, name: &str) -> Option<&mut List> {
        self.lists.iter_mut().find(|l| l.name == name)
    }

    fn active_mut(&mut self) -> Option<&mut List> {
        let name = self.active.clone()?;
        self.list_mut(&name)
    }

    /// Turn the note hook on only while the active list has notes.
    fn refresh_watch(&self) {
        let watch = self
            .active
            .as_deref()
            .and_then(|n| self.list(n))
            .is_some_and(List::has_notes);
        WATCH.store(watch, Ordering::Relaxed);
    }
}

runtime_local! {
    static STATE: Mutex<State> = Mutex::new(State {
        lists: Vec::new(),
        active: None,
        shown: None,
    });
    /// Is the note hook needed? One relaxed load per command otherwise.
    static WATCH: AtomicBool = AtomicBool::new(false);
}

// --- jumping -----------------------------------------------------------------

/// A place to move the focused view to.
#[derive(Debug, Clone)]
pub struct JumpTarget {
    pub path: PathBuf,
    pub line: usize,
    pub col: Option<usize>,
    /// Select through the end of this line.
    pub end_line: Option<usize>,
    /// Shown on the status line after the jump.
    pub status: Option<String>,
}

impl From<&Item> for JumpTarget {
    fn from(item: &Item) -> Self {
        JumpTarget {
            path: item.path.clone(),
            line: item.line,
            col: item.col,
            end_line: item.end_line,
            status: None,
        }
    }
}

#[derive(Debug)]
pub enum JumpError {
    /// The file is neither open nor on disk.
    Gone(PathBuf),
    Open(PathBuf, String),
}

impl std::fmt::Display for JumpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JumpError::Gone(path) => write!(
                f,
                "{} no longer exists",
                helix_stdx::path::get_relative_path(path).display()
            ),
            JumpError::Open(path, err) => write!(f, "{}: {err}", path.display()),
        }
    }
}

fn exists(editor: &Editor, path: &Path) -> bool {
    editor
        .document_id_by_path(&open_path(editor, path))
        .is_some()
        || path.is_file()
}

/// Every jump a client requests (`locations/set` with `goto`,
/// `editor/reload` with `follow`), through the "not yanking the user" gate:
/// while the user is busy it becomes the pending jump for `:bb-go`.
/// `then` runs right after the jump, whenever it happens. Returns whether
/// it was deferred; a jump that fails is reported on the status line.
pub fn request_jump(
    editor: &mut Editor,
    compositor: &mut Compositor,
    target: JumpTarget,
    interrupt: Interrupt,
    then: impl FnOnce(&mut Editor) + Send + 'static,
) -> bool {
    let what = format!(
        "show {}",
        interrupt::location(&target.path, Some(target.line))
    );
    let moved = interrupt::jump(editor, compositor, interrupt, what, move |editor, _| {
        jump(editor, &target, Action::Replace)
            .map_err(|err| RpcError::not_found(format!("bb: {err}")))?;
        then(editor);
        Ok(())
    });
    match moved {
        Ok(moved) => moved.is_none(),
        Err(err) => {
            // a missing file is reported, not an error
            editor.set_warning(err.message);
            false
        }
    }
}

/// Move the focused view to `target` now: open the file, select the place,
/// centre it, remember where the user was in the jump list.
pub fn jump(editor: &mut Editor, target: &JumpTarget, action: Action) -> Result<(), JumpError> {
    if !exists(editor, &target.path) {
        return Err(JumpError::Gone(target.path.clone()));
    }
    let path = open_path(editor, &target.path);
    if matches!(action, Action::Replace) {
        // a pane that already shows the file takes the focus
        if let Some(doc) = editor.document_id_by_path(&path) {
            focus_view_of(editor, doc);
        }
    }
    {
        let (view, doc) = current!(editor);
        view.push_jump(doc);
    }
    let doc_id = editor
        .open(&path, action)
        .map_err(|err| JumpError::Open(target.path.clone(), err.to_string()))?;
    let (view, doc) = current!(editor);
    debug_assert_eq!(doc.id(), doc_id);
    let text = doc.text().slice(..);
    let start = char_from_pos(
        text,
        Position {
            line: target.line,
            col: target.col.unwrap_or(1),
        },
    );
    let selection = match target.end_line {
        Some(end_line) if end_line >= target.line => {
            Selection::single(start, char_after_line(text, end_line).max(start))
        }
        _ => Selection::point(start),
    };
    doc.set_selection(view.id, selection);
    align_view(doc, view, Align::Center);
    if let Some(status) = &target.status {
        editor.set_status(status.clone());
        STATE.lock().shown = Some(status.clone());
    }
    Ok(())
}

// --- methods -----------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemParams {
    path: String,
    line: usize,
    col: Option<usize>,
    end_line: Option<usize>,
    label: Option<String>,
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetParams {
    list: String,
    title: Option<String>,
    items: Vec<ItemParams>,
    #[serde(default = "yes")]
    activate: bool,
    goto: Option<usize>,
    interrupt: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClearParams {
    list: Option<String>,
}

fn yes() -> bool {
    true
}

pub fn set(cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: SetParams = parse_params(params)?;
        let interrupt = Interrupt::parse(params.interrupt.as_deref())?;
        if params.list.is_empty() {
            return Err(RpcError::invalid_params("list", "list must not be empty"));
        }
        if params.items.len() > MAX_ITEMS {
            return Err(RpcError::invalid_params(
                "items",
                format!("at most {MAX_ITEMS} items"),
            ));
        }
        if let Some(goto) = params.goto {
            if goto >= params.items.len() {
                return Err(RpcError::invalid_params(
                    "goto",
                    format!("goto {goto} is past the {} items", params.items.len()),
                ));
            }
        }
        let root = &cx.shared.info.workspace_root;
        let items: Vec<Item> = params
            .items
            .into_iter()
            .map(|i| Item {
                path: resolve_path(&i.path, root),
                line: i.line.max(1),
                col: i.col,
                end_line: i.end_line,
                label: i.label.filter(|s| !s.is_empty()),
                note: i.note.filter(|s| !s.is_empty()),
            })
            .collect();
        let count = items.len();
        let name = params.list;
        let goto_target = params.goto.map(|i| JumpTarget::from(&items[i]));
        replace_list(
            name.clone(),
            params.title.unwrap_or_else(|| name.clone()),
            items,
            params.activate,
            params.goto,
        );
        let mut deferred = false;
        if let (Some(index), Some(mut target)) = (params.goto, goto_target) {
            let status = STATE.lock().list(&name).map(|l| l.status(index));
            target.status = status;
            deferred = on_editor(move |editor, compositor| {
                request_jump(editor, compositor, target, interrupt, |_| {})
            })
            .await?;
        }
        Ok(json!({ "count": count, "deferred": deferred }))
    })
}

fn replace_list(
    name: String,
    title: String,
    items: Vec<Item>,
    activate: bool,
    current: Option<usize>,
) {
    let mut by_path: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        by_path.entry(item.path.clone()).or_default().push(i);
    }
    let list = List {
        name: name.clone(),
        title,
        items,
        by_path,
        current,
    };
    let mut state = STATE.lock();
    match state.lists.iter_mut().find(|l| l.name == name) {
        Some(slot) => *slot = list,
        None => state.lists.push(list),
    }
    if activate {
        state.active = Some(name);
    }
    state.refresh_watch();
}

pub fn clear(_cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: ClearParams = parse_params(params)?;
        let mut state = STATE.lock();
        match params.list {
            Some(name) => {
                state.lists.retain(|l| l.name != name);
                if state.active.as_deref() == Some(name.as_str()) {
                    state.active = None;
                }
            }
            None => {
                state.lists.clear();
                state.active = None;
            }
        }
        state.refresh_watch();
        Ok(json!({}))
    })
}

// --- commands ----------------------------------------------------------------

/// Step through the active list, skipping items whose file is gone.
fn walk(editor: &mut Editor, forward: bool) -> anyhow::Result<()> {
    let (name, len, start) = {
        let mut state = STATE.lock();
        let Some(list) = state.active_mut() else {
            bail!("No active bb locations list");
        };
        if list.items.is_empty() {
            bail!("bb locations list '{}' is empty", list.name);
        }
        (list.name.clone(), list.items.len(), list.current)
    };
    let mut index = start;
    let mut skipped = 0;
    let mut wrapped = false;
    for _ in 0..len {
        let next = match (index, forward) {
            (None, true) => 0,
            (None, false) => len - 1,
            (Some(i), true) => (i + 1) % len,
            (Some(i), false) => (i + len - 1) % len,
        };
        wrapped |= index.is_some() && (forward && next == 0 || !forward && next == len - 1);
        index = Some(next);
        let (mut target, status) = {
            let state = STATE.lock();
            let Some(list) = state.list(&name).filter(|l| l.items.len() == len) else {
                bail!("bb locations list '{name}' changed");
            };
            (JumpTarget::from(&list.items[next]), list.status(next))
        };
        let mut extra = Vec::new();
        if skipped > 0 {
            extra.push(format!(
                "skipped {skipped} missing file{}",
                if skipped == 1 { "" } else { "s" }
            ));
        }
        if wrapped {
            extra.push("wrapped".to_owned());
        }
        target.status = Some(if extra.is_empty() {
            status
        } else {
            format!("{status} ({})", extra.join(", "))
        });
        match jump(editor, &target, Action::Replace) {
            Ok(()) => {
                set_current(&name, next);
                return Ok(());
            }
            Err(JumpError::Gone(_)) => skipped += 1,
            Err(err) => bail!("{err}"),
        }
    }
    bail!("Every file in bb locations list '{name}' is gone")
}

fn set_current(name: &str, index: usize) {
    if let Some(list) = STATE.lock().list_mut(name) {
        list.current = Some(index);
    }
}

fn bb_next(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    walk(cx.editor, true)
}

fn bb_prev(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    walk(cx.editor, false)
}

/// One picker row.
struct Row {
    list: String,
    index: usize,
    item: Item,
    file: String,
}

fn bb_locations(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let (name, title, rows) = {
        let mut state = STATE.lock();
        let name = match args.first() {
            Some(name) => name.to_owned(),
            None => match &state.active {
                Some(name) => name.clone(),
                None => match state.lists.last() {
                    Some(list) => list.name.clone(),
                    None => bail!("No bb locations lists"),
                },
            },
        };
        let Some(list) = state.list(&name) else {
            let names: Vec<_> = state.lists.iter().map(|l| l.name.as_str()).collect();
            bail!(
                "No bb locations list '{name}' (lists: {})",
                if names.is_empty() {
                    "none".to_owned()
                } else {
                    names.join(", ")
                }
            );
        };
        let rows: Vec<Row> = list
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| Row {
                list: name.clone(),
                index,
                file: helix_stdx::path::get_relative_path(&item.path)
                    .to_string_lossy()
                    .into_owned(),
                item: item.clone(),
            })
            .collect();
        let title = list.title.clone();
        state.active = Some(name.clone());
        state.refresh_watch();
        (name, title, rows)
    };
    if rows.is_empty() {
        bail!("bb locations list '{name}' is empty");
    }
    cx.editor
        .set_status(format!("{title} ({} items)", rows.len()));
    cx.jobs.callback(async move {
        let call = move |_: &mut Editor, compositor: &mut Compositor| {
            compositor.push(Box::new(overlaid(picker(rows))));
        };
        Ok(Callback::EditorCompositor(Box::new(call)))
    });
    Ok(())
}

fn picker(rows: Vec<Row>) -> ui::Picker<Row, ()> {
    let columns = [
        ui::PickerColumn::new("file", |row: &Row, _| row.file.as_str().into()),
        ui::PickerColumn::new("line", |row: &Row, _| match row.item.end_line {
            Some(end) if end > row.item.line => format!("{}-{end}", row.item.line).into(),
            _ => row.item.line.to_string().into(),
        }),
        ui::PickerColumn::new("label", |row: &Row, _| {
            row.item.label.as_deref().unwrap_or_default().into()
        }),
        ui::PickerColumn::new("note", |row: &Row, _| {
            row.item.note.as_deref().unwrap_or_default().into()
        }),
    ];
    ui::Picker::new(columns, 0, rows, (), |cx, row: &Row, action| {
        let mut target = JumpTarget::from(&row.item);
        target.status = STATE
            .lock()
            .list(&row.list)
            .filter(|l| l.items.len() > row.index)
            .map(|l| l.status(row.index));
        match jump(cx.editor, &target, action) {
            Ok(()) => set_current(&row.list, row.index),
            Err(err) => cx.editor.set_warning(format!("bb: {err}")),
        }
    })
    .with_preview(|_editor, row: &Row| {
        let first = row.item.line.saturating_sub(1);
        let last = row.item.last_line().saturating_sub(1);
        Some((row.item.path.as_path().into(), Some((first, last))))
    })
}

// --- the note under the cursor -----------------------------------------------

pub fn register_hooks() {
    register_hook!(move |event: &mut PostCommand<'_, '_>| {
        if WATCH.load(Ordering::Relaxed) {
            show_note_here(event.cx.editor);
        }
        Ok(())
    });
}

/// While the cursor stands on an item of the active list that has a note,
/// keep the note on the status line (unless a command just put something
/// else there).
fn show_note_here(editor: &mut Editor) {
    let (view, doc) = current_ref!(editor);
    let Some(path) = doc.path() else {
        return;
    };
    let text = doc.text().slice(..);
    let line = text.char_to_line(doc.selection(view.id).primary().cursor(text)) + 1;
    let mut state = STATE.lock();
    let State {
        lists,
        active,
        shown,
    } = &mut *state;
    let Some(list) = active
        .as_deref()
        .and_then(|name| lists.iter_mut().find(|l| l.name == name))
    else {
        return;
    };
    let Some(index) = list.by_path.get(path).and_then(|indices| {
        indices.iter().copied().find(|&i| {
            let item = &list.items[i];
            item.note.is_some() && (item.line..=item.last_line()).contains(&line)
        })
    }) else {
        return;
    };
    let ours = match editor.get_status() {
        None => true,
        Some((msg, _)) => shown.as_deref() == Some(msg.as_ref()),
    };
    if !ours {
        return;
    }
    list.current = Some(index);
    let status = list.status(index);
    if shown.as_deref() != Some(status.as_str()) || editor.get_status().is_none() {
        *shown = Some(status.clone());
        editor.set_status(status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, line: usize, note: Option<&str>) -> Item {
        Item {
            path: PathBuf::from(path),
            line,
            col: None,
            end_line: None,
            label: None,
            note: note.map(str::to_owned),
        }
    }

    #[test]
    fn status_text() {
        let mut items = vec![
            item("/r/a.go", 3, Some("entry point")),
            item("/r/b.go", 9, None),
        ];
        items[1].label = Some("handler".into());
        let list = List {
            name: "tour".into(),
            title: "Tour".into(),
            by_path: HashMap::new(),
            items,
            current: None,
        };
        assert_eq!(list.status(0), "tour 1/2: entry point");
        assert_eq!(list.status(1), "tour 2/2: handler");
        assert!(list.has_notes());
    }
}
