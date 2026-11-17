//! review/set, review/clear, review/state, `:bb-review`, `:bb-diff` and the
//! inline diff (step 5).
//!
//! A review changes what Helix's own diff compares each buffer with: the
//! file at the review's base revision instead of `HEAD` (helix-vcs
//! `review`, which `get_diff_base` consults, so a file opened or reloaded
//! later gets the review's base too). The gutter, `]g`/`[g` and
//! `:reset-diff-change` follow it, and Helix keeps the hunks current as the
//! user or an agent edits. The inline diff only draws those hunks: removed
//! lines as virtual lines above each change, struck through, and a
//! background on added and changed lines. Nothing in the document changes.
//!
//! When the review starts or ends, the open buffers get their new base off
//! the main thread: the blob is read and decoded in a blocking task and
//! handed to the buffer's diff worker; only a buffer that had no diff at
//! all (or must lose it) is touched on the main thread.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use futures_util::future::BoxFuture;
use helix_core::command_line::{Args, Signature};
use helix_core::doc_formatter::FormattedGrapheme;
use helix_core::line_ending::line_end_char_index;
use helix_core::unicode::segmentation::UnicodeSegmentation;
use helix_core::unicode::width::UnicodeWidthStr;
use helix_core::{Position, Rope, RopeSlice, Selection, Transaction};
use helix_event::runtime_local;
use helix_loader::workspace_trust::TrustQuery;
use helix_vcs::Hunk;
use helix_view::annotations::bridge::{
    display_text, inline_diff, removed_layout, removed_wrap, set_inline_diff, DiffSnapshot,
    RemovedWrap,
};
use helix_view::editor::{Action, GutterType};
use helix_view::graphics::{Margin, Modifier, Rect, Style};
use helix_view::input::KeyEvent;
use helix_view::keyboard::{KeyCode, KeyModifiers};
use helix_view::{align_view, Align, Document, DocumentId, Editor, Theme, View};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tui::buffer::Buffer as Surface;
use tui::text::{Span, Spans};
use tui::widgets::Block;

use super::annotations::blend;
use crate::bridge::convert::{doc_num, path_string, resolve_path};
use crate::bridge::dispatch::{on_editor, on_editor_read};
use crate::bridge::events::{self, broadcast, Kind};
use crate::bridge::methods::interrupt::{self, Interrupt};
use crate::bridge::methods::locations::{jump, JumpTarget};
use crate::bridge::protocol::{parse_params, ErrorCode, RpcError, RpcResult};
use crate::bridge::server::Ctx;
use crate::commands::typed::{CommandCompleter, TypableCommand};
use crate::compositor::{self, Component, Compositor, Context, Event, EventResult};
use crate::job::Callback;
use crate::ui::document::{LinePos, TextRenderer};
use crate::ui::overlay::overlaid;
use crate::ui::prompt::Completion;
use crate::ui::text_decorations::{Decoration, DecorationManager};
use crate::ui::{self, PromptEvent};

pub const CAPABILITY: Option<&str> = Some("review");

/// Most files one review lists.
const MAX_FILES: usize = 5000;
/// Lines of context around each change in the picker's preview.
const CONTEXT: usize = 3;
/// Most rows the picker's preview builds for one file.
const MAX_PREVIEW_ROWS: usize = 3000;

/// Target words `:bb-review` completes. Anything else is passed on too.
const TARGETS: &[&str] = &["uncommitted", "branch", "turn", "commit", "pr", "off"];

pub const COMMANDS: &[TypableCommand] = &[
    TypableCommand {
        name: "bb-review",
        aliases: &[],
        doc: "Pick a file of the bb review. With a target (uncommitted, branch, turn, commit, pr, off), start or end a review.",
        fun: bb_review,
        completer: CommandCompleter::positional(&[complete_target]),
        signature: Signature {
            positionals: (0, Some(1)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "bb-diff",
        aliases: &[],
        doc: "Show or hide removed and added lines inline, against the review's base or HEAD.",
        fun: bb_diff,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
];

// --- helix-vcs ---------------------------------------------------------------

#[cfg(feature = "git")]
use helix_vcs::review as vcs;

/// Without git support there is no review; the commands and methods say so.
#[cfg(not(feature = "git"))]
mod vcs {
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Repo {
        pub git_dir: PathBuf,
        pub workdir: PathBuf,
    }
    #[derive(Debug, Clone, Copy)]
    pub struct ObjectId;
    pub struct Review {
        pub repo: Repo,
        pub rev: Option<ObjectId>,
        pub renamed: HashMap<PathBuf, PathBuf>,
        pub added: HashSet<PathBuf>,
    }
    pub enum OpenError {
        NoRepository(String),
        UnknownRevision(String),
    }
    impl std::fmt::Display for OpenError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                OpenError::NoRepository(m) | OpenError::UnknownRevision(m) => f.write_str(m),
            }
        }
    }
    pub fn open(_: &Path, _: Option<&str>, _: bool) -> Result<(Repo, Option<ObjectId>), OpenError> {
        Err(OpenError::NoRepository(
            "this Helix was built without git support".into(),
        ))
    }
    pub fn set(_: Review) {}
    pub fn clear() {}
    pub fn file_at_base(_: &Path, _: bool) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("this Helix was built without git support")
    }
    pub fn hunks(_: &helix_core::Rope, _: &helix_core::Rope) -> Vec<helix_vcs::Hunk> {
        Vec::new()
    }
    pub fn resolve(path: &Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok()
    }
}

// --- state -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum Status {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl Status {
    fn letter(self) -> &'static str {
        match self {
            Status::Added => "A",
            Status::Modified => "M",
            Status::Deleted => "D",
            Status::Renamed => "R",
        }
    }
}

#[derive(Debug, Clone)]
struct File {
    path: PathBuf,
    status: Status,
    old_path: Option<PathBuf>,
    additions: u64,
    deletions: u64,
}

struct Active {
    id: String,
    title: String,
    /// The revision as the client gave it; `None` is `HEAD`.
    base: Option<String>,
    files: Vec<File>,
    workdir: PathBuf,
}

runtime_local! {
    static STATE: Mutex<Option<Active>> = Mutex::new(None);
    /// Bumped by every review/set and review/clear, so a rebase that a newer
    /// one overtook stops.
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    /// One rebase at a time, in the order they were asked for.
    static REBASE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
}

/// `[editor.bridge] inline-diff`: the inline diff's state at start. Called
/// from `bridge::start` whether or not the socket is enabled.
pub fn init(editor: &Editor) {
    set_inline_diff(editor.config().bridge.inline_diff);
}

/// The active review's id, for `event/send`.
pub fn active_id() -> Option<String> {
    STATE.lock().as_ref().map(|a| a.id.clone())
}

fn trust_for(editor: &Editor, path: &Path) -> bool {
    let dir = path.parent().unwrap_or(path);
    editor
        .workspace_trust
        .query(&helix_loader::find_workspace_in(dir).0, TrustQuery::Git)
        .is_trusted()
}

fn unsupported(reason: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code: ErrorCode::Unsupported,
        message: message.into(),
        data: Some(json!({ "reason": reason })),
    }
}

// --- starting and ending a review -----------------------------------------------

/// A review about to become the active one.
struct NewReview {
    id: String,
    title: String,
    base: Option<String>,
    files: Vec<File>,
    /// `Some` to switch the inline diff on or off.
    inline: Option<bool>,
}

/// Resolve the repository and revision, make it the base Helix compares
/// with, store the list and move every open buffer of the repository to
/// the new base. Returns the number of files.
async fn start_review(mut new: NewReview, anchor: PathBuf, trust: bool) -> Result<usize, RpcError> {
    let rev = new.base.clone();
    let files = std::mem::take(&mut new.files);
    let (opened, files) = tokio::task::spawn_blocking(move || {
        // symlinks resolved (macOS's /tmp, /var), as git sees the files
        let real = |p: &Path| vcs::resolve(p).unwrap_or_else(|| p.to_path_buf());
        let files: Vec<File> = files
            .into_iter()
            .map(|f| File {
                path: real(&f.path),
                old_path: f.old_path.as_deref().map(real),
                ..f
            })
            .collect();
        (vcs::open(&anchor, rev.as_deref(), trust), files)
    })
    .await
    .map_err(|err| RpcError::new(ErrorCode::Unsupported, err.to_string()))?;
    new.files = files;
    let (repo, rev) = opened.map_err(|err| match err {
        vcs::OpenError::NoRepository(msg) => unsupported("not a git repository", msg),
        vcs::OpenError::UnknownRevision(msg) => unsupported("unknown revision", msg),
    })?;
    let relative = |path: &Path| path.strip_prefix(&repo.workdir).map(Path::to_path_buf).ok();
    let mut renamed = HashMap::new();
    let mut added = HashSet::new();
    for file in &new.files {
        let Some(rel) = relative(&file.path) else {
            continue;
        };
        match (file.status, &file.old_path) {
            (Status::Renamed, Some(old)) => {
                if let Some(old) = relative(old) {
                    renamed.insert(rel, old);
                }
            }
            (Status::Added, _) => {
                added.insert(rel);
            }
            _ => {}
        }
    }
    let workdir = repo.workdir.clone();
    let count = new.files.len();
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    let previous = {
        let mut state = STATE.lock();
        let same = state.as_ref().is_some_and(|a| a.id == new.id);
        let previous = state.as_ref().map(|a| a.workdir.clone());
        vcs::set(vcs::Review {
            repo,
            rev,
            renamed,
            added,
        });
        *state = Some(Active {
            id: new.id,
            title: new.title,
            base: new.base,
            files: new.files,
            workdir: workdir.clone(),
        });
        // a new review shows the diff unless told otherwise; the same one
        // sent again leaves `:bb-diff` as the user set it
        match new.inline {
            Some(on) => set_inline_diff(on),
            None if !same => set_inline_diff(true),
            None => {}
        }
        previous
    };
    let mut workdirs = vec![workdir];
    workdirs.extend(previous.filter(|p| *p != workdirs[0]));
    rebase(workdirs, generation).await?;
    Ok(count)
}

/// End the review: back to `HEAD`, inline diff off.
async fn end_review() -> Result<(), RpcError> {
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    let previous = {
        let mut state = STATE.lock();
        vcs::clear();
        set_inline_diff(false);
        state.take()
    };
    if let Some(previous) = previous {
        rebase(vec![previous.workdir], generation).await?;
    }
    Ok(())
}

struct OpenDoc {
    doc: DocumentId,
    path: PathBuf,
    trust: bool,
    encoding: &'static helix_core::encoding::Encoding,
    handle: Option<helix_vcs::DiffHandle>,
}

/// Give every open buffer under `workdirs` the base Helix computes for it
/// now (the review's, or `HEAD`'s once it ended). Reading and decoding run
/// in a blocking task, and a buffer that already diffs gets its new base
/// through its diff worker from there.
async fn rebase(workdirs: Vec<PathBuf>, generation: u64) -> Result<(), RpcError> {
    let _turn = REBASE.lock().await;
    if GENERATION.load(Ordering::Relaxed) != generation {
        return Ok(());
    }
    let (docs, providers) = on_editor_read(|editor, _| {
        let docs: Vec<OpenDoc> = editor
            .documents()
            .filter_map(|doc| {
                Some(OpenDoc {
                    doc: doc.id(),
                    path: doc.path()?.to_path_buf(),
                    trust: editor
                        .workspace_trust
                        .query(doc.workspace_root(), TrustQuery::Git)
                        .is_trusted(),
                    encoding: doc.encoding(),
                    handle: doc.diff_handle().cloned(),
                })
            })
            .collect();
        (docs, editor.diff_providers.clone())
    })
    .await?;
    let on_main = tokio::task::spawn_blocking(move || {
        let mut on_main = Vec::new();
        for d in docs {
            if GENERATION.load(Ordering::Relaxed) != generation {
                return Vec::new();
            }
            let real = std::fs::canonicalize(&d.path).unwrap_or_else(|_| d.path.clone());
            if !workdirs.iter().any(|w| real.starts_with(w)) {
                continue;
            }
            match (providers.get_diff_base(&d.path, d.trust), d.handle) {
                (Some(bytes), Some(handle)) => {
                    let Ok((base, ..)) =
                        helix_view::document::from_reader(&mut bytes.as_slice(), Some(d.encoding))
                    else {
                        continue;
                    };
                    if *handle.load().diff_base() != base {
                        handle.update_diff_base(base);
                    }
                }
                (Some(bytes), None) => on_main.push((d.doc, Some(bytes))),
                (None, Some(_)) => on_main.push((d.doc, None)),
                (None, None) => {}
            }
        }
        on_main
    })
    .await
    .unwrap_or_default();
    if on_main.is_empty() {
        return Ok(());
    }
    on_editor(move |editor, _| {
        if GENERATION.load(Ordering::Relaxed) != generation {
            return;
        }
        for (doc, base) in on_main {
            let Some(doc) = editor.document_mut(doc) else {
                continue;
            };
            match base {
                Some(base) => doc.set_diff_base(base),
                None => doc.clear_diff_handle(),
            }
        }
    })
    .await
}

// --- methods -----------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetParams {
    id: String,
    title: Option<String>,
    #[serde(default)]
    base: Option<BaseParams>,
    files: Vec<FileParams>,
    inline: Option<bool>,
    #[serde(default)]
    open: bool,
    interrupt: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BaseParams {
    rev: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileParams {
    path: String,
    status: Status,
    old_path: Option<String>,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
}

pub fn set(cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: SetParams = parse_params(params)?;
        let interrupt = Interrupt::parse(params.interrupt.as_deref())?;
        if params.id.is_empty() {
            return Err(RpcError::invalid_params("id", "id must not be empty"));
        }
        if params.files.len() > MAX_FILES {
            return Err(RpcError::invalid_params(
                "files",
                format!("at most {MAX_FILES} files"),
            ));
        }
        if params.base.as_ref().is_some_and(|b| b.rev.is_empty()) {
            return Err(RpcError::invalid_params(
                "base",
                "base.rev must not be empty",
            ));
        }
        let root = cx.shared.info.workspace_root.clone();
        let files: Vec<File> = params
            .files
            .into_iter()
            .map(|f| File {
                path: resolve_path(&f.path, &root),
                status: f.status,
                old_path: f.old_path.map(|p| resolve_path(&p, &root)),
                additions: f.additions,
                deletions: f.deletions,
            })
            .collect();
        let anchor = files.first().map(|f| f.path.clone()).unwrap_or(root);
        let at = anchor.clone();
        let trust = on_editor_read(move |editor, _| trust_for(editor, &at)).await?;
        let base = params.base.map(|b| b.rev);
        let shown_base = base.clone().unwrap_or_else(|| "HEAD".to_owned());
        let title = params.title.unwrap_or_else(|| params.id.clone());
        let count = start_review(
            NewReview {
                id: params.id,
                title,
                base,
                files,
                inline: params.inline,
            },
            anchor,
            trust,
        )
        .await?;
        let mut deferred = false;
        if params.open {
            deferred = on_editor(move |editor, compositor| {
                let what = format!("show the review's {count} files");
                interrupt::jump(editor, compositor, interrupt, what, |editor, compositor| {
                    // the review is set either way; a list with nothing
                    // to pick is said on the status line
                    match picker(editor) {
                        Ok(picker) => compositor.push(picker),
                        Err(err) => editor.set_error(err.to_string()),
                    }
                    Ok(())
                })
                .map(|moved| moved.is_none())
            })
            .await??;
        }
        Ok(json!({ "files": count, "base": shown_base, "deferred": deferred }))
    })
}

pub fn clear(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        end_review().await?;
        Ok(json!({}))
    })
}

pub fn state(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let state = STATE.lock();
        Ok(match state.as_ref() {
            Some(a) => json!({
                "active": true,
                "id": a.id,
                "title": a.title,
                "base": a.base.as_deref().unwrap_or("HEAD"),
                "inline": inline_diff(),
                "files": a.files.len(),
            }),
            None => json!({
                "active": false,
                "id": null,
                "title": null,
                "base": null,
                "inline": inline_diff(),
                "files": 0,
            }),
        })
    })
}

// --- commands ----------------------------------------------------------------

fn complete_target(_editor: &Editor, input: &str) -> Vec<Completion> {
    TARGETS
        .iter()
        .filter(|t| t.starts_with(input))
        .map(|t| ((0..), Span::raw(*t)))
        .collect()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewEvent {
    target: String,
    path: Option<String>,
    doc_id: usize,
}

fn bb_review(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let Some(target) = args.first().map(str::to_owned) else {
        // fail at once when there is no review; the picker is built when
        // the job queue can reach the compositor
        if STATE.lock().is_none() {
            bail!("No bb review (:bb-review uncommitted, or ask bb for one)");
        }
        cx.jobs.callback(async move {
            let call = move |editor: &mut Editor, compositor: &mut Compositor| match picker(editor)
            {
                Ok(picker) => compositor.push(picker),
                Err(err) => editor.set_error(err.to_string()),
            };
            Ok(Callback::EditorCompositor(Box::new(call)))
        });
        return Ok(());
    };
    let client = events::wants_kind(Kind::Review)
        .then(crate::bridge::shared)
        .flatten();
    if target == "off" {
        // nothing to ask for: end it now, and tell the client
        tokio::spawn(async {
            if let Err(err) = end_review().await {
                log::warn!("bridge: ending the review: {}", err.message);
            }
        });
    }
    if let Some(shared) = client {
        let (view, doc) = current_ref!(cx.editor);
        let _ = view;
        broadcast(
            shared,
            Kind::Review,
            "event/review",
            ReviewEvent {
                target: target.clone(),
                path: path_string(doc.path()),
                doc_id: doc_num(doc.id()),
            },
            false,
        );
        cx.editor.set_status(match target.as_str() {
            "off" => "Review ended".to_owned(),
            _ => format!("Asked bb for the {target} review"),
        });
        return Ok(());
    }
    match target.as_str() {
        "off" => {
            cx.editor.set_status("Review ended");
            Ok(())
        }
        "uncommitted" => {
            let dir = match doc!(cx.editor).path() {
                Some(path) => path.to_path_buf(),
                None => helix_stdx::env::current_working_dir(),
            };
            let trust = trust_for(cx.editor, &dir);
            let providers = cx.editor.diff_providers.clone();
            cx.editor.set_status("Looking for uncommitted changes…");
            tokio::spawn(async move {
                let message = match uncommitted(dir, trust, providers).await {
                    Ok(0) => Err("No uncommitted changes".to_owned()),
                    Ok(n) => Ok(format!(
                        "Review: uncommitted changes, {n} file{} (:bb-review)",
                        if n == 1 { "" } else { "s" }
                    )),
                    Err(err) => Err(err.message),
                };
                let _ = on_editor(move |editor, _| match message {
                    Ok(msg) => editor.set_status(msg),
                    Err(msg) => editor.set_error(msg),
                })
                .await;
            });
            Ok(())
        }
        _ => bail!(":bb-review {target} needs bb (no client is listening)"),
    }
}

/// `:bb-review uncommitted` with no client: Helix's own `git status`
/// against `HEAD`, line counts included.
async fn uncommitted(
    dir: PathBuf,
    trust: bool,
    providers: helix_vcs::DiffProviderRegistry,
) -> Result<usize, RpcError> {
    let at = dir.clone();
    let opened = tokio::task::spawn_blocking(move || vcs::open(&at, None, trust))
        .await
        .map_err(|err| RpcError::new(ErrorCode::Unsupported, err.to_string()))?;
    let (repo, _) = opened.map_err(|err| unsupported("not a git repository", err.to_string()))?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    providers.for_each_changed_file(repo.workdir.clone(), trust, move |change| {
        if let Ok(change) = change {
            let _ = tx.send(change);
        }
        true
    });
    let mut files = Vec::new();
    // the sender goes away when the walk ends
    while let Some(change) = rx.recv().await {
        use helix_vcs::FileChange;
        let (path, status, old_path) = match change {
            FileChange::Untracked { path } => (path, Status::Added, None),
            FileChange::Modified { path } | FileChange::Conflict { path } => {
                (path, Status::Modified, None)
            }
            FileChange::Deleted { path } => (path, Status::Deleted, None),
            FileChange::Renamed { from_path, to_path } => {
                (to_path, Status::Renamed, Some(from_path))
            }
        };
        files.push(File {
            path,
            status,
            old_path,
            additions: 0,
            deletions: 0,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let count = start_review(
        NewReview {
            id: "uncommitted".into(),
            title: "Uncommitted changes".into(),
            base: None,
            files: files.clone(),
            inline: None,
        },
        repo.workdir.clone(),
        trust,
    )
    .await?;
    // the base is in place now: count the lines off the main thread
    let counted = tokio::task::spawn_blocking(move || {
        files
            .into_iter()
            .map(|mut f| {
                if let Some(diff) = file_diff(&f, None, trust) {
                    f.additions = diff.hunks.iter().map(|h| h.after.len() as u64).sum();
                    f.deletions = diff.hunks.iter().map(|h| h.before.len() as u64).sum();
                }
                f
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    if let Some(active) = STATE.lock().as_mut().filter(|a| a.id == "uncommitted") {
        if active.files.len() == counted.len() {
            active.files = counted;
        }
    }
    Ok(count)
}

fn bb_diff(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let on = !inline_diff();
    set_inline_diff(on);
    let against = STATE
        .lock()
        .as_ref()
        .map(|a| a.title.clone())
        .unwrap_or_else(|| "HEAD".to_owned());
    cx.editor.set_status(if on {
        format!("Inline diff on ({against})")
    } else {
        "Inline diff off".to_owned()
    });
    Ok(())
}

// --- a file's diff, for the preview and line counts -----------------------------

/// Lines of a file's diff, as the preview shows them.
struct FileDiff {
    base: Rope,
    text: Rope,
    hunks: Vec<Hunk>,
}

/// Off the main thread: the file's text at the base and now (`text` when
/// the buffer is open, else the file on disk), and their hunks.
fn file_diff(file: &File, text: Option<Rope>, trust: bool) -> Option<FileDiff> {
    let decode = |bytes: Vec<u8>| {
        helix_view::document::from_reader(&mut bytes.as_slice(), None)
            .ok()
            .map(|(rope, ..)| rope)
    };
    let base = match file.status {
        Status::Added => vcs::file_at_base(&file.path, trust)
            .ok()
            .and_then(decode)
            .unwrap_or_default(),
        _ => decode(vcs::file_at_base(&file.path, trust).ok()?)?,
    };
    let text = match (file.status, text) {
        (Status::Deleted, _) => Rope::new(),
        (_, Some(text)) => text,
        (_, None) => decode(std::fs::read(&file.path).ok()?)?,
    };
    let hunks = vcs::hunks(&base, &text);
    Some(FileDiff { base, text, hunks })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Header,
    Context,
    Added,
    Removed,
}

struct PreviewRow {
    mark: Mark,
    /// The line number in the new text (old text for removed lines).
    number: Option<usize>,
    text: String,
}

/// A unified diff with a few lines of context around each change.
fn preview_rows(diff: &FileDiff, tab_width: usize) -> Vec<PreviewRow> {
    let mut rows = Vec::new();
    let line = |text: &Rope, i: usize| display_text(text.line(i), tab_width);
    let text_lines = diff.text.len_lines();
    let mut hunks = diff.hunks.iter().peekable();
    while let Some(first) = hunks.next() {
        // hunks whose context touches are shown as one block
        let mut group = vec![first];
        while let Some(next) = hunks.peek() {
            let last = group[group.len() - 1];
            if next.after.start as usize <= last.after.end as usize + 2 * CONTEXT {
                group.push(hunks.next().unwrap());
            } else {
                break;
            }
        }
        let start = (group[0].after.start as usize).saturating_sub(CONTEXT);
        let old_start =
            (group[0].before.start as usize).saturating_sub(group[0].after.start as usize - start);
        let last = group[group.len() - 1];
        let end = (last.after.end as usize + CONTEXT).min(text_lines);
        rows.push(PreviewRow {
            mark: Mark::Header,
            number: None,
            text: format!("@@ -{} +{} @@", old_start + 1, start + 1),
        });
        let mut at = start;
        for hunk in group {
            for i in at..hunk.after.start as usize {
                rows.push(PreviewRow {
                    mark: Mark::Context,
                    number: Some(i + 1),
                    text: line(&diff.text, i),
                });
            }
            for i in hunk.before.start as usize..hunk.before.end as usize {
                rows.push(PreviewRow {
                    mark: Mark::Removed,
                    number: Some(i + 1),
                    text: line(&diff.base, i),
                });
            }
            for i in hunk.after.start as usize..hunk.after.end as usize {
                rows.push(PreviewRow {
                    mark: Mark::Added,
                    number: Some(i + 1),
                    text: line(&diff.text, i),
                });
            }
            at = hunk.after.end as usize;
        }
        for i in at..end {
            // the empty "line" after a final newline is not a line
            if i + 1 == text_lines && diff.text.line(i).len_chars() == 0 {
                break;
            }
            rows.push(PreviewRow {
                mark: Mark::Context,
                number: Some(i + 1),
                text: line(&diff.text, i),
            });
        }
        if rows.len() >= MAX_PREVIEW_ROWS {
            rows.truncate(MAX_PREVIEW_ROWS);
            break;
        }
    }
    rows
}

// --- the picker ------------------------------------------------------------------

#[derive(Clone)]
struct Row {
    index: usize,
    file: File,
    name: String,
}

/// Styles the picker's columns use.
struct PickerLook {
    plus: Style,
    minus: Style,
    delta: Style,
}

/// Previews by path, filled in by background tasks.
#[derive(Default)]
struct Previews {
    done: HashMap<PathBuf, Arc<Vec<PreviewRow>>>,
    pending: HashSet<PathBuf>,
}

/// Helix's picker over the review's files, with the file's diff drawn in
/// the preview pane (the picker itself only previews files).
struct ReviewPicker {
    picker: ui::Picker<Row, PickerLook>,
    previews: Arc<Mutex<Previews>>,
    show_preview: bool,
    trust: bool,
    tab_width: usize,
}

fn picker(editor: &Editor) -> anyhow::Result<Box<dyn Component>> {
    let (title, rows) = {
        let state = STATE.lock();
        let Some(active) = state.as_ref() else {
            bail!("No bb review (:bb-review uncommitted, or ask bb for one)");
        };
        let rows: Vec<Row> = active
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                let name = helix_stdx::path::get_relative_path(&file.path)
                    .to_string_lossy()
                    .into_owned();
                let name = match (&file.old_path, file.status) {
                    (Some(old), Status::Renamed) => format!(
                        "{} → {name}",
                        helix_stdx::path::get_relative_path(old).display()
                    ),
                    _ => name,
                };
                Row {
                    index,
                    file: file.clone(),
                    name,
                }
            })
            .collect();
        (active.title.clone(), rows)
    };
    if rows.is_empty() {
        bail!("The review '{title}' has no files");
    }
    let focused = doc!(editor).path().map(Path::to_path_buf);
    let initial = focused
        .and_then(|p| rows.iter().position(|r| r.file.path == p))
        .unwrap_or(0);
    let theme = &editor.theme;
    let fg = |scope: &str| Style {
        fg: theme.get(scope).fg,
        ..Style::default()
    };
    let look = PickerLook {
        plus: fg("diff.plus"),
        minus: fg("diff.minus"),
        delta: fg("diff.delta"),
    };
    let total = rows.len();
    let columns = [
        ui::PickerColumn::new("status", |row: &Row, look: &PickerLook| {
            let style = match row.file.status {
                Status::Added => look.plus,
                Status::Deleted => look.minus,
                Status::Modified | Status::Renamed => look.delta,
            };
            Span::styled(row.file.status.letter(), style).into()
        })
        .without_filtering(),
        ui::PickerColumn::new("changes", |row: &Row, look: &PickerLook| {
            Spans::from(vec![
                Span::styled(format!("+{}", row.file.additions), look.plus),
                Span::raw(" "),
                Span::styled(format!("-{}", row.file.deletions), look.minus),
            ])
            .into()
        })
        .without_filtering(),
        ui::PickerColumn::new("path", |row: &Row, _| row.name.as_str().into()),
    ];
    let picker = ui::Picker::new(columns, 2, rows, look, move |cx, row: &Row, action| {
        open_file(cx.editor, &row.file, row.index, total, action);
    })
    .with_initial_cursor(initial as u32)
    // reserves the preview pane; `ReviewPicker` draws the diff in it
    .with_preview(|_, _| None);
    let previews = Arc::new(Mutex::new(Previews::default()));
    let trust = rows_trust(editor);
    Ok(Box::new(overlaid(ReviewPicker {
        picker,
        previews,
        show_preview: true,
        trust,
        tab_width: doc!(editor).tab_width(),
    })))
}

fn rows_trust(editor: &Editor) -> bool {
    let workdir = STATE.lock().as_ref().map(|a| a.workdir.clone());
    workdir.is_some_and(|w| trust_for(editor, &w.join(".")))
}

impl ReviewPicker {
    /// The preview of the selected file, starting its computation if needed.
    fn preview(&mut self, editor: &Editor) -> Option<Arc<Vec<PreviewRow>>> {
        let row = self.picker.selection()?;
        let path = row.file.path.clone();
        let mut previews = self.previews.lock();
        if let Some(done) = previews.done.get(&path) {
            return Some(done.clone());
        }
        if !previews.pending.insert(path.clone()) {
            return None;
        }
        let file = row.file.clone();
        let text = editor.document_by_path(&path).map(|d| d.text().clone());
        let shared = self.previews.clone();
        let (trust, tab_width) = (self.trust, self.tab_width);
        tokio::task::spawn_blocking(move || {
            let rows = file_diff(&file, text, trust)
                .map(|diff| preview_rows(&diff, tab_width))
                .unwrap_or_default();
            let mut previews = shared.lock();
            previews.pending.remove(&file.path);
            previews.done.insert(file.path, Arc::new(rows));
            helix_event::request_redraw();
        });
        None
    }

    fn render_diff(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let inner = Block::bordered().inner(area).inner(Margin::horizontal(1));
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let theme = &cx.editor.theme;
        let text = theme.get("ui.text");
        let Some(rows) = self.preview(cx.editor) else {
            return;
        };
        if rows.is_empty() {
            let note = "No changes";
            let x = inner.x + inner.width.saturating_sub(note.len() as u16) / 2;
            surface.set_stringn(
                x,
                inner.y + inner.height / 2,
                note,
                inner.width as usize,
                text,
            );
            return;
        }
        let looks = DiffLooks::new(theme);
        let number_width = rows
            .iter()
            .filter_map(|r| r.number)
            .max()
            .unwrap_or(1)
            .to_string()
            .len();
        let numbers = theme.get("ui.linenr");
        let header = theme.get("comment");
        for (i, row) in rows.iter().take(inner.height as usize).enumerate() {
            let y = inner.y + i as u16;
            let (band, sign, style) = match row.mark {
                Mark::Header => (Style::default(), " ", header),
                Mark::Context => (Style::default(), " ", text),
                Mark::Added => (looks.plus_line, "+", text),
                Mark::Removed => (looks.removed_band, "-", looks.removed_text_plain),
            };
            surface.set_style(Rect::new(inner.x, y, inner.width, 1), band);
            let mut x = inner.x;
            let right = inner.right();
            if row.mark != Mark::Header {
                let n = row.number.map(|n| n.to_string()).unwrap_or_default();
                let label = format!("{n:>number_width$} ");
                (x, _) =
                    surface.set_stringn(x, y, &label, (right - x) as usize, numbers.patch(band));
                let sign_style = match row.mark {
                    Mark::Added => looks.plus_sign,
                    Mark::Removed => looks.minus_sign,
                    _ => numbers,
                };
                (x, _) =
                    surface.set_stringn(x, y, sign, (right - x) as usize, sign_style.patch(band));
                (x, _) = surface.set_stringn(x, y, " ", (right - x) as usize, band);
            }
            surface.set_stringn(x, y, &row.text, (right - x) as usize, style.patch(band));
        }
    }
}

impl Component for ReviewPicker {
    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if let Event::Key(KeyEvent {
            code: KeyCode::Char('t'),
            modifiers: KeyModifiers::CONTROL,
        }) = event
        {
            self.show_preview = !self.show_preview;
        }
        self.picker.handle_event(event, cx)
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        self.picker.render(area, surface, cx);
        // the picker's own layout: the right half is the preview
        if self.show_preview && area.width > ui::picker::MIN_AREA_WIDTH_FOR_PREVIEW {
            let preview = area.clip_left(area.width / 2);
            self.render_diff(preview, surface, cx);
        }
    }

    fn cursor(
        &self,
        area: Rect,
        editor: &Editor,
    ) -> (
        Option<helix_core::Position>,
        helix_view::graphics::CursorKind,
    ) {
        self.picker.cursor(area, editor)
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        self.picker.required_size(viewport)
    }

    fn id(&self) -> Option<&'static str> {
        self.picker.id()
    }
}

/// Open a file of the review at its first change (a deleted file: its text
/// at the base, in a read-only scratch buffer).
fn open_file(editor: &mut Editor, file: &File, index: usize, total: usize, action: Action) {
    let status = format!(
        "review {}/{total}: {} +{} -{}",
        index + 1,
        helix_stdx::path::get_relative_path(&file.path).display(),
        file.additions,
        file.deletions
    );
    if file.status == Status::Deleted {
        let path = file.path.clone();
        let trust = trust_for(editor, &path);
        tokio::spawn(async move {
            let at = path.clone();
            let text = tokio::task::spawn_blocking(move || vcs::file_at_base(&at, trust)).await;
            let _ = on_editor(move |editor, _| match text {
                Ok(Ok(bytes)) => {
                    open_deleted(editor, &path, &bytes, action);
                    editor.set_status(status);
                }
                Ok(Err(err)) => editor.set_error(format!("bb: {err:#}")),
                Err(err) => editor.set_error(format!("bb: {err}")),
            })
            .await;
        });
        return;
    }
    let target = JumpTarget {
        path: file.path.clone(),
        line: 1,
        col: None,
        end_line: None,
        status: Some(status),
    };
    if let Err(err) = jump(editor, &target, action) {
        editor.set_warning(format!("bb: {err}"));
        return;
    }
    let (view, doc) = current!(editor);
    let (view, doc) = (view.id, doc.id());
    if !goto_first_change(editor, doc) {
        // a file opened just now: its diff is still being computed
        let at = doc!(editor, &doc).selection(view).clone();
        tokio::spawn(async move {
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_millis(25)).await;
                let at = at.clone();
                let done = on_editor(move |editor, _| {
                    let still_here = editor.tree.contains(view)
                        && editor.tree.focus == view
                        && view!(editor, view).doc == doc
                        && *doc!(editor, &doc).selection(view) == at;
                    !still_here || goto_first_change(editor, doc)
                })
                .await
                .unwrap_or(true);
                if done {
                    break;
                }
            }
        });
    }
}

/// Select the first change of `doc` in the focused view, as `[g` would.
fn goto_first_change(editor: &mut Editor, doc_id: DocumentId) -> bool {
    let (view, doc) = current!(editor);
    if doc.id() != doc_id {
        return false;
    }
    let Some(hunk) = doc.diff_handle().map(|h| {
        let diff = h.load();
        (!diff.is_empty()).then(|| diff.nth_hunk(0))
    }) else {
        return false;
    };
    let Some(hunk) = hunk else {
        return false;
    };
    let text = doc.text().slice(..);
    let last = text.len_lines() - 1;
    let start = text.line_to_char((hunk.after.start as usize).min(last));
    let selection = if hunk.after.is_empty() {
        Selection::point(start)
    } else {
        Selection::single(
            start,
            text.line_to_char(
                (hunk.after.end as usize)
                    .min(last + 1)
                    .min(text.len_lines()),
            ),
        )
    };
    doc.set_selection(view.id, selection);
    align_view(doc, view, Align::Center);
    true
}

/// A read-only scratch buffer named `<path> (deleted)` with the file's text
/// at the base, highlighted as the file was.
fn open_deleted(editor: &mut Editor, path: &Path, bytes: &[u8], action: Action) {
    let name = PathBuf::from(format!("{} (deleted)", path.display()));
    if let Some(id) = editor.document_id_by_path(&name) {
        editor.switch(id, action);
        return;
    }
    let Ok((text, ..)) = helix_view::document::from_reader(&mut &bytes[..], None) else {
        editor.set_error(format!("bb: cannot decode {}", path.display()));
        return;
    };
    editor.new_file(action);
    let loader = editor.syn_loader.load();
    let (view, doc) = current!(editor);
    // a new scratch buffer holds a line ending of its own
    let all = doc.text().len_chars();
    let replace = Transaction::change(
        doc.text(),
        [(0, all, Some(text.to_string().into()))].into_iter(),
    );
    doc.apply(&replace, view.id);
    doc.append_changes_to_history(view);
    doc.reset_modified();
    doc.set_path(Some(path));
    let language = doc.detect_language_config(&loader);
    doc.set_path(Some(&name));
    doc.set_language(language, &loader);
    doc.readonly = true;
    doc.set_selection(view.id, Selection::point(0));
}

// --- drawing -------------------------------------------------------------------

/// How strongly the diff colours tint the background, out of 1.
const ADDED_TINT: f32 = 0.14;
const CHANGED_TINT: f32 = 0.12;
const REMOVED_TINT: f32 = 0.16;

struct DiffLooks {
    plus_line: Style,
    delta_line: Style,
    removed_band: Style,
    /// Removed text, struck through (or dim when the theme has no
    /// strikethrough).
    removed_text: Style,
    /// Removed text in the picker's preview, which is not mistaken for code.
    removed_text_plain: Style,
    minus_sign: Style,
    plus_sign: Style,
    /// The soft-wrap indicator, as Helix draws it.
    wrap: Style,
}

impl DiffLooks {
    fn new(theme: &Theme) -> DiffLooks {
        let bg = theme.get("ui.background").bg;
        let tint = |scope: &str, amount: f32| {
            blend(theme.get(scope).fg, bg, amount)
                .map(|c| Style::default().bg(c))
                .unwrap_or_default()
        };
        let minus = Style {
            fg: theme.get("diff.minus").fg,
            ..Style::default()
        };
        let strike = theme
            .try_get("markup.strikethrough")
            .is_some_and(|s| s.add_modifier.contains(Modifier::CROSSED_OUT));
        let removed_text = if strike {
            minus.add_modifier(Modifier::CROSSED_OUT)
        } else {
            minus.add_modifier(Modifier::DIM)
        };
        let gutter = |scope: &str, fallback: &str| {
            theme.try_get(scope).unwrap_or_else(|| theme.get(fallback))
        };
        DiffLooks {
            plus_line: tint("diff.plus", ADDED_TINT),
            delta_line: tint("diff.delta", CHANGED_TINT),
            removed_band: tint("diff.minus", REMOVED_TINT),
            removed_text,
            removed_text_plain: minus,
            minus_sign: gutter("diff.minus.gutter", "diff.minus"),
            plus_sign: gutter("diff.plus.gutter", "diff.plus"),
            wrap: gutter("ui.virtual.wrap", "ui.virtual"),
        }
    }
}

/// The snapshot to draw for `doc`, when the inline diff is on and Helix
/// diffs the document.
fn snapshot(doc: &Document) -> Option<DiffSnapshot> {
    if !inline_diff() || doc.diff_handle().is_none() {
        return None;
    }
    let snapshot = doc.bridge_annotations.diff_snapshot();
    (!snapshot.hunks.is_empty()).then_some(snapshot)
}

/// Column (absolute x) of Helix's diff gutter in `view`, if it shows one.
fn diff_gutter_x(view: &View, doc: &Document) -> Option<u16> {
    if view.gutter_offset(doc) == 0 {
        return None;
    }
    let mut x = view.area.x;
    for gutter in view.gutters() {
        if *gutter == GutterType::Diff {
            return Some(x);
        }
        x += gutter.width(view, doc) as u16;
    }
    None
}

/// Backgrounds of added and changed lines. Added before the annotations,
/// so a highlighted comment's lines keep their own background.
struct DiffLines<'a> {
    text: RopeSlice<'a>,
    hunks: Arc<[Hunk]>,
    hunk: usize,
    plus: Style,
    delta: Style,
}

impl Decoration for DiffLines<'_> {
    fn reset_pos(&mut self, pos: usize) -> usize {
        let line = self.text.char_to_line(pos.min(self.text.len_chars()));
        self.hunk = self.hunks.partition_point(|h| h.after.end as usize <= line);
        usize::MAX
    }

    fn decorate_line(&mut self, renderer: &mut TextRenderer, pos: LinePos) {
        let line = pos.doc_line;
        while self
            .hunks
            .get(self.hunk)
            .is_some_and(|h| (h.after.end as usize) <= line)
        {
            self.hunk += 1;
        }
        let Some(hunk) = self.hunks.get(self.hunk) else {
            return;
        };
        if (hunk.after.start as usize) > line {
            return;
        }
        let style = if hunk.before.is_empty() {
            self.plus
        } else {
            self.delta
        };
        let viewport = renderer.viewport;
        renderer.set_style(
            Rect::new(viewport.x, pos.visual_line, viewport.width, 1),
            style,
        );
    }
}

/// Removed lines, in the virtual lines `DiffRows` (helix-view) reserved
/// above each change. Added after the annotations, as those rows are.
struct DiffRemoved<'a> {
    text: RopeSlice<'a>,
    snapshot: DiffSnapshot,
    hunk: usize,
    /// Hunks whose rows go under the line that just ended.
    pending: Option<Range<usize>>,
    looks: DiffLooks,
    tab_width: usize,
    wrap: Option<RemovedWrap>,
    horizontal_offset: usize,
    gutter_x: Option<u16>,
}

impl DiffRemoved<'_> {
    /// Char index of the line ending under which the next rows go.
    fn next_anchor(&self) -> usize {
        match self.snapshot.hunks.get(self.hunk) {
            Some(hunk) => {
                let line = DiffSnapshot::anchor(hunk);
                if line >= self.text.len_lines() {
                    usize::MAX
                } else {
                    line_end_char_index(&self.text, line)
                }
            }
            None => usize::MAX,
        }
    }

    fn row(&self, renderer: &mut TextRenderer, row: usize, text: &str, style: Style) {
        self.wrapped_row(renderer, row, 0, None, text, style);
    }

    /// A row with `indent` blank columns, then `indicator`, then `text`.
    fn wrapped_row(
        &self,
        renderer: &mut TextRenderer,
        row: usize,
        indent: usize,
        indicator: Option<&str>,
        text: &str,
        style: Style,
    ) {
        let y = row as u16;
        let viewport = renderer.viewport;
        let band = self.looks.removed_band;
        renderer.set_style(Rect::new(viewport.x, y, viewport.width, 1), band);
        if let Some(x) = self.gutter_x {
            renderer.set_string(x, y, "▍", self.looks.minus_sign);
        }
        let right = viewport.right() as usize;
        let mut x = (viewport.x as usize + indent).min(right);
        if let Some(indicator) = indicator {
            renderer.set_stringn(
                x as u16,
                y,
                indicator,
                right - x,
                self.looks.wrap.patch(band),
            );
            x = (x + indicator.width()).min(right);
        }
        renderer.set_stringn(x as u16, y, text, right - x, style.patch(band));
    }

    fn draw_hunk(&self, renderer: &mut TextRenderer, hunk: &Hunk, first_row: usize) -> usize {
        let (lines, hidden) = self.snapshot.removed_lines(hunk);
        let mut row = first_row;
        let (top, height) = (renderer.offset.row, renderer.viewport.height as usize);
        let visible = |row: usize| row >= top && row - top < height;
        for line in lines {
            let text = display_text(self.snapshot.base.line(line), self.tab_width);
            match &self.wrap {
                None => {
                    if visible(row) {
                        let shown = skip_columns(&text, self.horizontal_offset);
                        self.row(renderer, row, shown, self.looks.removed_text);
                    }
                    row += 1;
                }
                Some(wrap) => {
                    for part in removed_layout(&text, wrap) {
                        if visible(row) {
                            let indicator = part.indicator.then_some(wrap.indicator.as_str());
                            let shown = &text[part.text];
                            let style = self.looks.removed_text;
                            self.wrapped_row(renderer, row, part.indent, indicator, shown, style);
                        }
                        row += 1;
                    }
                }
            }
        }
        if hidden > 0 {
            if visible(row) {
                let note = format!(
                    "⋯ {hidden} more removed line{}",
                    if hidden == 1 { "" } else { "s" }
                );
                self.row(renderer, row, &note, self.looks.removed_text_plain);
            }
            row += 1;
        }
        row - first_row
    }
}

/// `text` without its first `cols` columns.
fn skip_columns(text: &str, cols: usize) -> &str {
    let mut skip = cols;
    let mut start = 0;
    for (i, g) in text.grapheme_indices(true) {
        if skip == 0 {
            break;
        }
        skip = skip.saturating_sub(g.width());
        start = i + g.len();
    }
    if skip > 0 {
        return "";
    }
    &text[start..]
}

impl Decoration for DiffRemoved<'_> {
    fn reset_pos(&mut self, pos: usize) -> usize {
        let line = self.text.char_to_line(pos.min(self.text.len_chars()));
        self.hunk = self.snapshot.first_anchored(line);
        self.pending = None;
        self.next_anchor()
    }

    fn decorate_grapheme(
        &mut self,
        _renderer: &mut TextRenderer,
        _grapheme: &FormattedGrapheme,
    ) -> usize {
        let Some(hunk) = self.snapshot.hunks.get(self.hunk) else {
            return usize::MAX;
        };
        let line = DiffSnapshot::anchor(hunk);
        let start = self.hunk;
        while self
            .snapshot
            .hunks
            .get(self.hunk)
            .is_some_and(|h| DiffSnapshot::anchor(h) == line)
        {
            self.hunk += 1;
        }
        // Helix lays out no virtual lines after the last line
        self.pending = (line + 1 < self.text.len_lines()).then_some(start..self.hunk);
        self.next_anchor()
    }

    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        let Some(hunks) = self.pending.take() else {
            return Position::new(0, 0);
        };
        let start = pos.visual_line as usize + virt_off.row;
        let mut row = start;
        for i in hunks {
            let hunk = self.snapshot.hunks[i].clone();
            if !hunk.before.is_empty() {
                row += self.draw_hunk(renderer, &hunk, row);
            }
        }
        Position::new(row - start, 0)
    }
}

/// Line backgrounds, before the annotations draw. Returns the snapshot for
/// [`decorate_removed`].
pub(super) fn decorate_lines<'d>(
    doc: &'d Document,
    theme: &Theme,
    decorations: &mut DecorationManager<'d>,
) -> Option<DiffSnapshot> {
    let snapshot = snapshot(doc)?;
    let looks = DiffLooks::new(theme);
    decorations.add_decoration(DiffLines {
        text: doc.text().slice(..),
        hunks: snapshot.hunks.clone(),
        hunk: 0,
        plus: looks.plus_line,
        delta: looks.delta_line,
    });
    Some(snapshot)
}

/// Removed lines, after the annotations draw.
pub(super) fn decorate_removed<'d>(
    doc: &'d Document,
    view: &View,
    theme: &Theme,
    snapshot: DiffSnapshot,
    decorations: &mut DecorationManager<'d>,
) {
    if snapshot.hunks.iter().all(|h| h.before.is_empty()) {
        return;
    }
    let wrap = removed_wrap(doc, view.inner_width(doc));
    decorations.add_decoration(DiffRemoved {
        text: doc.text().slice(..),
        snapshot,
        hunk: 0,
        pending: None,
        looks: DiffLooks::new(theme),
        tab_width: doc.tab_width(),
        wrap,
        horizontal_offset: doc.view_offset(view.id).horizontal_offset,
        gutter_x: diff_gutter_x(view, doc),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns() {
        assert_eq!(skip_columns("abcdef", 2), "cdef");
        assert_eq!(skip_columns("ab", 5), "");
        assert_eq!(skip_columns("日本語", 2), "本語");
    }

    #[test]
    fn preview_shows_context_and_changes() {
        let base = Rope::from("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n");
        let text = Rope::from("a\nb\nC\nd\ne\nf\ng\nh\ni\nj\nk\n");
        let hunks = vcs::hunks(&base, &text);
        let diff = FileDiff { base, text, hunks };
        let rows = preview_rows(&diff, 4);
        let marks: Vec<_> = rows.iter().map(|r| (r.mark, r.text.as_str())).collect();
        assert_eq!(marks[0].0, Mark::Header);
        assert!(marks.contains(&(Mark::Removed, "c")));
        assert!(marks.contains(&(Mark::Added, "C")));
        assert!(marks.contains(&(Mark::Added, "k")));
        // 7 unchanged lines apart: more than twice the context, two blocks
        assert_eq!(rows.iter().filter(|r| r.mark == Mark::Header).count(), 2);
        let close = Rope::from("a\nb\nC\nd\ne\nf\ng\nH\ni\nj\n");
        let base = Rope::from("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n");
        let hunks = vcs::hunks(&base, &close);
        let rows = preview_rows(
            &FileDiff {
                base,
                text: close,
                hunks,
            },
            4,
        );
        // 4 lines apart: one block
        assert_eq!(rows.iter().filter(|r| r.mark == Mark::Header).count(), 1);
    }

    #[test]
    fn targets_complete() {
        let words = |input: &str| -> Vec<String> {
            TARGETS
                .iter()
                .filter(|t| t.starts_with(input))
                .map(|t| t.to_string())
                .collect()
        };
        assert_eq!(words("u"), ["uncommitted"]);
        assert_eq!(words("").len(), TARGETS.len());
        assert!(words("x").is_empty());
    }
}
