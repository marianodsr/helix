//! editor/applyEdits (step 2a).
//!
//! All edits become one `Transaction` against the text as it is when the
//! request reaches the main thread, applied like any other change: the
//! change hooks notify the language server and the bridge's own events,
//! selections in every view are mapped through it, and it is committed to
//! history as one revision of its own, so a single `u` restores the buffer
//! exactly. Pending changes (the user in insert mode) are committed first,
//! so the user's own typing and the edit never share an undo step.

use std::path::{Path, PathBuf};

use futures_util::future::BoxFuture;
use helix_core::{Tendril, Transaction};
use helix_view::{editor::Action, DocumentId, Editor, ViewId};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::bridge::convert::{char_from_pos, open_path, path_string, resolve_path};
use crate::bridge::dispatch::{on_editor, EDITOR_TIMEOUT};
use crate::bridge::protocol::{parse_params, ErrorCode, Range, RpcError, RpcResult};
use crate::bridge::server::Ctx;

pub const CAPABILITY: Option<&str> = Some("applyEdits");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApplyEditsParams {
    path: String,
    version: Option<i32>,
    edits: Vec<Edit>,
    label: Option<String>,
    #[serde(default)]
    save: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Edit {
    range: Range,
    new_text: String,
}

#[derive(Debug, Serialize)]
struct ApplyEditsResult {
    path: String,
    version: i32,
    modified: bool,
}

/// What the main thread hands back: the result so far, and the save to wait
/// for when one was started.
struct Applied {
    doc: DocumentId,
    result: ApplyEditsResult,
    save: Option<oneshot::Receiver<Result<usize, String>>>,
}

pub fn apply_edits(cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: ApplyEditsParams = parse_params(params)?;
        let path = resolve_path(&params.path, &cx.shared.info.workspace_root);
        let on_disk = tokio::fs::metadata(&path).await;
        if matches!(&on_disk, Ok(m) if m.is_dir()) {
            return Err(RpcError::not_found(format!(
                "{} is a directory",
                path.display()
            )));
        }
        let exists = on_disk.is_ok();
        let applied =
            on_editor(move |editor, _| apply_on_main(editor, &path, exists, params)).await??;
        let Applied {
            doc,
            mut result,
            save,
        } = applied;
        if let Some(save) = save {
            let saved = tokio::time::timeout(EDITOR_TIMEOUT, save).await;
            let revision = match saved {
                Ok(Ok(Ok(revision))) => revision,
                Ok(Ok(Err(err))) => return Err(save_failed(&result, err)),
                Ok(Err(_)) => return Err(save_failed(&result, "the save was dropped".into())),
                Err(_) => {
                    return Err(RpcError {
                        code: ErrorCode::Timeout,
                        message: "the edits were applied; the save did not finish in time".into(),
                        data: Some(json!({ "applied": true, "version": result.version })),
                    })
                }
            };
            // Helix records the save right after the write finishes; read the
            // state the buffer is in relative to the revision written.
            result.modified = on_editor(move |editor, _| {
                editor
                    .document_mut(doc)
                    .is_some_and(|d| d.get_current_revision() != revision)
            })
            .await?;
        }
        Ok(serde_json::to_value(result).unwrap_or_default())
    })
}

fn save_failed(result: &ApplyEditsResult, err: String) -> RpcError {
    RpcError {
        code: ErrorCode::Unsupported,
        message: format!("the edits were applied but saving failed: {err}"),
        data: Some(json!({
            "reason": "save failed",
            "applied": true,
            "version": result.version,
        })),
    }
}

/// Edits as char offsets in the current text, sorted, checked for overlap.
fn to_changes(
    text: helix_core::RopeSlice,
    edits: Vec<Edit>,
) -> Result<Vec<(usize, usize, Option<Tendril>)>, RpcError> {
    let mut changes = Vec::with_capacity(edits.len());
    for (i, edit) in edits.into_iter().enumerate() {
        let (start, end) = (edit.range.start, edit.range.end);
        if (start.line, start.col) > (end.line, end.col) {
            return Err(RpcError::invalid_params(
                "edits",
                format!("edit {i}: range ends before it starts"),
            ));
        }
        let from = char_from_pos(text, start);
        let to = char_from_pos(text, end).max(from);
        let insert = (!edit.new_text.is_empty()).then(|| Tendril::from(edit.new_text.as_str()));
        changes.push((i, from, to, insert));
    }
    // stable: insertions at one point keep the order they were given in,
    // and come before a replacement starting there
    changes.sort_by_key(|&(_, from, to, _)| (from, to));
    for pair in changes.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if b.1 < a.2 {
            return Err(RpcError::invalid_params(
                "edits",
                format!("edits {} and {} overlap", a.0.min(b.0), a.0.max(b.0)),
            ));
        }
    }
    Ok(changes
        .into_iter()
        .map(|(_, from, to, insert)| (from, to, insert))
        .collect())
}

/// The view an edit is applied through: the focused one if it shows the
/// document, else any that does, else the focused one (a background
/// document already has a selection there from `Action::Load`).
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

fn apply_on_main(
    editor: &mut Editor,
    path: &Path,
    exists: bool,
    params: ApplyEditsParams,
) -> Result<Applied, RpcError> {
    let path = &open_path(editor, path);
    let doc_id = match editor.document_id_by_path(path) {
        Some(id) => id,
        None if exists => editor
            .open(path, Action::Load)
            .map_err(|err| RpcError::not_found(format!("{}: {err}", path.display())))?,
        None => return Err(RpcError::not_found(format!("{} not found", path.display()))),
    };
    let doc = doc!(editor, &doc_id);
    if let Some(version) = params.version {
        if version != doc.version() {
            return Err(RpcError {
                code: ErrorCode::VersionMismatch,
                message: format!("the buffer is at version {}, not {version}", doc.version()),
                data: Some(json!({ "version": doc.version() })),
            });
        }
    }
    let changes = to_changes(doc.text().slice(..), params.edits)?;
    if params.save {
        check_disk(doc, path)?;
    }

    let view_id = view_for(editor, doc_id);
    let view = view_mut!(editor, view_id);
    let doc = doc_mut!(editor, &doc_id);
    if !changes.is_empty() {
        let transaction = Transaction::change(doc.text(), changes.into_iter());
        // keep the user's pending typing out of this undo step
        doc.append_changes_to_history(view);
        doc.apply(&transaction, view.id);
        doc.append_changes_to_history(view);
    }
    if let Some(label) = params.label.filter(|l| !l.is_empty()) {
        editor.set_status(label);
    }
    let save = if params.save {
        Some(start_save(editor, doc_id).map_err(|err| RpcError {
            code: ErrorCode::Unsupported,
            message: format!("the edits were applied but saving failed: {err}"),
            data: Some(json!({ "reason": "save failed", "applied": true })),
        })?)
    } else {
        None
    };
    let doc = doc!(editor, &doc_id);
    Ok(Applied {
        doc: doc_id,
        result: ApplyEditsResult {
            path: path_string(doc.path()).unwrap_or_default(),
            version: doc.version(),
            modified: doc.is_modified(),
        },
        save,
    })
}

/// Helix refuses to save over a file changed on disk since it read it; find
/// that out before applying anything.
fn check_disk(doc: &helix_view::Document, path: &Path) -> Result<(), RpcError> {
    let Ok(mtime) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        return Ok(());
    };
    if doc.last_saved_time() < mtime {
        return Err(RpcError {
            code: ErrorCode::Unsupported,
            message: format!(
                "{} changed on disk since Helix read it; reload it first",
                path.display()
            ),
            data: Some(json!({ "reason": "changed on disk" })),
        });
    }
    Ok(())
}

/// `Editor::save`, plus a channel that says how the write ended. The save
/// goes through the editor's own save queue, so Helix records it, notifies
/// the language server and reports it exactly as for `:w`.
fn start_save(
    editor: &mut Editor,
    doc_id: DocumentId,
) -> anyhow::Result<oneshot::Receiver<Result<usize, String>>> {
    use futures_util::stream;
    let doc = doc_mut!(editor, &doc_id);
    let future = doc.save(None::<PathBuf>, false)?;
    let handler = editor.language_servers.file_event_handler.clone();
    let (tx, rx) = oneshot::channel();
    let future = async move {
        let res = future.await;
        if let Ok(event) = &res {
            handler.file_changed(event.path.clone());
        }
        let _ = tx.send(match &res {
            Ok(event) => Ok(event.revision),
            Err(err) => Err(err.to_string()),
        });
        res
    };
    editor
        .saves
        .get(&doc_id)
        .ok_or_else(|| anyhow::anyhow!("saves are closed for this document"))?
        .send(stream::once(Box::pin(future)))
        .map_err(|err| anyhow::anyhow!("failed to send save event: {err}"))?;
    editor.write_count += 1;
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::protocol::Position;
    use helix_core::Rope;

    fn edit(sl: usize, sc: usize, el: usize, ec: usize, text: &str) -> Edit {
        Edit {
            range: Range {
                start: Position { line: sl, col: sc },
                end: Position { line: el, col: ec },
            },
            new_text: text.into(),
        }
    }

    fn apply(text: &str, edits: Vec<Edit>) -> Result<String, RpcError> {
        let mut rope = Rope::from(text);
        let changes = to_changes(rope.slice(..), edits)?;
        let t = Transaction::change(&rope, changes.into_iter());
        assert!(t.apply(&mut rope));
        Ok(rope.to_string())
    }

    #[test]
    fn ranges_refer_to_the_text_before_any_edit() {
        // given out of order; the second edit's position ignores the first
        let out = apply(
            "one\ntwo\nthree\n",
            vec![edit(3, 1, 3, 6, "THREE"), edit(1, 1, 1, 4, "1")],
        )
        .unwrap();
        assert_eq!(out, "1\ntwo\nTHREE\n");
    }

    #[test]
    fn multibyte_columns_are_chars() {
        let out = apply("aé🦀b\n", vec![edit(1, 3, 1, 4, "crab")]).unwrap();
        assert_eq!(out, "aécrabb\n");
    }

    #[test]
    fn crlf_lines_and_end_of_line() {
        // a col past the end stops before "\r\n", never inside it
        let out = apply("ab\r\ncd\r\n", vec![edit(1, 99, 1, 99, "!")]).unwrap();
        assert_eq!(out, "ab!\r\ncd\r\n");
        // whole line 2, line ending included, via {2,1}..{3,1}
        let out = apply("ab\r\ncd\r\nef", vec![edit(2, 1, 3, 1, "")]).unwrap();
        assert_eq!(out, "ab\r\nef");
    }

    #[test]
    fn end_of_file() {
        let out = apply("a\nb", vec![edit(2, 2, 2, 2, "\nc")]).unwrap();
        assert_eq!(out, "a\nb\nc");
        let out = apply("a\n", vec![edit(2, 1, 2, 1, "b\n")]).unwrap();
        assert_eq!(out, "a\nb\n");
        // past the end clamps to the end
        let out = apply("a\n", vec![edit(9, 1, 9, 1, "z")]).unwrap();
        assert_eq!(out, "a\nz");
        let out = apply("", vec![edit(1, 1, 1, 1, "new")]).unwrap();
        assert_eq!(out, "new");
    }

    #[test]
    fn insertions_at_one_point_keep_their_order() {
        let out = apply(
            "x\n",
            vec![
                edit(1, 1, 1, 2, "Y"),
                edit(1, 1, 1, 1, "a"),
                edit(1, 1, 1, 1, "b"),
            ],
        )
        .unwrap();
        assert_eq!(out, "abY\n");
        // touching ranges do not overlap
        let out = apply("abcd", vec![edit(1, 1, 1, 3, "X"), edit(1, 3, 1, 5, "Y")]).unwrap();
        assert_eq!(out, "XY");
    }

    #[test]
    fn overlapping_and_backwards_ranges_are_refused() {
        let err = apply("abcdef", vec![edit(1, 1, 1, 4, "X"), edit(1, 3, 1, 5, "Y")]).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert_eq!(err.data, Some(json!({ "field": "edits" })));
        // an insertion strictly inside a replaced range overlaps it
        let err = apply("abcdef", vec![edit(1, 1, 1, 4, "X"), edit(1, 2, 1, 2, "Y")]).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        let err = apply("abcdef", vec![edit(1, 4, 1, 2, "X")]).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }
}
