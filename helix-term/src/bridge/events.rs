//! Editor hooks, subscription flags and the event coalescer.
//!
//! Every hook starts with one relaxed load of `wants` and returns when no
//! client listens to its kind. When someone does, the hook copies a small
//! snapshot (ids, a `Selection`, an O(1) `Rope` clone) into an unbounded
//! channel; the event task does the coalescing, conversion and JSON.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use helix_core::{Rope, Selection};
use helix_event::{register_hook, runtime_local};
use helix_view::document::Mode;
use helix_view::events::{
    DocumentDidChange, DocumentDidClose, DocumentDidOpen, DocumentFocusLost, SelectionDidChange,
};
use helix_view::{Document, DocumentId, Editor, ViewId};
use parking_lot::Mutex;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;

use super::convert::{doc_num, path_string, selection_to_proto, selection_with_text};
use super::dispatch::on_editor;
use super::protocol::{
    self, DocumentChangedEvent, DocumentEvent, DocumentSavedEvent, FocusEvent, ModeEvent,
    SelectionEvent, SendEvent, SendKind, MAX_TEXT,
};
use super::protocol::{parse_params, RpcError, RpcResult, SubscribeParams};
use super::server::Ctx;
use super::{methods, Shared};
use crate::events::{OnModeSwitch, PostCommand};
use futures_util::future::BoxFuture;

pub const SELECTION_WINDOW: Duration = Duration::from_millis(50);
pub const CHANGED_WINDOW: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Selection,
    Documents,
    Diagnostics,
    Mode,
    Send,
    Lsp,
    ExternalChange,
}

impl Kind {
    pub const COUNT: usize = 7;
    pub const ALL: [Kind; Self::COUNT] = [
        Kind::Selection,
        Kind::Documents,
        Kind::Diagnostics,
        Kind::Mode,
        Kind::Send,
        Kind::Lsp,
        Kind::ExternalChange,
    ];

    pub const fn bit(self) -> u32 {
        1 << self as u32
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Selection => "selection",
            Kind::Documents => "documents",
            Kind::Diagnostics => "diagnostics",
            Kind::Mode => "mode",
            Kind::Send => "send",
            Kind::Lsp => "lsp",
            Kind::ExternalChange => "externalChange",
        }
    }

    pub fn parse(name: &str) -> Option<Kind> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }

    pub fn names(mask: u32) -> Vec<&'static str> {
        Self::ALL
            .into_iter()
            .filter(|k| mask & k.bit() != 0)
            .map(Kind::name)
            .collect()
    }
}

const SELECTION: u32 = Kind::Selection.bit();
const DOCUMENTS: u32 = Kind::Documents.bit();
const MODE: u32 = Kind::Mode.bit();

/// What the hooks need on the key-press path.
struct Hot {
    /// Bit per `Kind`: is any client subscribed?
    wants: AtomicU32,
    tx: OnceLock<UnboundedSender<Msg>>,
    /// Focus and mode as last reported, to notice changes.
    seen: Mutex<Seen>,
}

#[derive(Default)]
struct Seen {
    view: Option<ViewId>,
    doc: Option<DocumentId>,
    mode: Option<Mode>,
    /// The last `documentChanged` sent: document, version, modified.
    changed: Option<(DocumentId, i32, bool)>,
}

runtime_local! {
    static HOT: Hot = Hot {
        wants: AtomicU32::new(0),
        tx: OnceLock::new(),
        seen: Mutex::new(Seen { view: None, doc: None, mode: None, changed: None }),
    };
}

#[inline(always)]
pub fn wants(mask: u32) -> bool {
    HOT.wants.load(Ordering::Relaxed) & mask != 0
}

pub fn wants_kind(kind: Kind) -> bool {
    wants(kind.bit())
}

pub fn set_wants(mask: u32) {
    HOT.wants.store(mask, Ordering::Relaxed);
}

pub(super) fn send(msg: Msg) {
    if let Some(tx) = HOT.tx.get() {
        let _ = tx.send(msg);
    }
}

/// Selection of one view, as the hook saw it.
pub struct SelSnap {
    pub view: ViewId,
    pub doc: DocumentId,
    pub path: Option<PathBuf>,
    pub version: i32,
    pub text: Rope,
    pub selection: Selection,
}

impl SelSnap {
    pub fn of(doc: &Document, view: ViewId) -> Option<SelSnap> {
        let selection = doc.selections().get(&view)?.clone();
        Some(SelSnap {
            view,
            doc: doc.id(),
            path: doc.path().map(Path::to_path_buf),
            version: doc.version(),
            text: doc.text().clone(),
            selection,
        })
    }
}

pub struct DocMeta {
    pub doc: DocumentId,
    pub path: Option<PathBuf>,
    pub language: Option<String>,
    pub version: i32,
    pub modified: bool,
}

impl DocMeta {
    pub fn of(doc: &Document) -> DocMeta {
        DocMeta {
            doc: doc.id(),
            path: doc.path().map(Path::to_path_buf),
            language: doc.language_name().map(str::to_owned),
            version: doc.version(),
            modified: doc.is_modified(),
        }
    }
}

/// Everything `event/send` carries, captured on the main thread.
pub struct SendSnap {
    pub kind: SendKind,
    pub note: String,
    pub send_now: bool,
    pub meta: DocMeta,
    pub text: Rope,
    /// `None` for `flush`, which carries no ranges.
    pub selection: Option<Selection>,
}

pub enum Msg {
    Selection(SelSnap),
    Focus {
        view: ViewId,
        doc: DocumentId,
        path: Option<PathBuf>,
        /// Emit `event/focus` (false when only seeding the tracker).
        announce: bool,
        sel: Option<SelSnap>,
    },
    Changed(DocMeta),
    Opened(DocMeta),
    Closed(DocMeta),
    Saved(DocMeta),
    Mode(Mode),
    Send(SendSnap),
}

// --- hooks -------------------------------------------------------------------

pub fn install(editor: &Editor, shared: &Arc<Shared>) {
    let (tx, rx) = unbounded_channel();
    if HOT.tx.set(tx).is_err() {
        return;
    }
    {
        let mut seen = HOT.seen.lock();
        seen.view = Some(editor.tree.focus);
        seen.doc = editor.tree.try_get(editor.tree.focus).map(|v| v.doc);
        seen.mode = Some(editor.mode());
    }
    let task = tokio::spawn(run(rx, shared.clone()));
    shared.track(task.abort_handle());

    register_hook!(move |event: &mut SelectionDidChange<'_>| {
        if wants(SELECTION) {
            if let Some(snap) = SelSnap::of(event.doc, event.view) {
                send(Msg::Selection(snap));
            }
        }
        Ok(())
    });
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if wants(SELECTION | DOCUMENTS) {
            if wants(SELECTION) {
                if let Some(snap) = SelSnap::of(event.doc, event.view) {
                    send(Msg::Selection(snap));
                }
            }
            if wants(DOCUMENTS) {
                // The change is composed into the document only after this
                // hook, so `is_modified` may still say false. A change leaves
                // the buffer unmodified only when it returns to the saved
                // state (undo, redo, reload); `check_editor` corrects those.
                let doc = &*event.doc;
                let meta = DocMeta {
                    doc: doc.id(),
                    path: doc.path().map(Path::to_path_buf),
                    language: None,
                    version: doc.version(),
                    modified: true,
                };
                HOT.seen.lock().changed = Some((meta.doc, meta.version, true));
                send(Msg::Changed(meta));
            }
        }
        Ok(())
    });
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        if wants(SELECTION | DOCUMENTS | MODE) {
            if wants(DOCUMENTS) {
                if let Some(doc) = event.editor.document(event.doc) {
                    send(Msg::Opened(DocMeta::of(doc)));
                }
            }
            check_editor(event.editor);
        }
        Ok(())
    });
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        if wants(SELECTION | DOCUMENTS | MODE) {
            if wants(DOCUMENTS) {
                send(Msg::Closed(DocMeta::of(&event.doc)));
            }
            check_editor(event.editor);
        }
        Ok(())
    });
    register_hook!(move |event: &mut DocumentFocusLost<'_>| {
        if wants(SELECTION | DOCUMENTS | MODE) {
            check_editor(event.editor);
        }
        Ok(())
    });
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        if wants(SELECTION | DOCUMENTS | MODE) {
            check_editor(event.cx.editor);
        }
        Ok(())
    });
    register_hook!(move |event: &mut PostCommand<'_, '_>| {
        if wants(SELECTION | DOCUMENTS | MODE) {
            check_editor(event.cx.editor);
        }
        Ok(())
    });
    methods::register_hooks();
}

/// Called from hooks that can see the whole editor: report mode and focus
/// changes since the last look.
fn check_editor(editor: &Editor) {
    let mut seen = HOT.seen.lock();
    if wants(MODE) && seen.mode != Some(editor.mode()) {
        seen.mode = Some(editor.mode());
        send(Msg::Mode(editor.mode()));
    }
    if !wants(SELECTION | DOCUMENTS) {
        return;
    }
    let Some(view) = editor.tree.try_get(editor.tree.focus) else {
        return;
    };
    if let Some((doc, version, modified)) = seen.changed.filter(|c| c.0 == view.doc) {
        match editor.document(doc) {
            Some(d) if d.version() == version && d.is_modified() != modified => {
                seen.changed = Some((doc, version, !modified));
                send(Msg::Changed(DocMeta::of(d)));
            }
            _ => {}
        }
    }
    if seen.view == Some(view.id) && seen.doc == Some(view.doc) {
        return;
    }
    let announce = seen.doc != Some(view.doc);
    seen.view = Some(view.id);
    seen.doc = Some(view.doc);
    drop(seen);
    let Some(doc) = editor.document(view.doc) else {
        return;
    };
    send(Msg::Focus {
        view: view.id,
        doc: view.doc,
        path: doc.path().map(Path::to_path_buf),
        announce,
        sel: wants(SELECTION)
            .then(|| SelSnap::of(doc, view.id))
            .flatten(),
    });
}

/// Run when a client subscribes: the tracker may be stale after a period
/// with nobody listening.
pub fn seed(editor: &Editor) {
    let mut seen = HOT.seen.lock();
    seen.mode = Some(editor.mode());
    let Some(view) = editor.tree.try_get(editor.tree.focus) else {
        return;
    };
    seen.view = Some(view.id);
    seen.doc = Some(view.doc);
    drop(seen);
    send(Msg::Focus {
        view: view.id,
        doc: view.doc,
        path: editor
            .document(view.doc)
            .and_then(|d| d.path().map(Path::to_path_buf)),
        announce: false,
        sel: None,
    });
}

pub fn document_saved(editor: &Editor, doc: DocumentId) {
    if wants(DOCUMENTS) {
        if let Some(doc) = editor.document(doc) {
            let meta = DocMeta::of(doc);
            HOT.seen.lock().changed = Some((meta.doc, meta.version, meta.modified));
            send(Msg::Saved(meta));
        }
    }
}

pub fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "normal",
        Mode::Insert => "insert",
        Mode::Select => "select",
    }
}

// --- events/subscribe, events/unsubscribe ------------------------------------

pub fn subscribe(cx: Ctx, params: serde_json::Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(change_subscriptions(cx, params, true))
}

pub fn unsubscribe(cx: Ctx, params: serde_json::Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(change_subscriptions(cx, params, false))
}

async fn change_subscriptions(cx: Ctx, params: serde_json::Value, add: bool) -> RpcResult {
    let params: SubscribeParams = parse_params(params)?;
    let mut mask = 0;
    for name in &params.events {
        match Kind::parse(name) {
            Some(kind) => mask |= kind.bit(),
            None => {
                return Err(RpcError::invalid_params(
                    "events",
                    format!("unknown event kind: {name}"),
                ))
            }
        }
    }
    if add && mask & (SELECTION | DOCUMENTS | MODE) != 0 {
        // Start the focus/mode tracker from the current state before the
        // hooks begin to report differences.
        on_editor(|editor, _| seed(editor)).await?;
    }
    let after =
        cx.client.set_subscriptions(
            &cx.shared,
            |before| if add { before | mask } else { before & !mask },
        );
    Ok(serde_json::json!({ "subscribed": Kind::names(after) }))
}

// --- event task --------------------------------------------------------------

/// Where the coalescer's output goes: the clients, or a test buffer.
pub trait Sink {
    fn emit(&self, kind: Kind, method: &str, params: serde_json::Value, coalescable: bool);
}

impl Sink for Arc<Shared> {
    fn emit(&self, kind: Kind, method: &str, params: serde_json::Value, coalescable: bool) {
        broadcast(self, kind, method, params, coalescable);
    }
}

struct Coalescer<S: Sink> {
    sink: S,
    focus: Option<ViewId>,
    /// Latest selection per view since the last flush.
    selections: Vec<SelSnap>,
    selection_due: Option<Instant>,
    selection_sent: Option<Instant>,
    last_selection: Option<serde_json::Value>,
    changes: HashMap<DocumentId, DocMeta>,
    changes_due: Option<Instant>,
    changes_sent: Option<Instant>,
}

/// Throttle with a leading edge: emit now if nothing went out in the last
/// `window`, otherwise once the window since the last emission ends. A lone
/// change therefore waits for no timer, and a burst is at most one event per
/// window, carrying the final state.
fn schedule(due: &mut Option<Instant>, sent: Option<Instant>, window: Duration) -> bool {
    if due.is_some() {
        return false;
    }
    match sent {
        Some(sent) if sent.elapsed() < window => {
            *due = Some(sent + window);
            false
        }
        _ => true,
    }
}

async fn run(mut rx: UnboundedReceiver<Msg>, shared: Arc<Shared>) {
    let mut c = Coalescer {
        sink: shared,
        focus: None,
        selections: Vec::new(),
        selection_due: None,
        selection_sent: None,
        last_selection: None,
        changes: HashMap::new(),
        changes_due: None,
        changes_sent: None,
    };
    loop {
        let sel_due = c.selection_due;
        let changes_due = c.changes_due;
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(msg) => c.handle(msg),
                None => break,
            },
            _ = sleep_until(sel_due), if sel_due.is_some() => {
                c.flush_selection();
                if let Some(due) = sel_due {
                    log::debug!("bridge: selection flushed {:?} after its window", due.elapsed());
                }
            }
            _ = sleep_until(changes_due), if changes_due.is_some() => c.flush_changes(),
        }
    }
}

async fn sleep_until(due: Option<Instant>) {
    if let Some(due) = due {
        tokio::time::sleep_until(due).await
    }
}

impl<S: Sink> Coalescer<S> {
    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Selection(snap) => self.queue_selection(snap),
            Msg::Focus {
                view,
                doc,
                path,
                announce,
                sel,
            } => {
                self.focus = Some(view);
                if announce {
                    self.emit(
                        Kind::Documents,
                        "event/focus",
                        FocusEvent {
                            doc_id: doc_num(doc),
                            path: path_string(path.as_deref()),
                        },
                        false,
                    );
                }
                if let Some(sel) = sel {
                    self.queue_selection(sel);
                }
            }
            Msg::Changed(meta) => {
                self.changes.insert(meta.doc, meta);
                if schedule(&mut self.changes_due, self.changes_sent, CHANGED_WINDOW) {
                    self.flush_changes();
                }
            }
            Msg::Opened(meta) => {
                self.emit(
                    Kind::Documents,
                    "event/documentOpened",
                    document_event(&meta),
                    false,
                );
            }
            Msg::Closed(meta) => {
                self.flush_change_of(meta.doc);
                self.emit(
                    Kind::Documents,
                    "event/documentClosed",
                    document_event(&meta),
                    false,
                );
            }
            Msg::Saved(meta) => {
                self.flush_change_of(meta.doc);
                self.emit(
                    Kind::Documents,
                    "event/documentSaved",
                    DocumentSavedEvent {
                        doc_id: doc_num(meta.doc),
                        path: path_string(meta.path.as_deref()),
                        version: meta.version,
                    },
                    false,
                );
            }
            Msg::Mode(mode) => self.emit(
                Kind::Mode,
                "event/mode",
                ModeEvent {
                    mode: mode_name(mode),
                },
                false,
            ),
            Msg::Send(snap) => {
                let event = send_event(snap);
                self.emit(Kind::Send, "event/send", event, false);
            }
        }
    }

    fn queue_selection(&mut self, snap: SelSnap) {
        match self.selections.iter_mut().find(|s| s.view == snap.view) {
            Some(slot) => *slot = snap,
            None => self.selections.push(snap),
        }
        if schedule(
            &mut self.selection_due,
            self.selection_sent,
            SELECTION_WINDOW,
        ) {
            self.flush_selection();
        }
    }

    fn flush_selection(&mut self) {
        self.selection_due = None;
        let mut selections = std::mem::take(&mut self.selections);
        let snap = match self.focus {
            Some(focus) => match selections.iter().position(|s| s.view == focus) {
                Some(i) => selections.swap_remove(i),
                None => return,
            },
            None => match selections.pop() {
                Some(snap) => snap,
                None => return,
            },
        };
        let text = snap.text.slice(..);
        let event = SelectionEvent {
            path: path_string(snap.path.as_deref()),
            doc_id: doc_num(snap.doc),
            version: snap.version,
            primary: snap.selection.primary_index(),
            ranges: selection_to_proto(text, &snap.selection),
        };
        let value = serde_json::to_value(&event).unwrap_or_default();
        if self.last_selection.as_ref() == Some(&value) {
            return;
        }
        self.last_selection = Some(value.clone());
        self.selection_sent = Some(Instant::now());
        self.sink
            .emit(Kind::Selection, "event/selection", value, true);
    }

    fn flush_changes(&mut self) {
        self.changes_due = None;
        let mut changes: Vec<DocMeta> = self.changes.drain().map(|(_, m)| m).collect();
        changes.sort_by_key(|m| doc_num(m.doc));
        for meta in changes {
            self.emit_change(meta);
        }
    }

    /// Changes go out before a save or close of the same document, even
    /// inside the window, so a client never sees them in the wrong order.

    fn flush_change_of(&mut self, doc: DocumentId) {
        if let Some(meta) = self.changes.remove(&doc) {
            self.emit_change(meta);
        }
        if self.changes.is_empty() {
            self.changes_due = None;
        }
    }

    fn emit_change(&mut self, meta: DocMeta) {
        self.changes_sent = Some(Instant::now());
        self.emit(
            Kind::Documents,
            "event/documentChanged",
            DocumentChangedEvent {
                doc_id: doc_num(meta.doc),
                path: path_string(meta.path.as_deref()),
                version: meta.version,
                modified: meta.modified,
            },
            true,
        );
    }

    fn emit(&self, kind: Kind, method: &str, params: impl serde::Serialize, coalescable: bool) {
        let params = serde_json::to_value(params).unwrap_or_default();
        self.sink.emit(kind, method, params, coalescable);
    }
}

/// Serialize once and queue for every client subscribed to `kind`.
pub(super) fn broadcast(
    shared: &Shared,
    kind: Kind,
    method: &str,
    params: impl serde::Serialize,
    coalescable: bool,
) {
    let clients: Vec<_> = shared
        .clients
        .lock()
        .iter()
        .filter(|c| c.subscriptions() & kind.bit() != 0)
        .cloned()
        .collect();
    if clients.is_empty() {
        return;
    }
    let line = super::server::encode(&protocol::notification(method, params));
    for client in clients {
        client.push(line.clone(), coalescable);
    }
}

fn document_event(meta: &DocMeta) -> DocumentEvent {
    DocumentEvent {
        doc_id: doc_num(meta.doc),
        path: path_string(meta.path.as_deref()),
        language_id: meta.language.clone(),
    }
}

pub fn send_event(snap: SendSnap) -> SendEvent {
    let (ranges, truncated) = match &snap.selection {
        Some(selection) => selection_with_text(snap.text.slice(..), selection, MAX_TEXT),
        None => (Vec::new(), false),
    };
    SendEvent {
        kind: snap.kind,
        note: snap.note,
        path: path_string(snap.meta.path.as_deref()),
        doc_id: doc_num(snap.meta.doc),
        language_id: snap.meta.language,
        version: snap.meta.version,
        modified: snap.meta.modified,
        ranges,
        truncated,
        send_now: snap.send_now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slotmap::KeyData;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Buf(RefCell<Vec<(Kind, String, serde_json::Value, bool)>>);

    impl Sink for &Buf {
        fn emit(&self, kind: Kind, method: &str, params: serde_json::Value, coalescable: bool) {
            self.0
                .borrow_mut()
                .push((kind, method.to_owned(), params, coalescable));
        }
    }

    impl Buf {
        fn methods(&self) -> Vec<String> {
            self.0.borrow().iter().map(|e| e.1.clone()).collect()
        }
    }

    fn coalescer(buf: &Buf) -> Coalescer<&Buf> {
        Coalescer {
            sink: buf,
            focus: None,
            selections: Vec::new(),
            selection_due: None,
            selection_sent: None,
            last_selection: None,
            changes: HashMap::new(),
            changes_due: None,
            changes_sent: None,
        }
    }

    fn view(n: u64) -> ViewId {
        ViewId::from(KeyData::from_ffi((1 << 32) | n))
    }

    fn snap(view: ViewId, version: i32, head: usize) -> SelSnap {
        SelSnap {
            view,
            doc: DocumentId::default(),
            path: Some(PathBuf::from("/r/a.go")),
            version,
            text: Rope::from("hello\nworld\n"),
            selection: Selection::single(head, head + 1),
        }
    }

    fn meta(version: i32) -> DocMeta {
        DocMeta {
            doc: DocumentId::default(),
            path: Some(PathBuf::from("/r/a.go")),
            language: Some("go".into()),
            version,
            modified: true,
        }
    }

    #[tokio::test]
    async fn selection_leading_edge_then_latest_for_the_focused_view() {
        let buf = Buf::default();
        let mut c = coalescer(&buf);
        c.handle(Msg::Focus {
            view: view(1),
            doc: DocumentId::default(),
            path: None,
            announce: false,
            sel: None,
        });
        // the first change after a quiet period goes out at once
        c.handle(Msg::Selection(snap(view(1), 1, 0)));
        assert_eq!(buf.methods(), ["event/selection"]);
        assert!(c.selection_due.is_none());
        // the rest of the burst waits for the window, latest wins
        c.handle(Msg::Selection(snap(view(2), 1, 3)));
        c.handle(Msg::Selection(snap(view(1), 2, 7)));
        assert!(c.selection_due.is_some());
        assert_eq!(buf.0.borrow().len(), 1);
        c.flush_selection();
        let events = buf.0.borrow();
        assert_eq!(events.len(), 2);
        let (kind, method, params, coalescable) = &events[1];
        assert_eq!(*kind, Kind::Selection);
        assert_eq!(method, "event/selection");
        assert!(coalescable);
        assert_eq!(params["version"], 2);
        assert_eq!(params["ranges"][0]["start"]["line"], 2);
        assert_eq!(params["ranges"][0]["start"]["col"], 2);
        assert!(c.selections.is_empty());
    }

    #[tokio::test]
    async fn quiet_period_resets_the_throttle() {
        let buf = Buf::default();
        let mut c = coalescer(&buf);
        c.handle(Msg::Selection(snap(view(1), 1, 0)));
        c.selection_sent = Some(Instant::now() - SELECTION_WINDOW * 2);
        c.handle(Msg::Selection(snap(view(1), 1, 2)));
        assert_eq!(buf.methods(), ["event/selection", "event/selection"]);
        assert!(c.selection_due.is_none());
    }

    #[tokio::test]
    async fn unchanged_selection_is_not_repeated() {
        let buf = Buf::default();
        let mut c = coalescer(&buf);
        c.handle(Msg::Selection(snap(view(1), 1, 0)));
        c.handle(Msg::Selection(snap(view(1), 1, 0)));
        c.flush_selection();
        c.handle(Msg::Selection(snap(view(1), 1, 2)));
        c.flush_selection();
        assert_eq!(buf.methods(), ["event/selection", "event/selection"]);
    }

    #[tokio::test]
    async fn other_views_do_not_emit() {
        let buf = Buf::default();
        let mut c = coalescer(&buf);
        c.focus = Some(view(1));
        c.handle(Msg::Selection(snap(view(2), 1, 0)));
        c.flush_selection();
        assert!(buf.0.borrow().is_empty());
    }

    #[tokio::test]
    async fn changes_coalesce_and_flush_before_save_and_close() {
        let buf = Buf::default();
        let mut c = coalescer(&buf);
        c.handle(Msg::Changed(meta(1)));
        c.handle(Msg::Changed(meta(2)));
        c.handle(Msg::Changed(meta(3)));
        assert_eq!(buf.methods(), ["event/documentChanged"]);
        assert!(c.changes_due.is_some());
        c.handle(Msg::Saved(meta(3)));
        assert_eq!(
            buf.methods(),
            [
                "event/documentChanged",
                "event/documentChanged",
                "event/documentSaved"
            ]
        );
        assert_eq!(buf.0.borrow()[1].2["version"], 3);
        assert!(c.changes_due.is_none());

        c.handle(Msg::Changed(meta(4)));
        assert_eq!(buf.0.borrow().len(), 3, "inside the window");
        c.handle(Msg::Closed(meta(4)));
        c.flush_changes();
        assert_eq!(
            &buf.methods()[3..],
            ["event/documentChanged", "event/documentClosed"]
        );
    }

    #[tokio::test]
    async fn focus_announces_and_sends_selection() {
        let buf = Buf::default();
        let mut c = coalescer(&buf);
        c.handle(Msg::Focus {
            view: view(3),
            doc: DocumentId::default(),
            path: Some(PathBuf::from("/r/b.rs")),
            announce: true,
            sel: Some(snap(view(3), 5, 0)),
        });
        assert_eq!(buf.methods(), ["event/focus", "event/selection"]);
        assert_eq!(buf.0.borrow()[0].2["path"], "/r/b.rs");
        assert_eq!(buf.0.borrow()[0].2["docId"], 1);
    }

    #[test]
    fn kinds_round_trip() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.name()), Some(kind));
        }
        assert_eq!(Kind::parse("nope"), None);
        assert_eq!(
            Kind::names(Kind::Mode.bit() | Kind::Send.bit()),
            ["mode", "send"]
        );
    }

    #[test]
    fn send_event_without_ranges_for_flush() {
        let ev = send_event(SendSnap {
            kind: SendKind::Flush,
            note: String::new(),
            send_now: true,
            meta: meta(1),
            text: Rope::from("x"),
            selection: None,
        });
        assert!(ev.ranges.is_empty());
        assert_eq!(ev.kind, SendKind::Flush);
        let ev = send_event(SendSnap {
            kind: SendKind::Send,
            note: "look".into(),
            send_now: false,
            meta: meta(1),
            text: Rope::from("ab\ncd\n"),
            selection: Some(Selection::single(0, 3)),
        });
        assert_eq!(ev.ranges[0].text, "ab\n");
        assert_eq!(ev.note, "look");
    }
}
