//! diagnostics/get and `event/diagnostics`.
//!
//! The main thread only copies the diagnostics in scope (with an O(1) clone
//! of each open buffer's text). Filtering, counting, sorting, position
//! conversion and JSON happen in a blocking task.
//!
//! For an open buffer the diagnostics are the ones Helix shows (already
//! mapped through unsaved edits, in chars). For files that are not open they
//! are what the server published, converted with the server's offset
//! encoding against the file on disk.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use helix_core::diagnostic::{Diagnostic, DiagnosticProvider, Severity as HxSeverity};
use helix_core::{Rope, Uri};
use helix_event::{register_hook, runtime_local};
use helix_lsp::{lsp, LanguageServerId, OffsetEncoding};
use helix_view::events::{DiagnosticsDidChange, LanguageServerExited};
use helix_view::Editor;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;

use super::lsp::{range_from_lsp, Texts};
use crate::bridge::convert::{open_path, pos_from_char, resolve_path};
use crate::bridge::dispatch::on_editor;
use crate::bridge::events::{broadcast, wants_kind, Kind};
use crate::bridge::protocol::{parse_params, ErrorCode, Position, Range, RpcError, RpcResult};
use crate::bridge::server::Ctx;
use crate::bridge::Shared;

pub const CAPABILITY: Option<&str> = Some("diagnostics");

/// `event/diagnostics`: at most one per path in this window.
pub const WINDOW: Duration = Duration::from_millis(200);

const MAX_ITEMS: usize = 500;
const DEFAULT_LIMIT: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Severity {
    Hint,
    Info,
    Warning,
    Error,
}

impl From<Option<HxSeverity>> for Severity {
    /// A diagnostic without a severity counts as an error, as most clients
    /// show it.
    fn from(s: Option<HxSeverity>) -> Self {
        match s {
            Some(HxSeverity::Hint) => Severity::Hint,
            Some(HxSeverity::Info) => Severity::Info,
            Some(HxSeverity::Warning) => Severity::Warning,
            Some(HxSeverity::Error) | None => Severity::Error,
        }
    }
}

impl From<Option<lsp::DiagnosticSeverity>> for Severity {
    fn from(s: Option<lsp::DiagnosticSeverity>) -> Self {
        match s {
            Some(lsp::DiagnosticSeverity::HINT) => Severity::Hint,
            Some(lsp::DiagnosticSeverity::INFORMATION) => Severity::Info,
            Some(lsp::DiagnosticSeverity::WARNING) => Severity::Warning,
            _ => Severity::Error,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Counts {
    errors: usize,
    warnings: usize,
    infos: usize,
    hints: usize,
}

impl Counts {
    fn add(&mut self, s: Severity) {
        match s {
            Severity::Error => self.errors += 1,
            Severity::Warning => self.warnings += 1,
            Severity::Info => self.infos += 1,
            Severity::Hint => self.hints += 1,
        }
    }

    fn is_empty(&self) -> bool {
        *self == Counts::default()
    }
}

// --- copying on the main thread ------------------------------------------------

/// Where a diagnostic is, as Helix holds it.
#[derive(Debug, Clone, Copy)]
enum At {
    /// Char offsets into the open buffer's text.
    Chars(usize, usize),
    /// The server's range, in its encoding (the file is not open).
    Lsp(lsp::Range, OffsetEncoding),
}

impl At {
    fn sort_key(&self) -> (u64, u64) {
        match self {
            At::Chars(start, _) => (*start as u64, 0),
            At::Lsp(range, _) => (range.start.line as u64, range.start.character as u64),
        }
    }
}

#[derive(Debug)]
struct Raw {
    /// Index into `Snapshot::files`, which is sorted by path.
    file: usize,
    severity: Severity,
    at: At,
    message: String,
    source: Option<String>,
    code: Option<Value>,
    server: Option<LanguageServerId>,
}

#[derive(Debug)]
struct File {
    path: PathBuf,
    /// The open buffer's text; `None` when the file is not open.
    text: Option<Rope>,
}

#[derive(Debug, Default)]
struct Snapshot {
    files: Vec<File>,
    items: Vec<Raw>,
    servers: HashMap<LanguageServerId, String>,
}

fn server_of(provider: &DiagnosticProvider) -> Option<LanguageServerId> {
    provider.language_server_id()
}

fn hx_code(code: &helix_core::diagnostic::NumberOrString) -> Value {
    match code {
        helix_core::diagnostic::NumberOrString::Number(n) => (*n).into(),
        helix_core::diagnostic::NumberOrString::String(s) => s.clone().into(),
    }
}

fn lsp_code(code: &lsp::NumberOrString) -> Value {
    match code {
        lsp::NumberOrString::Number(n) => (*n).into(),
        lsp::NumberOrString::String(s) => s.clone().into(),
    }
}

/// The diagnostics of each path in scope: `Doc` for open buffers, `Lsp`
/// for what servers published about files that are not open.
enum Source<'a> {
    Doc(&'a helix_view::Document),
    Lsp(&'a [(lsp::Diagnostic, DiagnosticProvider)]),
}

/// Every path in scope with its source, sorted by path.
fn sources<'a>(editor: &'a Editor, scope: Option<&Path>) -> Vec<(&'a Path, Source<'a>)> {
    let mut out: Vec<(&Path, Source)> = Vec::new();
    match scope {
        Some(path) => {
            if let Some(doc) = editor
                .document_id_by_path(&open_path(editor, path))
                .and_then(|id| editor.document(id))
            {
                if let Some(p) = doc.path() {
                    out.push((p, Source::Doc(doc)));
                }
            } else if let Some((uri, diags)) = editor.diagnostics.get_key_value(&Uri::from(path)) {
                if let Some(p) = uri.as_path() {
                    out.push((p, Source::Lsp(diags)));
                }
            }
        }
        None => {
            let mut open = HashSet::new();
            for doc in editor.documents() {
                if let Some(p) = doc.path() {
                    open.insert(p);
                    out.push((p, Source::Doc(doc)));
                }
            }
            for (uri, diags) in &editor.diagnostics {
                if let Some(p) = uri.as_path().filter(|p| !open.contains(p)) {
                    out.push((p, Source::Lsp(diags)));
                }
            }
            out.sort_by(|a, b| a.0.cmp(b.0));
        }
    }
    out
}

fn encoding(editor: &Editor, id: Option<LanguageServerId>) -> OffsetEncoding {
    id.and_then(|id| editor.language_servers.get_by_id(id))
        .filter(|c| c.is_initialized())
        .map(|c| c.offset_encoding())
        .unwrap_or_default()
}

/// Main thread: copy the diagnostics in scope.
fn snapshot(editor: &Editor, scope: Option<&Path>) -> Snapshot {
    let mut snap = Snapshot::default();
    let mut encodings: HashMap<Option<LanguageServerId>, OffsetEncoding> = HashMap::new();
    for (path, source) in sources(editor, scope) {
        let file = snap.files.len();
        match source {
            Source::Doc(doc) => {
                if doc.diagnostics().is_empty() {
                    continue;
                }
                snap.files.push(File {
                    path: path.to_path_buf(),
                    text: Some(doc.text().clone()),
                });
                snap.items
                    .extend(doc.diagnostics().iter().map(|d: &Diagnostic| Raw {
                        file,
                        severity: d.severity.into(),
                        at: At::Chars(d.range.start, d.range.end),
                        message: d.message.clone(),
                        source: d.source.clone(),
                        code: d.code.as_ref().map(hx_code),
                        server: server_of(&d.provider),
                    }));
            }
            Source::Lsp(diags) => {
                if diags.is_empty() {
                    continue;
                }
                snap.files.push(File {
                    path: path.to_path_buf(),
                    text: None,
                });
                snap.items.extend(diags.iter().map(|(d, provider)| {
                    let server = server_of(provider);
                    let enc = *encodings
                        .entry(server)
                        .or_insert_with(|| encoding(editor, server));
                    Raw {
                        file,
                        severity: d.severity.into(),
                        at: At::Lsp(d.range, enc),
                        message: d.message.clone(),
                        source: d.source.clone(),
                        code: d.code.as_ref().map(lsp_code),
                        server,
                    }
                }));
            }
        }
    }
    let ids: HashSet<_> = snap.items.iter().filter_map(|d| d.server).collect();
    snap.servers = ids
        .into_iter()
        .filter_map(|id| Some((id, editor.language_servers.get_by_id(id)?.name().to_owned())))
        .collect();
    snap
}

/// Main thread: severity counts of one path, without copying anything.
fn counts_of(editor: &Editor, path: &Path, open: &HashMap<&Path, &helix_view::Document>) -> Counts {
    let mut counts = Counts::default();
    match open.get(path) {
        Some(doc) => doc
            .diagnostics()
            .iter()
            .for_each(|d| counts.add(d.severity.into())),
        None => {
            if let Some(diags) = editor.diagnostics.get(&Uri::from(path)) {
                diags
                    .iter()
                    .for_each(|(d, _)| counts.add(d.severity.into()));
            }
        }
    }
    counts
}

// --- diagnostics/get -------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetParams {
    path: Option<String>,
    min_severity: Option<Severity>,
    limit: Option<usize>,
    #[serde(default)]
    offset: usize,
}

#[derive(Debug, Serialize)]
struct Item {
    path: String,
    range: Range,
    severity: Severity,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<String>,
}

pub fn get(cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: GetParams = parse_params(params)?;
        let path = params
            .path
            .as_deref()
            .map(|p| resolve_path(p, &cx.shared.info.workspace_root));
        let scope = path.clone();
        let (snap, known) = on_editor(move |editor, _| {
            let snap = snapshot(editor, scope.as_deref());
            let known = scope.as_deref().is_none_or(|p| {
                editor.document_id_by_path(&open_path(editor, p)).is_some()
                    || editor.diagnostics.contains_key(&Uri::from(p))
            });
            (snap, known)
        })
        .await?;
        if let Some(path) = path.filter(|_| !known) {
            if tokio::fs::metadata(&path).await.is_err() {
                return Err(RpcError::not_found(format!("{} not found", path.display())));
            }
        }
        let limit = params.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_ITEMS);
        let min = params.min_severity.unwrap_or(Severity::Hint);
        tokio::task::spawn_blocking(move || page(snap, min, limit, params.offset))
            .await
            .map_err(|err| RpcError::new(ErrorCode::Unsupported, err.to_string()))
    })
}

/// Off the main thread: count, filter, sort, cut and convert.
fn page(snap: Snapshot, min: Severity, limit: usize, offset: usize) -> Value {
    let Snapshot {
        files,
        mut items,
        servers,
    } = snap;
    let mut counts = Counts::default();
    for item in &items {
        counts.add(item.severity);
    }
    items.retain(|d| d.severity >= min);
    let total = items.len();
    if limit == 0 {
        return json!({ "counts": counts, "total": total });
    }
    // errors first, then by path (files are sorted) and position
    items.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(a.file.cmp(&b.file))
            .then(a.at.sort_key().cmp(&b.at.sort_key()))
    });
    let page: Vec<Raw> = items.into_iter().skip(offset).take(limit).collect();
    let truncated = offset.saturating_add(page.len()) < total;
    let mut disk = Texts::new(HashMap::new());
    let out: Vec<Item> = page
        .into_iter()
        .map(|d| {
            let file = &files[d.file];
            let range = match d.at {
                At::Chars(start, end) => {
                    let text = file.text.as_ref().map(|t| t.slice(..));
                    match text {
                        Some(text) => Range {
                            start: pos_from_char(text, start),
                            end: pos_from_char(text, end.max(start)),
                        },
                        None => Range {
                            start: Position { line: 1, col: 1 },
                            end: Position { line: 1, col: 1 },
                        },
                    }
                }
                At::Lsp(range, enc) => range_from_lsp(disk.get(&file.path), range, enc),
            };
            Item {
                path: file.path.to_string_lossy().into_owned(),
                range,
                severity: d.severity,
                message: d.message,
                source: d.source,
                code: d.code,
                server: d.server.and_then(|id| servers.get(&id).cloned()),
            }
        })
        .collect();
    json!({ "items": out, "total": total, "truncated": truncated, "counts": counts })
}

// --- event/diagnostics -----------------------------------------------------------

enum Msg {
    /// Diagnostics of this path may have changed.
    Path(PathBuf),
    /// A server exited and its diagnostics were dropped.
    ServerExited,
}

runtime_local! {
    static TX: OnceLock<UnboundedSender<Msg>> = OnceLock::new();
}

fn send(msg: Msg) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(msg);
    }
}

pub fn register_hooks() {
    let Some(shared) = crate::bridge::shared() else {
        return;
    };
    let (tx, rx) = unbounded_channel();
    if TX.set(tx).is_err() {
        return;
    }
    let task = tokio::spawn(run(rx, shared.clone()));
    shared.track(task.abort_handle());

    // Open buffers (pushed and pulled diagnostics).
    register_hook!(move |event: &mut DiagnosticsDidChange<'_>| {
        if wants_kind(Kind::Diagnostics) {
            if let Some(path) = event.editor.document(event.doc).and_then(|d| d.path()) {
                send(Msg::Path(path.to_path_buf()));
            }
        }
        Ok(())
    });
    register_hook!(move |_event: &mut LanguageServerExited<'_>| {
        if wants_kind(Kind::Diagnostics) {
            send(Msg::ServerExited);
        }
        Ok(())
    });
}

/// A server published diagnostics for `uri`, open or not (called from
/// Helix's `publishDiagnostics` handler, before it stores them).
pub fn published(uri: &Uri) {
    if wants_kind(Kind::Diagnostics) {
        if let Some(path) = uri.as_path() {
            send(Msg::Path(path.to_path_buf()));
        }
    }
}

/// Paths waiting for their event, and what was last reported.
#[derive(Default)]
struct Coalescer {
    dirty: HashSet<PathBuf>,
    /// Counts last sent, for paths that had any.
    reported: HashMap<PathBuf, Counts>,
    /// Re-check every reported path (a server exited).
    sweep: bool,
    due: Option<Instant>,
    sent: Option<Instant>,
}

impl Coalescer {
    fn add(&mut self, msg: Msg) {
        match msg {
            Msg::Path(path) => {
                self.dirty.insert(path);
            }
            Msg::ServerExited => self.sweep = true,
        }
    }

    /// Leading-edge throttle, as for the other events: true to flush now.
    fn schedule(&mut self) -> bool {
        if self.due.is_some() {
            return false;
        }
        match self.sent {
            Some(sent) if sent.elapsed() < WINDOW => {
                self.due = Some(sent + WINDOW);
                false
            }
            _ => true,
        }
    }

    /// The paths to count now: `(path, only if changed)`.
    fn take(&mut self) -> Vec<(PathBuf, bool)> {
        self.due = None;
        let mut paths: Vec<(PathBuf, bool)> = self.dirty.drain().map(|p| (p, false)).collect();
        if std::mem::take(&mut self.sweep) {
            let listed: HashSet<PathBuf> = paths.iter().map(|(p, _)| p.clone()).collect();
            paths.extend(
                self.reported
                    .keys()
                    .filter(|p| !listed.contains(*p))
                    .map(|p| (p.clone(), true)),
            );
        }
        paths.sort();
        paths
    }

    /// Events to send for the counted paths.
    fn events(&mut self, counted: Vec<(PathBuf, bool, Counts)>) -> Vec<Value> {
        let mut out = Vec::new();
        for (path, only_if_changed, counts) in counted {
            let before = self.reported.get(&path).copied().unwrap_or_default();
            if only_if_changed && before == counts {
                continue;
            }
            if counts.is_empty() {
                self.reported.remove(&path);
            } else {
                self.reported.insert(path.clone(), counts);
            }
            let mut event = serde_json::to_value(counts).unwrap_or_default();
            event["path"] = path.to_string_lossy().into_owned().into();
            out.push(event);
        }
        if !out.is_empty() {
            self.sent = Some(Instant::now());
        }
        out
    }
}

async fn run(mut rx: UnboundedReceiver<Msg>, shared: Arc<Shared>) {
    let mut c = Coalescer::default();
    loop {
        let due = c.due;
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(msg) => {
                    c.add(msg);
                    if c.schedule() {
                        flush(&mut c, &shared).await;
                    }
                }
                None => break,
            },
            _ = sleep_until(due), if due.is_some() => flush(&mut c, &shared).await,
        }
    }
}

async fn sleep_until(due: Option<Instant>) {
    if let Some(due) = due {
        tokio::time::sleep_until(due).await
    }
}

async fn flush(c: &mut Coalescer, shared: &Shared) {
    let paths = c.take();
    if paths.is_empty() {
        return;
    }
    // Counted on the main thread after Helix stored the new diagnostics.
    let counted = on_editor(move |editor, _| {
        let open: HashMap<&Path, &helix_view::Document> = editor
            .documents()
            .filter_map(|d| Some((d.path()?, d)))
            .collect();
        paths
            .into_iter()
            .map(|(path, only_if_changed)| {
                let counts = counts_of(editor, &path, &open);
                (path, only_if_changed, counts)
            })
            .collect::<Vec<_>>()
    })
    .await;
    let Ok(counted) = counted else {
        return;
    };
    for event in c.events(counted) {
        broadcast(shared, Kind::Diagnostics, "event/diagnostics", event, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(file: usize, severity: Severity, at: At) -> Raw {
        Raw {
            file,
            severity,
            at,
            message: format!("{severity:?}"),
            source: Some("compiler".into()),
            code: None,
            server: None,
        }
    }

    fn lr(l1: u32, c1: u32, l2: u32, c2: u32) -> lsp::Range {
        lsp::Range::new(lsp::Position::new(l1, c1), lsp::Position::new(l2, c2))
    }

    fn snapshot_fixture() -> Snapshot {
        let open = Rope::from("aé🦀x\nline two\n");
        Snapshot {
            files: vec![
                File {
                    path: PathBuf::from("/r/a.go"),
                    text: Some(open),
                },
                File {
                    path: PathBuf::from("/r/does-not-exist.go"),
                    text: None,
                },
            ],
            items: vec![
                raw(
                    1,
                    Severity::Warning,
                    At::Lsp(lr(4, 2, 4, 5), OffsetEncoding::Utf16),
                ),
                raw(0, Severity::Hint, At::Chars(6, 10)),
                raw(0, Severity::Error, At::Chars(3, 4)),
                raw(
                    1,
                    Severity::Error,
                    At::Lsp(lr(0, 0, 0, 1), OffsetEncoding::Utf16),
                ),
                raw(0, Severity::Info, At::Chars(0, 1)),
            ],
            servers: HashMap::new(),
        }
    }

    #[test]
    fn errors_first_then_path_and_position() {
        let v = page(snapshot_fixture(), Severity::Hint, 100, 0);
        assert_eq!(v["total"], 5);
        assert_eq!(v["truncated"], false);
        assert_eq!(
            v["counts"],
            json!({ "errors": 2, "warnings": 1, "infos": 1, "hints": 1 })
        );
        let order: Vec<(String, String)> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i["severity"].as_str().unwrap().to_owned(),
                    i["path"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(order[0], ("error".into(), "/r/a.go".into()));
        assert_eq!(order[1], ("error".into(), "/r/does-not-exist.go".into()));
        assert_eq!(order[2].0, "warning");
        assert_eq!(order[3].0, "info");
        assert_eq!(order[4].0, "hint");
        // char offsets in the open buffer: 'x' is char 3, col 4
        assert_eq!(
            v["items"][0]["range"],
            json!({ "start": { "line": 1, "col": 4 }, "end": { "line": 1, "col": 5 } })
        );
        // a file that cannot be read: the server's numbers, 1-based
        assert_eq!(
            v["items"][2]["range"]["start"],
            json!({ "line": 5, "col": 3 })
        );
        assert_eq!(v["items"][0]["source"], "compiler");
        assert!(v["items"][0].get("code").is_none());
    }

    #[test]
    fn min_severity_limit_offset_and_counts_only() {
        let v = page(snapshot_fixture(), Severity::Warning, 2, 1);
        assert_eq!(v["total"], 3, "minSeverity applies to total");
        assert_eq!(v["counts"]["hints"], 1, "counts do not");
        assert_eq!(v["items"].as_array().unwrap().len(), 2);
        assert_eq!(v["truncated"], false);
        let v = page(snapshot_fixture(), Severity::Hint, 2, 0);
        assert_eq!(v["truncated"], true);
        let v = page(snapshot_fixture(), Severity::Error, 0, 0);
        assert_eq!(
            v,
            json!({ "counts": { "errors": 2, "warnings": 1, "infos": 1, "hints": 1 }, "total": 2 })
        );
    }

    #[test]
    fn severity_names_and_order() {
        assert!(Severity::Error > Severity::Warning && Severity::Info > Severity::Hint);
        let s: Severity = serde_json::from_value(json!("info")).unwrap();
        assert_eq!(s, Severity::Info);
        assert!(serde_json::from_value::<Severity>(json!("information")).is_err());
        assert_eq!(
            Severity::from(None::<lsp::DiagnosticSeverity>),
            Severity::Error
        );
    }

    #[tokio::test]
    async fn coalescer_sweeps_after_server_exit() {
        let mut c = Coalescer::default();
        c.add(Msg::Path("/r/a.go".into()));
        assert!(c.schedule(), "leading edge");
        let paths = c.take();
        assert_eq!(paths, vec![(PathBuf::from("/r/a.go"), false)]);
        let errors = Counts {
            errors: 2,
            ..Default::default()
        };
        let events = c.events(vec![("/r/a.go".into(), false, errors)]);
        assert_eq!(events[0]["errors"], 2);
        assert_eq!(events[0]["path"], "/r/a.go");

        // inside the window: waits
        c.add(Msg::Path("/r/b.go".into()));
        assert!(!c.schedule());
        assert!(c.due.is_some());

        // a sweep re-checks reported paths but only reports changes
        c.add(Msg::ServerExited);
        let paths = c.take();
        assert_eq!(
            paths,
            vec![
                (PathBuf::from("/r/a.go"), true),
                (PathBuf::from("/r/b.go"), false)
            ]
        );
        let events = c.events(vec![
            ("/r/a.go".into(), true, Counts::default()),
            ("/r/b.go".into(), false, Counts::default()),
        ]);
        assert_eq!(events.len(), 2);
        assert!(c.reported.is_empty());
        let events = c.events(vec![("/r/a.go".into(), true, Counts::default())]);
        assert!(events.is_empty(), "unchanged after a sweep");
    }
}
