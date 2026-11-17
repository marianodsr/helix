//! editor/reload and the focus-gained mtime check (step 2a).
//!
//! A reload happens in three steps so the main thread only applies a diff:
//!
//! 1. main thread: snapshot each candidate buffer (ids, path, the mtime
//!    Helix last read or wrote, an O(1) `Rope` clone);
//! 2. blocking task: stat each file; for the ones that changed, read and
//!    decode them with the buffer's encoding and diff against the snapshot
//!    (`compare_ropes`, as Helix's `:reload` does), and fetch the new diff
//!    base;
//! 3. main thread: apply each diff the way `Document::reload` does: one
//!    transaction, so selections and scroll positions in every view are
//!    mapped through it, the language server hears a normal didChange, and
//!    `u` undoes the reload; then the saved revision and the mtime that was
//!    read are recorded, indent and line ending re-detected, the diff base
//!    refreshed and `file_changed` sent, as `:reload` does.
//!
//! If the buffer changed between steps 1 and 3 the diff is recomputed on the
//! main thread against its current text. No file watcher: the check runs on
//! `editor/reload` and when the terminal regains focus.

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use futures_util::future::BoxFuture;
use helix_core::{diff::compare_ropes, encoding::Encoding, Rope, Transaction};
use helix_event::runtime_local;
use helix_vcs::DiffProviderRegistry;
use helix_view::{DocumentId, Editor, ViewId};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bridge::convert::{path_string, pos_from_char, resolve_path};
use crate::bridge::dispatch::on_editor;
use crate::bridge::events::{self, broadcast, DocMeta, Kind, Msg};
use crate::bridge::methods::locations::{parse_interrupt, request_jump, Interrupt, JumpTarget};
use crate::bridge::protocol::{parse_params, Range, RpcError, RpcResult};
use crate::bridge::server::Ctx;

pub const CAPABILITY: Option<&str> = Some("reload");

/// Hooks this module needs (registered when the bridge starts).
pub fn register_hooks() {}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReloadParams {
    paths: Option<Vec<String>>,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    reveal: bool,
    #[serde(default)]
    follow: bool,
    interrupt: Option<String>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReloadResult {
    pub reloaded: Vec<String>,
    pub unchanged: Vec<String>,
    pub skipped_modified: Vec<String>,
    pub missing: Vec<String>,
    /// Only with `follow`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExternalChangeEvent {
    path: String,
    modified: bool,
}

/// Where a reload was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Request {
        force: bool,
        reveal: bool,
        follow: bool,
        interrupt: Interrupt,
    },
    FocusGained,
}

impl Origin {
    fn force(self) -> bool {
        matches!(self, Origin::Request { force: true, .. })
    }
}

/// One open buffer, as the main thread saw it.
struct Candidate {
    doc: DocumentId,
    path: PathBuf,
    version: i32,
    modified: bool,
    saved_at: SystemTime,
    text: Rope,
    encoding: &'static Encoding,
    trust_full: bool,
}

/// What the blocking task found for a candidate.
enum Found {
    Missing,
    /// Same mtime as Helix last saw.
    Same,
    /// The mtime moved but the content equals the buffer.
    SameContent(SystemTime),
    /// Changed on disk while the buffer has unsaved changes.
    ChangedUnderEdits(SystemTime),
    Changed(Box<Change>),
    Error(String),
}

struct Change {
    text: Rope,
    mtime: SystemTime,
    transaction: Transaction,
    diff_base: Option<Vec<u8>>,
    head: Option<std::sync::Arc<arc_swap::ArcSwap<Box<str>>>>,
}

/// What a reload changed in one buffer, in the new text: the char ranges it
/// inserted and the text it removed, by the 1-based line it was removed
/// above. This is what the reveal animation (step 2d) plays.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadChanges {
    pub inserted: Vec<std::ops::Range<usize>>,
    pub removed: Vec<Removed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Removed {
    pub line: usize,
    pub text: String,
}

impl ReloadChanges {
    /// `inserted` as protocol ranges.
    pub fn ranges(&self, text: &Rope) -> Vec<Range> {
        let s = text.slice(..);
        self.inserted
            .iter()
            .map(|r| Range {
                start: pos_from_char(s, r.start),
                end: pos_from_char(s, r.end),
            })
            .collect()
    }

    /// Where the first change starts, as a char offset in the new text.
    fn first(&self, text: &Rope) -> Option<usize> {
        let removed = self
            .removed
            .first()
            .map(|r| text.line_to_char((r.line - 1).min(text.len_lines() - 1)));
        let inserted = self.inserted.first().map(|r| r.start);
        match (inserted, removed) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// Walk a reload's change set: insertions as ranges in the new text,
/// deletions as the removed lines of the old text.
pub fn reload_changes(old: &Rope, new: &Rope, transaction: &Transaction) -> ReloadChanges {
    use helix_core::Operation;
    let mut out = ReloadChanges::default();
    let (mut old_pos, mut new_pos) = (0, 0);
    for op in transaction.changes().changes() {
        match op {
            Operation::Retain(n) => {
                old_pos += n;
                new_pos += n;
            }
            Operation::Delete(n) => {
                let line = new.char_to_line(new_pos.min(new.len_chars())) + 1;
                let removed = old.slice(old_pos..old_pos + n).to_string();
                let mut lines: Vec<&str> = removed.split_inclusive('\n').collect();
                if lines.is_empty() {
                    lines.push("");
                }
                for l in lines {
                    out.removed.push(Removed {
                        line,
                        text: l.trim_end_matches(['\n', '\r']).to_owned(),
                    });
                }
                old_pos += n;
            }
            Operation::Insert(s) => {
                let n = s.chars().count();
                match out.inserted.last_mut() {
                    Some(last) if last.end == new_pos => last.end += n,
                    _ => out.inserted.push(new_pos..new_pos + n),
                }
                new_pos += n;
            }
        }
    }
    out
}

/// What happened to one buffer on the main thread.
enum Applied {
    Reloaded { changes: ReloadChanges, text: Rope },
    Unchanged,
    SkippedModified,
    Gone,
}

// --- editor/reload -------------------------------------------------------------

pub fn reload(cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: ReloadParams = parse_params(params)?;
        let interrupt = parse_interrupt(params.interrupt.as_deref())?;
        let root = &cx.shared.info.workspace_root;
        let paths = params
            .paths
            .map(|paths| paths.iter().map(|p| resolve_path(p, root)).collect());
        let origin = Origin::Request {
            force: params.force,
            reveal: params.reveal,
            follow: params.follow,
            interrupt,
        };
        let result = run(paths, origin).await?;
        Ok(serde_json::to_value(result).unwrap_or_default())
    })
}

/// The whole reload: snapshot, check and read off the main thread, apply.
async fn run(paths: Option<Vec<PathBuf>>, origin: Origin) -> Result<ReloadResult, RpcError> {
    let (candidates, providers) = on_editor(move |editor, _| {
        (
            snapshot(editor, paths.as_deref()),
            editor.diff_providers.clone(),
        )
    })
    .await?;
    if candidates.is_empty() {
        return Ok(ReloadResult::default());
    }
    let force = origin.force();
    let (candidates, found) = tokio::task::spawn_blocking(move || {
        let found = candidates
            .iter()
            .map(|c| inspect(c, force, &providers))
            .collect::<Vec<_>>();
        (candidates, found)
    })
    .await
    .map_err(|err| RpcError::new(crate::bridge::protocol::ErrorCode::Timeout, err.to_string()))?;

    let mut result = ReloadResult::default();
    let mut work = Vec::new();
    let mut external = Vec::new();
    for (c, found) in candidates.into_iter().zip(found) {
        let path = c.path.to_string_lossy().into_owned();
        match found {
            Found::Missing => result.missing.push(path),
            Found::Same => result.unchanged.push(path),
            Found::Error(err) => {
                log::warn!("bridge: reload {path}: {err}");
                result.missing.push(path);
            }
            Found::ChangedUnderEdits(mtime) => {
                result.skipped_modified.push(path);
                if first_notice(c.doc, mtime) {
                    external.push(c.path.clone());
                }
            }
            found @ (Found::SameContent(_) | Found::Changed(_)) => work.push((c, found)),
        }
    }
    if !external.is_empty() {
        announce_external(&external);
    }
    if work.is_empty() {
        if origin == Origin::FocusGained && !external.is_empty() {
            on_editor(move |editor, _| warn_external(editor, &external)).await?;
        }
        return Ok(result);
    }

    let begin = Instant::now();
    let (applied, deferred, late_external) = on_editor(move |editor, _| {
        let main = Instant::now();
        let mut applied = Vec::with_capacity(work.len());
        let mut late_external = Vec::new();
        let mut follow_to = None;
        for (c, found) in work {
            let outcome = apply(editor, &c, found, origin);
            match &outcome {
                Applied::Reloaded { changes, text } => {
                    // REVEAL CALL SITE (step 2d): with `reveal`, the
                    // coordinator calls `reveal::play(editor, c.doc,
                    // changes.ranges(text), changes.removed.clone(), 600,
                    // false)` here, once per reloaded buffer.
                    if let Origin::Request { reveal: true, .. } = origin {
                        let _ = (changes.ranges(text), &changes.removed);
                    }
                    if follow_to.is_none() {
                        follow_to = changes.first(text).map(|at| {
                            let p = pos_from_char(text.slice(..), at);
                            (c.path.clone(), p)
                        });
                    }
                }
                Applied::SkippedModified => {
                    // the user typed while the file was being read
                    late_external.push(c.path.clone());
                }
                Applied::Unchanged | Applied::Gone => {}
            }
            applied.push((c.path, outcome));
        }
        let mut deferred = None;
        if let Origin::Request {
            follow: true,
            interrupt,
            ..
        } = origin
        {
            deferred = Some(false);
            if let Some((path, at)) = follow_to {
                let target = JumpTarget {
                    path,
                    line: at.line,
                    col: Some(at.col),
                    end_line: None,
                    status: None,
                };
                match request_jump(editor, target, interrupt) {
                    Ok(d) => deferred = Some(d),
                    Err(err) => editor.set_warning(format!("bb: {err}")),
                }
            }
        }
        if origin == Origin::FocusGained && !(external.is_empty() && late_external.is_empty()) {
            let all: Vec<_> = external.iter().chain(&late_external).cloned().collect();
            warn_external(editor, &all);
        }
        log::debug!(
            "bridge: reload applied {} buffer(s) in {:?} on the main thread",
            applied.len(),
            main.elapsed()
        );
        (applied, deferred, late_external)
    })
    .await?;
    log::debug!("bridge: reload round trip {:?}", begin.elapsed());
    if !late_external.is_empty() {
        announce_external(&late_external);
    }
    for (path, outcome) in applied {
        let path = path.to_string_lossy().into_owned();
        match outcome {
            Applied::Reloaded { .. } => result.reloaded.push(path),
            Applied::Unchanged => result.unchanged.push(path),
            Applied::SkippedModified => result.skipped_modified.push(path),
            Applied::Gone => {}
        }
    }
    result.deferred = deferred;
    Ok(result)
}

/// Copy what the blocking task needs from each open buffer with a file.
fn snapshot(editor: &Editor, paths: Option<&[PathBuf]>) -> Vec<Candidate> {
    editor
        .documents()
        .filter_map(|doc| {
            let path = doc.path()?;
            if let Some(paths) = paths {
                if !paths.iter().any(|p| p == path) {
                    return None;
                }
            }
            let trust_full = editor
                .workspace_trust
                .query(
                    doc.workspace_root(),
                    helix_loader::workspace_trust::TrustQuery::Git,
                )
                .is_trusted();
            Some(Candidate {
                doc: doc.id(),
                path: path.to_path_buf(),
                version: doc.version(),
                modified: doc.is_modified(),
                saved_at: doc.last_saved_time(),
                text: doc.text().clone(),
                encoding: doc.encoding(),
                trust_full,
            })
        })
        .collect()
}

/// Off the main thread: has the file changed, and into what?
fn inspect(c: &Candidate, force: bool, providers: &DiffProviderRegistry) -> Found {
    let mtime = match std::fs::metadata(&c.path) {
        Ok(meta) if meta.is_file() => meta.modified().unwrap_or_else(|_| SystemTime::now()),
        Ok(_) => return Found::Missing,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Found::Missing,
        Err(err) => return Found::Error(err.to_string()),
    };
    if mtime == c.saved_at {
        return Found::Same;
    }
    if c.modified && !force {
        return Found::ChangedUnderEdits(mtime);
    }
    let (text, mtime) = match read_stable(&c.path, c.encoding, mtime) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Found::Missing,
        Err(err) => return Found::Error(err.to_string()),
    };
    if text == c.text {
        return Found::SameContent(mtime);
    }
    let transaction = compare_ropes(&c.text, &text);
    Found::Changed(Box::new(Change {
        diff_base: providers.get_diff_base(&c.path, c.trust_full),
        head: providers.get_current_head_name(&c.path, c.trust_full),
        text,
        mtime,
        transaction,
    }))
}

/// Read the file, retrying if it changed while being read, so the mtime
/// recorded is the one of the text read.
fn read_stable(
    path: &Path,
    encoding: &'static Encoding,
    mut mtime: SystemTime,
) -> std::io::Result<(Rope, SystemTime)> {
    let mut tries = 0;
    loop {
        let mut file = std::fs::File::open(path)?;
        let (text, ..) = helix_view::document::from_reader(&mut file, Some(encoding))?;
        let after = std::fs::metadata(path)?
            .modified()
            .unwrap_or_else(|_| SystemTime::now());
        tries += 1;
        if after == mtime || tries == 3 {
            return Ok((text, after));
        }
        mtime = after;
    }
}

/// The view a reload is applied through: the focused one if it shows the
/// document, else any that does, else the focused one (the document then
/// gets a selection there, as `:reload-all` does for hidden buffers).
fn view_for(editor: &mut Editor, doc: DocumentId) -> ViewId {
    let focus = editor.tree.focus;
    let view = if editor.tree.get(focus).doc == doc {
        focus
    } else {
        editor
            .tree
            .views()
            .find(|(v, _)| v.doc == doc)
            .map(|(v, _)| v.id)
            .unwrap_or(focus)
    };
    if let Some(d) = editor.document_mut(doc) {
        d.ensure_view_init(view);
    }
    view
}

/// On the main thread: apply what the blocking task found.
fn apply(editor: &mut Editor, c: &Candidate, found: Found, origin: Origin) -> Applied {
    let Some(doc) = editor.document_mut(c.doc) else {
        return Applied::Gone;
    };
    if !origin.force() && doc.is_modified() {
        return Applied::SkippedModified;
    }
    let change = match found {
        Found::SameContent(mtime) => {
            // nothing to apply; remember the new mtime when the buffer is
            // still in its saved state
            if !doc.is_modified() {
                let rev = doc.get_current_revision();
                doc.set_last_saved_revision(rev, mtime);
            }
            return Applied::Unchanged;
        }
        Found::Changed(change) => change,
        _ => return Applied::Unchanged,
    };
    let Change {
        text,
        mtime,
        mut transaction,
        diff_base,
        head,
    } = *change;
    let old = if doc.version() == c.version {
        c.text.clone()
    } else {
        // changed while the file was read: diff against what it is now
        transaction = compare_ropes(doc.text(), &text);
        doc.text().clone()
    };
    let changes = reload_changes(&old, &text, &transaction);

    let view_id = view_for(editor, c.doc);
    let scrolloff = editor.config().scrolloff;
    let view = view_mut!(editor, view_id);
    let doc = doc_mut!(editor, &c.doc);
    doc.detect_readonly();
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    let rev = doc.get_current_revision();
    doc.set_last_saved_revision(rev, mtime);
    doc.detect_indent_and_line_ending();
    if let Some(base) = diff_base {
        doc.set_diff_base(base);
    }
    doc.set_version_control_head(head);
    let new_text = doc.text().clone();
    if events::wants_kind(Kind::Documents) {
        // the change hook reported `modified: true`; the buffer is clean now
        events::send(Msg::Changed(DocMeta::of(doc)));
    }
    editor
        .language_servers
        .file_event_handler
        .file_changed(c.path.clone());
    let views: Vec<ViewId> = editor
        .tree
        .views()
        .filter(|(v, _)| v.doc == c.doc)
        .map(|(v, _)| v.id)
        .collect();
    for id in views {
        let view = view_mut!(editor, id);
        let doc = doc_mut!(editor, &c.doc);
        view.ensure_cursor_in_view(doc, scrolloff);
    }
    forget_notice(c.doc);
    Applied::Reloaded {
        changes,
        text: new_text,
    }
}

// --- external changes ------------------------------------------------------------

runtime_local! {
    /// The mtime each modified buffer was last reported changed at, so each
    /// change is announced once.
    static NOTICED: Mutex<Vec<(DocumentId, SystemTime)>> = Mutex::new(Vec::new());
}

fn first_notice(doc: DocumentId, mtime: SystemTime) -> bool {
    let mut noticed = NOTICED.lock();
    match noticed.iter_mut().find(|(d, _)| *d == doc) {
        Some((_, seen)) if *seen == mtime => false,
        Some((_, seen)) => {
            *seen = mtime;
            true
        }
        None => {
            noticed.push((doc, mtime));
            true
        }
    }
}

fn forget_notice(doc: DocumentId) {
    NOTICED.lock().retain(|(d, _)| *d != doc);
}

/// `event/externalChange` for each path, from the bridge task.
fn announce_external(paths: &[PathBuf]) {
    if !events::wants_kind(Kind::ExternalChange) {
        return;
    }
    let Some(shared) = crate::bridge::shared() else {
        return;
    };
    for path in paths {
        broadcast(
            shared,
            Kind::ExternalChange,
            "event/externalChange",
            ExternalChangeEvent {
                path: path_string(Some(path)).unwrap_or_default(),
                modified: true,
            },
            false,
        );
    }
}

fn warn_external(editor: &mut Editor, paths: &[PathBuf]) {
    let names: Vec<String> = paths
        .iter()
        .map(|p| {
            helix_stdx::path::get_relative_path(p)
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    editor.set_warning(match names.as_slice() {
        [one] => {
            format!("{one} changed on disk; your unsaved changes are kept (:reload discards them)")
        }
        many => format!(
            "{} files with unsaved changes changed on disk: {} (:reload discards your changes)",
            many.len(),
            many.join(", ")
        ),
    });
}

// --- focus gained ------------------------------------------------------------------

/// The terminal regained focus: reload clean buffers whose file changed and
/// warn about modified ones. The main thread only copies ids and paths; the
/// stats happen on a blocking thread, and nothing more runs when nothing
/// changed.
pub fn focus_gained(editor: &mut Editor) {
    if crate::bridge::shared().is_none() || editor.documents().all(|d| d.path().is_none()) {
        return;
    }
    tokio::spawn(async move {
        if let Err(err) = run(None, Origin::FocusGained).await {
            log::warn!("bridge: focus check failed: {}", err.message);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::protocol::Position;

    fn changes(old: &str, new: &str) -> ReloadChanges {
        let (old, new) = (Rope::from(old), Rope::from(new));
        let t = compare_ropes(&old, &new);
        let mut applied = old.clone();
        assert!(t.apply(&mut applied));
        assert_eq!(applied, new);
        reload_changes(&old, &new, &t)
    }

    #[test]
    fn inserted_lines_are_ranges_in_the_new_text() {
        let c = changes("a\nb\nc\n", "a\nb\nnew\nc\n");
        assert_eq!(c.inserted, vec![4..8]);
        assert!(c.removed.is_empty());
        let text = Rope::from("a\nb\nnew\nc\n");
        assert_eq!(
            c.ranges(&text),
            vec![Range {
                start: Position { line: 3, col: 1 },
                end: Position { line: 4, col: 1 }
            }]
        );
        assert_eq!(c.first(&text), Some(4));
    }

    #[test]
    fn removed_lines_keep_their_text_and_place() {
        let c = changes("one\ntwo\nthree\nfour\n", "one\nfour\n");
        assert!(c.inserted.is_empty());
        assert_eq!(
            c.removed,
            vec![
                Removed {
                    line: 2,
                    text: "two".into()
                },
                Removed {
                    line: 2,
                    text: "three".into()
                },
            ]
        );
        assert_eq!(c.first(&Rope::from("one\nfour\n")), Some(4));
    }

    #[test]
    fn multibyte_and_crlf() {
        let c = changes("aé\r\n🦀x\r\n", "aé\r\n🦀yz\r\n");
        let text = Rope::from("aé\r\n🦀yz\r\n");
        let ranges = c.ranges(&text);
        assert_eq!(ranges.len(), 1);
        // the crab is one char: "yz" starts at col 2 of line 2
        assert_eq!(ranges[0].start, Position { line: 2, col: 2 });
        assert_eq!(ranges[0].end, Position { line: 2, col: 4 });
        assert_eq!(
            c.removed,
            vec![Removed {
                line: 2,
                text: "x".into()
            }]
        );
    }

    #[test]
    fn end_of_file_without_newline() {
        let c = changes("a\nb", "a\nb\nc");
        let text = Rope::from("a\nb\nc");
        let r = c.ranges(&text);
        assert_eq!(r.last().unwrap().end, Position { line: 3, col: 2 });
    }

    #[test]
    fn notices_once_per_mtime() {
        let doc = DocumentId::default();
        let t = SystemTime::UNIX_EPOCH;
        let later = t + std::time::Duration::from_secs(1);
        NOTICED.lock().clear();
        assert!(first_notice(doc, t));
        assert!(!first_notice(doc, t));
        assert!(first_notice(doc, later));
    }
}
