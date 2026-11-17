//! `:bb-send`, `:bb-send!`, `:bb-ask`, `:bb-comment`, `:bb-comments-send`,
//! `:bb-status`. Ordinary typed commands with no default key binding; users
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
    ensure_listener()?;
    let text = note(&args);
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
    // most useful first: a narrow status line cuts the end
    cx.editor.set_status(match clients.len() {
        0 => format!("bb bridge: no clients; offers {capabilities}; {socket}"),
        n => format!(
            "bb bridge: {n} client{}: {}; offers {capabilities}; {socket}",
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
