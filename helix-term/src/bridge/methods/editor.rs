//! bridge/hello, editor/state, editor/open, editor/text, editor/selection,
//! editor/status.
//!
//! Closures passed to `on_editor` only copy what they need (an O(1) `Rope`
//! clone, a `Selection`); conversion and text extraction happen afterwards,
//! off the main thread.

use std::path::Path;

use futures_util::future::BoxFuture;
use helix_core::{Rope, Selection};
use helix_view::{align_view, editor::Action, Align, Document, DocumentId, Editor};
use serde_json::Value;
use slotmap::{Key, KeyData};

use crate::bridge::convert::{
    char_after_line, char_from_pos, doc_num, focus_view_of, open_path, path_string, resolve_path,
    selection_to_proto, selection_with_text, text_in_range,
};
use crate::bridge::dispatch::{on_editor, CAPABILITIES};
use crate::bridge::events::{mode_name, DocMeta};
use crate::bridge::methods::command;
use crate::bridge::methods::interrupt::{self, Interrupt};
use crate::bridge::protocol::{
    parse_params, DocumentInfo, ErrorCode, FocusedInfo, HelloParams, HelloResult, OpenParams,
    OpenResult, Position, RpcError, RpcResult, SelectionResult, Severity, Split, StateResult,
    StatusParams, TextParams, TextResult, Visible, MAX_TEXT, PROTOCOL_VERSION,
};
use crate::bridge::server::Ctx;

pub const CAPABILITY: Option<&str> = Some("editor");

/// Most list items one response carries.
const MAX_ITEMS: usize = 500;

/// Files larger than this are not read from disk for `editor/text`.
const MAX_DISK_READ: u64 = 64 * 1024 * 1024;

pub fn view_num(view: helix_view::ViewId) -> u64 {
    view.data().as_ffi()
}

pub fn view_from_num(n: u64) -> helix_view::ViewId {
    KeyData::from_ffi(n).into()
}

fn command_policy(editor: &Editor) -> &'static str {
    use helix_view::editor::BridgeCommandPolicy as P;
    match command::policy(editor) {
        P::Safe => "safe",
        P::All => "all",
        P::None => "none",
    }
}

pub fn hello(cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: HelloParams = parse_params(params)?;
        let name = match (params.client, params.client_version) {
            (Some(c), Some(v)) => Some(format!("{c} {v}")),
            (Some(c), None) => Some(c),
            (None, _) => None,
        };
        *cx.client.name.lock() = name;
        let policy = on_editor(|editor, _| command_policy(editor)).await?;
        let info = &cx.shared.info;
        let result = HelloResult {
            protocol: PROTOCOL_VERSION,
            helix_version: helix_loader::VERSION_AND_GIT_HASH,
            pid: info.pid,
            cwd: helix_stdx::env::current_working_dir()
                .to_string_lossy()
                .into_owned(),
            workspace_root: info.workspace_root.to_string_lossy().into_owned(),
            capabilities: CAPABILITIES
                .iter()
                .flatten()
                .copied()
                // listed only while Helix is watching (`[editor.bridge] watch`)
                .filter(|c| *c != "watch" || super::watch::running())
                .collect(),
        };
        let mut value = serde_json::to_value(result).unwrap_or_default();
        value["commandPolicy"] = policy.into();
        Ok(value)
    })
}

fn document_info(meta: DocMeta) -> DocumentInfo {
    DocumentInfo {
        doc_id: doc_num(meta.doc),
        path: path_string(meta.path.as_deref()),
        language_id: meta.language,
        version: meta.version,
        modified: meta.modified,
    }
}

/// The focused view's document and selection, copied on the main thread.
struct Focused {
    meta: DocMeta,
    text: Rope,
    selection: Selection,
    first_line: usize,
    last_line: usize,
}

fn focused(editor: &Editor) -> Focused {
    let (view, doc) = current_ref!(editor);
    let text = doc.text().clone();
    let slice = text.slice(..);
    let anchor = doc.view_offset(view.id).anchor.min(slice.len_chars());
    Focused {
        meta: DocMeta::of(doc),
        selection: doc.selection(view.id).clone(),
        first_line: slice.char_to_line(anchor) + 1,
        last_line: view.estimate_last_doc_line(doc) + 1,
        text,
    }
}

pub fn state(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let (mode, focused, documents) = on_editor(|editor, _| {
            let documents: Vec<DocMeta> = editor.documents().map(DocMeta::of).collect();
            (mode_name(editor.mode()), focused(editor), documents)
        })
        .await?;
        let truncated = documents.len() > MAX_ITEMS;
        let mut documents: Vec<DocumentInfo> = documents.into_iter().map(document_info).collect();
        documents.sort_by_key(|d| d.doc_id);
        documents.truncate(MAX_ITEMS);
        let primary = focused.selection.primary_index();
        let selections = selection_to_proto(focused.text.slice(..), &focused.selection);
        let meta = focused.meta;
        let result = StateResult {
            mode,
            focused: FocusedInfo {
                doc_id: doc_num(meta.doc),
                path: path_string(meta.path.as_deref()),
                language_id: meta.language,
                version: meta.version,
                modified: meta.modified,
                selections,
                primary,
                visible: Visible {
                    first_line: focused.first_line,
                    last_line: focused.last_line,
                },
            },
            documents,
        };
        let mut value = serde_json::to_value(result).unwrap_or_default();
        if truncated {
            value["truncated"] = true.into();
        }
        Ok(value)
    })
}

pub fn open(cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: OpenParams = parse_params(params)?;
        let interrupt = Interrupt::parse(params.interrupt.as_deref())?;
        let path = resolve_path(&params.path, &cx.shared.info.workspace_root);
        let on_disk = std::fs::metadata(&path);
        if matches!(&on_disk, Ok(m) if m.is_dir()) {
            return Err(RpcError::not_found(format!(
                "{} is a directory",
                path.display()
            )));
        }
        let exists = on_disk.is_ok();
        on_editor(move |editor, compositor| {
            interrupt::open(editor, compositor, interrupt, path, exists, params)
        })
        .await?
    })
}

pub(crate) fn open_on_main(
    editor: &mut Editor,
    path: &Path,
    exists: bool,
    params: OpenParams,
) -> Result<OpenResult, RpcError> {
    let path = &open_path(editor, path);
    if !exists && editor.document_id_by_path(path).is_none() {
        return Err(RpcError::not_found(format!("{} not found", path.display())));
    }
    if params.focus {
        if let Some(view) = params.view_id {
            let view = view_from_num(view);
            if !editor.tree.contains(view) {
                return Err(RpcError::not_found(format!(
                    "no view {}",
                    params.view_id.unwrap_or(0)
                )));
            }
            editor.focus(view);
        } else if params.split == Split::None {
            // a pane that already shows the file takes the focus
            if let Some(doc) = editor.document_id_by_path(path) {
                focus_view_of(editor, doc);
            }
        }
    }
    let action = match (params.focus, params.split) {
        (false, _) => Action::Load,
        (true, Split::None) => Action::Replace,
        (true, Split::Horizontal) => Action::HorizontalSplit,
        (true, Split::Vertical) => Action::VerticalSplit,
    };
    let doc_id = editor
        .open(path, action)
        .map_err(|err| RpcError::not_found(format!("{}: {err}", path.display())))?;
    // With `Load` the document gets the focused view's selection slot but
    // stays out of sight.
    let view_id = editor.tree.focus;
    if let Some(line) = params.line {
        let doc = doc_mut!(editor, &doc_id);
        let text = doc.text().slice(..);
        let start = char_from_pos(
            text,
            Position {
                line,
                col: params.col.unwrap_or(1),
            },
        );
        let selection = match params.end_line {
            Some(end_line) => {
                let end = match params.end_col {
                    Some(col) => char_from_pos(
                        text,
                        Position {
                            line: end_line,
                            col,
                        },
                    ),
                    None => char_after_line(text, end_line),
                };
                Selection::single(start, end.max(start))
            }
            None => Selection::point(start),
        };
        doc.set_selection(view_id, selection);
        if params.focus {
            let view = view_mut!(editor, view_id);
            let doc = doc_mut!(editor, &doc_id);
            align_view(doc, view, Align::Center);
        }
    }
    if let Some(note) = params.note.filter(|n| !n.is_empty()) {
        editor.set_status(note);
    }
    let doc = doc!(editor, &doc_id);
    Ok(OpenResult {
        doc_id: doc_num(doc_id),
        path: path_string(doc.path()).unwrap_or_default(),
        version: doc.version(),
        view_id: params.focus.then(|| view_num(view_id)),
        deferred: false,
    })
}

fn find_doc(editor: &Editor, doc_id: Option<usize>, path: Option<&Path>) -> Option<DocumentId> {
    match (doc_id, path) {
        (Some(n), _) => editor
            .documents()
            .map(Document::id)
            .find(|id| doc_num(*id) == n),
        (None, Some(path)) => editor.document_id_by_path(&open_path(editor, path)),
        (None, None) => None,
    }
}

pub fn text(cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: TextParams = parse_params(params)?;
        if params.path.is_none() && params.doc_id.is_none() {
            return Err(RpcError::invalid_params(
                "path",
                "path or docId is required",
            ));
        }
        let path = params
            .path
            .as_deref()
            .map(|p| resolve_path(p, &cx.shared.info.workspace_root));
        let lookup_path = path.clone();
        let doc_id = params.doc_id;
        let open = on_editor(move |editor, _| {
            let id = find_doc(editor, doc_id, lookup_path.as_deref())?;
            let doc = editor.document(id)?;
            Some((doc.text().clone(), DocMeta::of(doc)))
        })
        .await?;
        let range = params.range;
        let result = match open {
            Some((text, meta)) => {
                let (text, truncated) = text_in_range(&text, range, MAX_TEXT);
                TextResult {
                    path: path_string(meta.path.as_deref()),
                    version: Some(meta.version),
                    modified: meta.modified,
                    text,
                    truncated,
                }
            }
            None => {
                let Some(path) = path.filter(|_| doc_id.is_none()) else {
                    return Err(RpcError::not_found("no document with that docId"));
                };
                let text = read_from_disk(&path).await?;
                let (text, truncated) = text_in_range(&text, range, MAX_TEXT);
                TextResult {
                    path: Some(path.to_string_lossy().into_owned()),
                    version: None,
                    modified: false,
                    text,
                    truncated,
                }
            }
        };
        Ok(serde_json::to_value(result).unwrap_or_default())
    })
}

async fn read_from_disk(path: &Path) -> Result<Rope, RpcError> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|err| RpcError::not_found(format!("{}: {err}", path.display())))?;
    if meta.is_dir() {
        return Err(RpcError::not_found(format!(
            "{} is a directory",
            path.display()
        )));
    }
    if meta.len() > MAX_DISK_READ {
        return Err(RpcError::new(
            ErrorCode::TooLarge,
            format!("{} is larger than 64 MiB", path.display()),
        ));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|err| RpcError::not_found(format!("{}: {err}", path.display())))?;
    Ok(Rope::from(String::from_utf8_lossy(&bytes).as_ref()))
}

pub fn selection(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let focused = on_editor(|editor, _| focused(editor)).await?;
        let (ranges, truncated) =
            selection_with_text(focused.text.slice(..), &focused.selection, MAX_TEXT);
        let meta = focused.meta;
        let result = SelectionResult {
            path: path_string(meta.path.as_deref()),
            doc_id: doc_num(meta.doc),
            language_id: meta.language,
            version: meta.version,
            modified: meta.modified,
            primary: focused.selection.primary_index(),
            ranges,
            truncated,
        };
        Ok(serde_json::to_value(result).unwrap_or_default())
    })
}

pub fn status(_cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: StatusParams = parse_params(params)?;
        on_editor(move |editor, _| match params.severity {
            Severity::Info => editor.set_status(params.message),
            Severity::Warning => editor.set_warning(params.message),
            Severity::Error => editor.set_error(params.message),
        })
        .await?;
        Ok(serde_json::json!({}))
    })
}
