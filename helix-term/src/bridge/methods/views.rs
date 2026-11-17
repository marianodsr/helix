//! view/list, view/focus, view/close, editor/select.
//!
//! A view is one pane. `viewId` is Helix's own view key, as `editor/open`
//! returns it. Focus-moving requests go through `interrupt::jump`.

use futures_util::future::BoxFuture;
use helix_core::{Range as HxRange, Selection, SmallVec};
use helix_view::events::DocumentFocusLost;
use helix_view::tree::Layout;
use helix_view::{align_view, Align, Editor, ViewId};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::bridge::convert::{char_from_pos, doc_num, path_string};
use crate::bridge::dispatch::on_editor;
use crate::bridge::methods::editor::{view_from_num, view_num};
use crate::bridge::methods::interrupt::{self, Interrupt};
use crate::bridge::protocol::{parse_params, ErrorCode, Range, RpcError, RpcResult};
use crate::bridge::server::Ctx;

pub const CAPABILITY: Option<&str> = Some("views");

#[derive(Debug, Serialize)]
struct Area {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ViewInfo {
    view_id: u64,
    doc_id: usize,
    path: Option<String>,
    focused: bool,
    area: Area,
    first_line: usize,
    last_line: usize,
}

/// `{ "split", "children" }` for a container, `{ "viewId" }` for a pane.
#[derive(Debug, PartialEq, Serialize)]
#[serde(untagged)]
enum Node {
    Split {
        split: &'static str,
        children: Vec<Node>,
    },
    View {
        #[serde(rename = "viewId")]
        view_id: u64,
    },
}

#[derive(Debug, Serialize)]
struct ListResult {
    views: Vec<ViewInfo>,
    layout: Node,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ViewParams {
    view_id: u64,
    interrupt: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectParams {
    view_id: Option<u64>,
    ranges: Vec<Range>,
    #[serde(default)]
    primary: usize,
    #[serde(default = "yes")]
    center: bool,
    interrupt: Option<String>,
}

fn yes() -> bool {
    true
}

fn split_name(layout: Layout) -> &'static str {
    // Helix's names: a vertical layout puts panes side by side (`:vsplit`).
    match layout {
        Layout::Horizontal => "horizontal",
        Layout::Vertical => "vertical",
    }
}

fn node_of(editor: &Editor, id: ViewId) -> Node {
    match editor.tree.container(id) {
        Some((layout, children)) => Node::Split {
            split: split_name(layout),
            children: children
                .iter()
                .map(|&child| node_of(editor, child))
                .collect(),
        },
        None => Node::View {
            view_id: view_num(id),
        },
    }
}

fn view_info(editor: &Editor, id: ViewId) -> ViewInfo {
    let view = editor.tree.get(id);
    let doc = &editor.documents[&view.doc];
    let text = doc.text().slice(..);
    let anchor = doc.view_offset(id).anchor.min(text.len_chars());
    ViewInfo {
        view_id: view_num(id),
        doc_id: doc_num(view.doc),
        path: path_string(doc.path()),
        focused: editor.tree.focus == id,
        area: Area {
            x: view.area.x,
            y: view.area.y,
            width: view.area.width,
            height: view.area.height,
        },
        first_line: text.char_to_line(anchor) + 1,
        last_line: view.estimate_last_doc_line(doc) + 1,
    }
}

pub fn list(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let result = on_editor(|editor, _| {
            let views = editor
                .tree
                .traverse()
                .map(|(id, _)| view_info(editor, id))
                .collect();
            ListResult {
                views,
                layout: node_of(editor, editor.tree.root()),
            }
        })
        .await?;
        Ok(serde_json::to_value(result).unwrap_or_default())
    })
}

fn known_view(editor: &Editor, n: u64) -> Result<ViewId, RpcError> {
    let id = view_from_num(n);
    if editor.tree.contains(id) {
        Ok(id)
    } else {
        Err(RpcError::not_found(format!("no view {n}")))
    }
}

/// `src/a.go:12`: what a pane shows, for the pending-jump notice.
fn shown(editor: &Editor, id: ViewId, line: Option<usize>) -> String {
    let doc = &editor.documents[&editor.tree.get(id).doc];
    match doc.path() {
        Some(path) => interrupt::location(path, line),
        None => doc.display_name().into_owned(),
    }
}

pub fn focus(_cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: ViewParams = parse_params(params)?;
        let mode = Interrupt::parse(params.interrupt.as_deref())?;
        let n = params.view_id;
        on_editor(move |editor, compositor| {
            let id = known_view(editor, n)?;
            if editor.tree.focus == id {
                return Ok(json!({ "deferred": false }));
            }
            let what = format!("show {}", shown(editor, id, None));
            let moved = interrupt::jump(editor, compositor, mode, what, move |editor, _| {
                let id = known_view(editor, n)?;
                editor.focus(id);
                Ok(())
            })?;
            Ok(json!({ "deferred": moved.is_none() }))
        })
        .await?
    })
}

/// Closes the pane, not its buffer. Closing the focused pane moves the
/// user's focus, so that case honours `interrupt` like `view/focus`.
pub fn close(_cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: ViewParams = parse_params(params)?;
        let mode = Interrupt::parse(params.interrupt.as_deref())?;
        let n = params.view_id;
        on_editor(move |editor, compositor| {
            let id = known_view(editor, n)?;
            check_not_last(editor)?;
            if editor.tree.focus != id {
                close_view(editor, id);
                return Ok(json!({ "deferred": false }));
            }
            let what = format!("close the pane showing {}", shown(editor, id, None));
            let moved = interrupt::jump(editor, compositor, mode, what, move |editor, _| {
                let id = known_view(editor, n)?;
                check_not_last(editor)?;
                close_view(editor, id);
                Ok(())
            })?;
            Ok(json!({ "deferred": moved.is_none() }))
        })
        .await?
    })
}

fn check_not_last(editor: &Editor) -> Result<(), RpcError> {
    if editor.tree.views().count() == 1 {
        return Err(RpcError::new(
            ErrorCode::Unsupported,
            "the last pane cannot be closed",
        ));
    }
    Ok(())
}

fn close_view(editor: &mut Editor, id: ViewId) {
    let was_focused = editor.tree.focus == id;
    let doc = editor.tree.get(id).doc;
    editor.close(id);
    if was_focused {
        // `Editor::close` sends no event; this is what a focus change sends,
        // so hooks (and bb's focus events) see it now, not at the next key.
        helix_event::dispatch(DocumentFocusLost { editor, doc });
    }
}

/// Protocol ranges -> a Helix selection; a range whose start equals its end
/// is a cursor there.
fn to_selection(text: helix_core::RopeSlice, ranges: &[Range], primary: usize) -> Selection {
    let ranges: SmallVec<[HxRange; 1]> = ranges
        .iter()
        .map(|r| HxRange::new(char_from_pos(text, r.start), char_from_pos(text, r.end)))
        .collect();
    Selection::new(ranges, primary)
}

pub fn select(_cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: SelectParams = parse_params(params)?;
        let mode = Interrupt::parse(params.interrupt.as_deref())?;
        if params.ranges.is_empty() {
            return Err(RpcError::invalid_params(
                "ranges",
                "ranges must not be empty",
            ));
        }
        if params.primary >= params.ranges.len() {
            return Err(RpcError::invalid_params(
                "primary",
                "primary must index into ranges",
            ));
        }
        on_editor(move |editor, compositor| {
            let id = match params.view_id {
                Some(n) => known_view(editor, n)?,
                None => editor.tree.focus,
            };
            let doc = &editor.documents[&editor.tree.get(id).doc];
            let version = doc.version();
            let line = params.ranges[params.primary].start.line;
            let what = format!("show {}", shown(editor, id, Some(line)));
            let n = view_num(id);
            let moved = interrupt::jump(editor, compositor, mode, what, move |editor, _| {
                let id = known_view(editor, n)?;
                Ok(select_on_main(editor, id, &params))
            })?;
            Ok(match moved {
                Some(version) => json!({ "version": version, "deferred": false }),
                None => json!({ "version": version, "deferred": true }),
            })
        })
        .await?
    })
}

fn select_on_main(editor: &mut Editor, id: ViewId, params: &SelectParams) -> i32 {
    let scrolloff = editor.config().scrolloff;
    let view = editor.tree.get_mut(id);
    let doc = editor
        .documents
        .get_mut(&view.doc)
        .expect("view's document");
    let selection = to_selection(doc.text().slice(..), &params.ranges, params.primary);
    doc.set_selection(id, selection);
    if params.center {
        align_view(doc, view, Align::Center);
    } else {
        view.ensure_cursor_in_view(doc, scrolloff);
    }
    doc.version()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::convert::pos_from_char;
    use crate::bridge::protocol::Position;
    use helix_core::Rope;

    fn range(sl: usize, sc: usize, el: usize, ec: usize) -> Range {
        Range {
            start: Position { line: sl, col: sc },
            end: Position { line: el, col: ec },
        }
    }

    #[test]
    fn ranges_become_a_selection() {
        let text = Rope::from("one\ntwo\nthree\n");
        let sel = to_selection(
            text.slice(..),
            &[range(1, 1, 1, 4), range(3, 2, 3, 2), range(2, 1, 2, 9)],
            1,
        );
        // `Selection::new` sorts the ranges; the primary follows its range.
        let r: Vec<_> = sel.iter().map(|r| (r.anchor, r.head)).collect();
        assert_eq!(r, [(0, 3), (4, 7), (9, 9)]);
        assert_eq!((sel.primary().anchor, sel.primary().head), (9, 9));
        assert_eq!(pos_from_char(text.slice(..), sel.primary().head).line, 3);
        // past the end of the text clamps to its end
        let sel = to_selection(text.slice(..), &[range(2, 1, 99, 1)], 0);
        assert_eq!(sel.primary().head, text.len_chars());
    }

    #[test]
    fn layout_nodes_serialize() {
        let node = Node::Split {
            split: "vertical",
            children: vec![Node::View { view_id: 7 }, Node::View { view_id: 9 }],
        };
        assert_eq!(
            serde_json::to_value(node).unwrap(),
            json!({ "split": "vertical", "children": [{ "viewId": 7 }, { "viewId": 9 }] })
        );
    }
}
