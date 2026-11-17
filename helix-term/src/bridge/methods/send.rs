//! `:bb-send`, `:bb-send!`, `:bb-ask`, `:bb-comment`, `:bb-comments-send`,
//! `:bb-emit`, `:bb-status`. Ordinary typed commands with no default key binding; users
//! bind them in their own config, e.g. `C = ":bb-comment"` under
//! `[keys.select]`.

use anyhow::bail;
use helix_core::command_line::{Args, Signature};

use crate::bridge::events::{self, DocMeta, Kind, Msg, SendSnap};
use crate::bridge::protocol::SendKind;
use crate::commands::typed::{CommandCompleter, TypableCommand};
use crate::compositor::{self, Compositor};
use crate::job::Callback;
use crate::ui::{self, PromptEvent};
use helix_view::Editor;

/// Everything after the command name, taken literally (quotes and `%` are
/// not interpreted), so a note can say anything.
const NOTE: Signature = Signature {
    positionals: (0, None),
    raw_after: Some(0),
    ..Signature::DEFAULT
};

pub const COMMANDS: &[TypableCommand] = &[
    TypableCommand {
        name: "bb-send",
        aliases: &[],
        doc: "Send the selections to bb, with an optional note, as a draft.",
        fun: bb_send,
        completer: CommandCompleter::none(),
        signature: NOTE,
    },
    TypableCommand {
        name: "bb-send!",
        aliases: &[],
        doc: "Send the selections to bb, with an optional note, and send it at once.",
        fun: bb_send_now,
        completer: CommandCompleter::none(),
        signature: NOTE,
    },
    TypableCommand {
        name: "bb-ask",
        aliases: &[],
        doc: "Ask bb a question about the selections.",
        fun: bb_ask,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (1, None),
            raw_after: Some(0),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "bb-comment",
        aliases: &[],
        doc: "Add a review comment on the selected lines for bb. Without text, prompts for it.",
        fun: bb_comment,
        completer: CommandCompleter::none(),
        signature: NOTE,
    },
    TypableCommand {
        name: "bb-comments-send",
        aliases: &[],
        doc: "Send every pending review comment to bb.",
        fun: bb_comments_send,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "bb-emit",
        aliases: &[],
        doc: "Ask bb to do something: `:bb-emit <name> [words]`. The names are bb's.",
        fun: bb_emit,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (1, None),
            ..Signature::DEFAULT
        },
    },
    TypableCommand {
        name: "bb-status",
        aliases: &[],
        doc: "Show the bb bridge socket and connected clients.",
        fun: bb_status,
        completer: CommandCompleter::none(),
        signature: Signature {
            positionals: (0, Some(0)),
            ..Signature::DEFAULT
        },
    },
];

fn note(args: &Args) -> String {
    args.first().unwrap_or_default().trim().to_owned()
}

fn bb_send(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    emit(cx.editor, SendKind::Send, note(&args), false)?;
    cx.editor.set_status("Sent to bb");
    Ok(())
}

fn bb_send_now(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    emit(cx.editor, SendKind::Send, note(&args), true)?;
    cx.editor.set_status("Sent to bb");
    Ok(())
}

fn bb_ask(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let question = note(&args);
    if question.is_empty() {
        bail!(":bb-ask needs a question");
    }
    emit(cx.editor, SendKind::Ask, question, true)?;
    cx.editor.set_status("Asked bb");
    Ok(())
}

fn bb_comment(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let text = note(&args);
    if text.is_empty() && events::wants_kind(Kind::CommentPrompt) {
        // the client asks for the text in its own UI, placed by the
        // selection's cells, which need the compositor's size: a job
        cx.jobs.callback(async move {
            let call = move |editor: &mut Editor, compositor: &mut Compositor| {
                request_comment(editor, compositor);
            };
            Ok(Callback::EditorCompositor(Box::new(call)))
        });
        return Ok(());
    }
    ensure_listener()?;
    if !text.is_empty() {
        emit(cx.editor, SendKind::Comment, text, false)?;
        cx.editor.set_status("Comment added");
        return Ok(());
    }
    cx.jobs.callback(async move {
        let call = move |_: &mut Editor, compositor: &mut Compositor| {
            let prompt = ui::Prompt::new(
                "comment: ".into(),
                None,
                ui::completers::none,
                |cx: &mut compositor::Context, input: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }
                    let text = input.trim();
                    if text.is_empty() {
                        cx.editor.set_status("Empty comment, nothing sent");
                        return;
                    }
                    match emit(cx.editor, SendKind::Comment, text.to_owned(), false) {
                        Ok(()) => cx.editor.set_status("Comment added"),
                        Err(err) => cx.editor.set_error(err.to_string()),
                    }
                },
            );
            compositor.push(Box::new(prompt));
        };
        Ok(Callback::EditorCompositor(Box::new(call)))
    });
    Ok(())
}

fn bb_emit(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let name = args.first().unwrap_or_default().to_owned();
    let shared = events::wants_kind(Kind::Action)
        .then(crate::bridge::shared)
        .flatten();
    let Some(shared) = shared else {
        bail!("No bb client is listening for '{name}' (:bb-emit)");
    };
    let (_, doc) = current_ref!(cx.editor);
    let rest: Vec<String> = args.iter().skip(1).map(|a| a.to_string()).collect();
    events::broadcast(
        shared,
        Kind::Action,
        "event/action",
        serde_json::json!({
            "name": name,
            "args": rest,
            "path": crate::bridge::convert::path_string(doc.path()),
            "docId": crate::bridge::convert::doc_num(doc.id()),
            "mode": events::mode_name(cx.editor.mode()),
        }),
        false,
    );
    Ok(())
}

/// `event/commentRequest`: the primary selection as `event/send` carries
/// it, and its first and last visible cells in the terminal grid.
fn request_comment(editor: &mut Editor, compositor: &Compositor) {
    let (view, doc) = current_ref!(editor);
    let size = compositor.size();
    let (top, bottom) = selection_cells(editor);
    let screen = serde_json::json!({
        "top": top,
        "bottom": bottom,
        "rows": size.height,
        "cols": size.width,
    });
    events::send(Msg::CommentRequest {
        snap: SendSnap {
            kind: SendKind::Comment,
            note: String::new(),
            send_now: false,
            meta: DocMeta::of(doc),
            text: doc.text().clone(),
            selection: Some(doc.selection(view.id).clone()),
        },
        screen,
    });
}

/// The terminal cells (0-based row and column from the top-left of the
/// screen) of the primary selection's first grapheme and of its last one
/// (not counting a final line ending), placed as the renderer places them:
/// the view's area without its gutter, its scroll offsets, soft wrap and
/// every virtual line `View::text_annotations` reserves (annotations, the
/// inline diff, inline diagnostics). Cells off screen are clamped to the
/// view.
pub fn selection_cells(editor: &Editor) -> (serde_json::Value, serde_json::Value) {
    use helix_core::chars::char_is_line_ending;
    use helix_core::graphemes::prev_grapheme_boundary;
    use helix_core::{visual_offset_from_anchor, VisualOffsetError};

    let (view, doc) = current_ref!(editor);
    let text = doc.text().slice(..);
    let range = doc.selection(view.id).primary();
    let first = range.from();
    let mut last = prev_grapheme_boundary(text, range.to()).max(first);
    if last > first && char_is_line_ending(text.char(last)) {
        last = prev_grapheme_boundary(text, last).max(first);
    }
    let inner = view.inner_area(doc);
    let offset = doc.view_offset(view.id);
    let format = doc.text_format(inner.width, None);
    let annotations = view.text_annotations(doc, None);
    let height = inner.height.max(1) as usize;
    let width = inner.width.max(1) as usize;
    let cell = |pos: usize| {
        let (row, col) = match visual_offset_from_anchor(
            text,
            offset.anchor,
            pos,
            &format,
            &annotations,
            offset.vertical_offset + height,
        ) {
            Ok((p, _)) => (
                p.row as isize - offset.vertical_offset as isize,
                p.col as isize - offset.horizontal_offset as isize,
            ),
            Err(VisualOffsetError::PosBeforeAnchorRow) => (0, 0),
            Err(VisualOffsetError::PosAfterMaxRow) => (height as isize - 1, 0),
        };
        let row = row.clamp(0, height as isize - 1) as usize;
        let col = col.clamp(0, width as isize - 1) as usize;
        serde_json::json!({ "row": inner.y as usize + row, "col": inner.x as usize + col })
    };
    (cell(first), cell(last))
}

fn bb_comments_send(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    emit(cx.editor, SendKind::Flush, String::new(), true)?;
    cx.editor.set_status("Comments sent to bb");
    Ok(())
}

fn bb_status(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let Some(shared) = crate::bridge::shared() else {
        cx.editor.set_status("bb bridge is off");
        return Ok(());
    };
    let clients: Vec<String> = shared
        .clients
        .lock()
        .iter()
        .map(|c| {
            let name = c.name.lock().clone().unwrap_or_else(|| "unnamed".into());
            match Kind::names(c.subscriptions()) {
                subs if subs.is_empty() => name,
                subs => format!("{name} [{}]", subs.join(" ")),
            }
        })
        .collect();
    let socket = shared.info.socket.display();
    let all = crate::bridge::dispatch::CAPABILITIES;
    let offered: Vec<&str> = all.iter().flatten().copied().collect();
    // the list of ten no longer fits a status line beside the clients
    let capabilities = if offered.len() == all.len() {
        "all of protocol 1".to_owned()
    } else {
        offered.join(" ")
    };
    let watching = match super::watch::counts() {
        _ if !super::watch::running() => String::new(),
        (n, 0) => format!("; watching {n} files"),
        (n, over) => format!("; watching {n} files ({over} over the descriptor limit)"),
    };
    // most useful first: a narrow status line cuts the end
    cx.editor.set_status(match clients.len() {
        0 => format!("bb bridge: no clients; offers {capabilities}{watching}; {socket}"),
        n => format!(
            "bb bridge: {n} client{}: {}; offers {capabilities}{watching}; {socket}",
            if n == 1 { "" } else { "s" },
            clients.join(", ")
        ),
    });
    Ok(())
}

fn ensure_listener() -> anyhow::Result<()> {
    if !events::wants_kind(Kind::Send) {
        bail!("No bb client is listening (bridge: nobody subscribed to send)");
    }
    Ok(())
}

/// Capture the focused selection and hand it to the event task.
fn emit(editor: &Editor, kind: SendKind, note: String, send_now: bool) -> anyhow::Result<()> {
    ensure_listener()?;
    let (view, doc) = current_ref!(editor);
    events::send(Msg::Send(SendSnap {
        kind,
        note,
        send_now,
        meta: DocMeta::of(doc),
        text: doc.text().clone(),
        selection: (kind != SendKind::Flush).then(|| doc.selection(view.id).clone()),
    }));
    Ok(())
}
