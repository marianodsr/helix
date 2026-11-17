//! editor/command and the command policy.
//!
//! Three forms, each run on the main loop as the keyboard would run it:
//! `typed` (a command line), `static` (a named command, as key maps name
//! them) and `keys` (key events fed through the compositor, resolved against
//! the user's own key map). The result carries what the command left on the
//! status line, and whether it left a prompt, picker or key sequence open.
//!
//! The policy (`[editor.bridge] commands`) is enforced here, in Helix, so no
//! client can skip it. `none` refuses the method and `all` runs anything.
//! `safe` refuses whatever would run a program outside bb's permission mode
//! or throw away the user's work, by every route: a command name or alias, a
//! `%sh{…}` expansion at any depth, a key bound to a shell command or macro, a
//! command line reached by keys, the command palette. bb-bridge/COMMAND_POLICY.md
//! lists every command with its verdict; a test keeps it in step with this
//! file and fails when upstream adds a command nobody has classified.
//!
//! Typed and static commands get a job queue of their own: async jobs reach
//! the editor through the global queue as usual, and jobs that must finish
//! before exit are handed to the real queue as `Callback::Followup`s.

use std::borrow::Cow;
use std::num::NonZeroUsize;

use futures_util::future::BoxFuture;
use futures_util::stream::FuturesUnordered;
use helix_core::command_line::{self, Args, ExpansionKind, Token, TokenKind, Tokenizer};
use helix_event::runtime_local;
use helix_view::document::Mode;
use helix_view::editor::{BridgeCommandPolicy, Severity};
use helix_view::input::KeyEvent;
use helix_view::keyboard::{KeyCode, KeyModifiers};
use helix_view::{current, Editor};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::bridge::dispatch::on_editor;
use crate::bridge::events::mode_name;
use crate::bridge::methods::interrupt::{self, Interrupt};
use crate::bridge::protocol::{parse_params, ErrorCode, RpcError, RpcResult};
use crate::bridge::server::Ctx;
use crate::commands::typed::{TypableCommand, TYPABLE_COMMAND_MAP};
use crate::commands::{self, MappableCommand};
use crate::compositor::{self, Component, Compositor, Event};
use crate::events::{OnModeSwitch, PostCommand};
use crate::job::{self, Callback, Job, Jobs};
use crate::keymap::{KeyTrie, KeyTrieNode, Keymaps};
use crate::ui::editor::InsertEvent;
use crate::ui::{picker, EditorView, Prompt, PromptEvent};

pub const CAPABILITY: Option<&str> = Some("command");

// --- the policy ----------------------------------------------------------------

const SHELL: &str = "runs a shell command";
const SHELL_EXPANSION: &str = "contains a %sh{…} expansion, which runs a shell command";
const DISCARDS: &str = "discards unsaved changes";
const OVERWRITES: &str = "overwrites a file that changed on disk since Helix read it";
const DEBUGGER: &str = "starts or drives the debugger, which runs programs";
const SPEAKS: &str = "sends a message to bb in the user's name";
const RELOADS: &str =
    "reloads configuration from files an agent can write, which can name programs to run";

/// Typed commands `safe` refuses, by canonical name (aliases resolve to it).
const REFUSED_TYPED: &[(&str, &str)] = &[
    ("run-shell-command", SHELL),
    ("pipe", SHELL),
    ("pipe-to", SHELL),
    ("insert-output", SHELL),
    ("append-output", SHELL),
    ("quit!", DISCARDS),
    ("quit-all!", DISCARDS),
    ("cquit!", DISCARDS),
    ("buffer-close!", DISCARDS),
    ("buffer-close-all!", DISCARDS),
    ("buffer-close-others!", DISCARDS),
    ("write-quit!", DISCARDS),
    ("write-quit-all!", DISCARDS),
    ("write!", OVERWRITES),
    ("write-all!", OVERWRITES),
    ("write-buffer-close!", OVERWRITES),
    ("exit!", OVERWRITES),
    (
        "lsp-workspace-command",
        "runs a language server command, which can run programs",
    ),
    ("debug-start", DEBUGGER),
    ("debug-remote", DEBUGGER),
    ("debug-eval", DEBUGGER),
    (
        "workspace-trust",
        "lets the workspace's own configuration start programs, which is the user's decision",
    ),
    ("config-reload", RELOADS),
    ("workspace-untrust", RELOADS),
    ("workspace-exclude", RELOADS),
    ("bb-send", SPEAKS),
    ("bb-send!", SPEAKS),
    ("bb-ask", SPEAKS),
    ("bb-comment", SPEAKS),
    ("bb-comments-send", SPEAKS),
];

/// Typed commands `safe` refuses only in some states (see `check_typed_args`),
/// for the policy document.
#[cfg(test)]
const CONDITIONAL_TYPED: &[(&str, &str)] = &[
    ("reload", "refused while the buffer has unsaved changes"),
    ("reload-all", "refused while any buffer has unsaved changes"),
    (
        "set-option",
        "refused for the settings listed under Settings",
    ),
    (
        "toggle-option",
        "refused for the settings listed under Settings",
    ),
];

/// Static commands `safe` refuses. Every `dap_*` command is refused too.
const REFUSED_STATIC: &[(&str, &str)] = &[
    ("shell_pipe", SHELL),
    ("shell_pipe_to", SHELL),
    ("shell_insert_output", SHELL),
    ("shell_append_output", SHELL),
    ("shell_keep_pipe", SHELL),
    (
        "replay_macro",
        "replays keys from a register without checking them",
    ),
    (
        "command_palette",
        "runs any command, out of reach of the policy",
    ),
    (
        "suspend",
        "suspends Helix and hands the terminal to the shell",
    ),
];

/// Top-level settings `:set-option` and `:toggle-option` may not change.
const REFUSED_SETTINGS: &[(&str, &str)] = &[
    ("shell", "names the program shell commands run"),
    (
        "clipboard-provider",
        "can name programs to run on every yank and paste",
    ),
    ("terminal", "names the program the debugger runs in"),
    (
        "workspace-trust",
        "decides which workspaces may start programs",
    ),
    ("bridge", "holds this command policy"),
];

/// Static commands that wait for the next key; `static` cannot give it.
const WAITS_FOR_KEY: &[&str] = &[
    "find_till_char",
    "find_next_char",
    "extend_till_char",
    "extend_next_char",
    "till_prev_char",
    "find_prev_char",
    "extend_till_prev_char",
    "extend_prev_char",
    "replace",
    "select_register",
    "insert_register",
    "copy_between_registers",
    "select_textobject_around",
    "select_textobject_inner",
    "surround_add",
    "surround_replace",
    "surround_delete",
    "insert_char_interactive",
    "append_char_interactive",
    "goto_word",
    "extend_to_word",
];

fn typed_refusal(name: &str) -> Option<&'static str> {
    REFUSED_TYPED
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, reason)| *reason)
}

fn static_refusal(name: &str) -> Option<&'static str> {
    if name.starts_with("dap_") {
        return Some(DEBUGGER);
    }
    REFUSED_STATIC
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, reason)| *reason)
}

/// The top-level setting a `:set-option` key names, read the way
/// `set_option` reads it (lowercase, `.` as a JSON pointer separator).
fn setting_refusal(key: &str) -> Option<(String, &'static str)> {
    let key = key.to_lowercase();
    let pointer = key.replace('.', "/");
    let top = pointer
        .split('/')
        .next()
        .unwrap_or_default()
        .replace("~1", "/")
        .replace("~0", "~");
    REFUSED_SETTINGS
        .iter()
        .find(|(name, _)| *name == top)
        .map(|(name, reason)| (name.to_string(), *reason))
}

runtime_local! {
    static AT_START: Mutex<Option<BridgeCommandPolicy>> = Mutex::new(None);
}

/// Remember the policy the bridge started with. Called once from `start`.
pub fn capture_policy(editor: &Editor) {
    *AT_START.lock() = Some(editor.config().bridge.commands);
}

fn permissiveness(policy: BridgeCommandPolicy) -> u8 {
    match policy {
        BridgeCommandPolicy::None => 0,
        BridgeCommandPolicy::Safe => 1,
        BridgeCommandPolicy::All => 2,
    }
}

/// The policy in force: the stricter of the one at start and the current
/// configuration, so a configuration change (`:set`, `:config-reload`) can
/// tighten it but never loosen it.
pub fn policy(editor: &Editor) -> BridgeCommandPolicy {
    let now = editor.config().bridge.commands;
    match *AT_START.lock() {
        Some(start) if permissiveness(start) < permissiveness(now) => start,
        _ => now,
    }
}

fn refused(command: &str, reason: &str) -> RpcError {
    RpcError {
        code: ErrorCode::Unsupported,
        message: format!("`{command}` is refused by the command policy: it {reason}"),
        data: Some(json!({ "reason": reason, "command": command })),
    }
}

fn unsupported(message: impl Into<String>) -> RpcError {
    let message = message.into();
    RpcError {
        code: ErrorCode::Unsupported,
        data: Some(json!({ "reason": message })),
        message,
    }
}

/// Fails if a token runs a shell when expanded, at any depth. Walks the
/// token exactly as `helix_view::expansion::expand` does, with the same
/// tokenizer.
fn check_expansion(token: &Token) -> Result<(), ()> {
    match token.kind {
        TokenKind::Expansion(ExpansionKind::Shell) => Err(()),
        TokenKind::Expand => check_expand_content(&token.content),
        _ => Ok(()),
    }
}

fn check_expand_content(content: &str) -> Result<(), ()> {
    let mut start = 0;
    while let Some(offset) = content[start..].find('%') {
        let idx = start + offset;
        if content.as_bytes().get(idx + 1) == Some(&b'%') {
            start = idx + 2;
            continue;
        }
        let mut tokenizer = Tokenizer::new(&content[idx..], true);
        match tokenizer.parse_percent_token() {
            Some(Ok(token)) => check_expansion(&token)?,
            // Expansion stops here with an error, before anything after it.
            _ => return Ok(()),
        }
        start = idx + tokenizer.pos();
    }
    Ok(())
}

/// A command line resolved as `execute_command_line` resolves it: a bare
/// number is `goto`, otherwise a name or alias.
fn resolve(line: &str) -> Option<(&'static TypableCommand, &str)> {
    let (name, rest, _) = command_line::split(line);
    if name.is_empty() {
        return None;
    }
    if name.parse::<usize>().is_ok() && rest.trim().is_empty() {
        return Some((TYPABLE_COMMAND_MAP.get("goto")?, name));
    }
    TYPABLE_COMMAND_MAP.get(name).map(|cmd| (*cmd, rest))
}

/// Parse arguments as Helix will, expanding them; a `%sh{…}` anywhere is
/// `Err(true)`, any other parse failure `Err(false)` (Helix fails the same
/// way, before running anything).
fn parse_args<'a>(
    editor: &Editor,
    cmd: &TypableCommand,
    args: &'a str,
    refuse_shell: bool,
) -> Result<Args<'a>, (bool, String)> {
    let mut shell = false;
    let parsed = Args::parse(args, cmd.signature, true, |token| {
        if refuse_shell && check_expansion(&token).is_err() {
            shell = true;
            return Err("shell expansion refused".into());
        }
        helix_view::expansion::expand(editor, token).map_err(Into::into)
    });
    parsed.map_err(|err| (shell, err.to_string()))
}

/// Checks that need the parsed (expanded) arguments or the editor's state.
fn check_typed_args(editor: &Editor, cmd: &TypableCommand, args: &Args) -> Result<(), RpcError> {
    match cmd.name {
        "reload" if helix_view::doc!(editor).is_modified() => Err(refused(
            "reload",
            "discards the unsaved changes in this buffer",
        )),
        "reload-all" if editor.documents().any(|doc| doc.is_modified()) => Err(refused(
            "reload-all",
            "discards the unsaved changes of a modified buffer",
        )),
        "set-option" | "toggle-option" => match args.first().and_then(setting_refusal) {
            Some((setting, reason)) => Err(refused(
                &format!("{} {setting}", cmd.name),
                &format!("changes a setting that {reason}"),
            )),
            None => Ok(()),
        },
        _ => Ok(()),
    }
}

/// The whole `safe` check of a typed command about to run with `args`.
fn check_typed(editor: &Editor, cmd: &TypableCommand, args: &str) -> Result<(), RpcError> {
    if let Some(reason) = typed_refusal(cmd.name) {
        return Err(refused(cmd.name, reason));
    }
    match parse_args(editor, cmd, args, true) {
        Ok(args) => check_typed_args(editor, cmd, &args),
        Err((true, _)) => Err(refused(cmd.name, SHELL_EXPANSION)),
        Err((false, _)) => Ok(()),
    }
}

fn check_mappable(editor: &Editor, cmd: &MappableCommand) -> Result<(), RpcError> {
    match cmd {
        MappableCommand::Static { name, .. } => match static_refusal(name) {
            Some(reason) => Err(refused(name, reason)),
            None => Ok(()),
        },
        MappableCommand::Typable { name, args, .. } => {
            // `MappableCommand::execute` looks the name up without the
            // bare-number rule.
            match TYPABLE_COMMAND_MAP.get(name.as_str()) {
                Some(cmd) => check_typed(editor, cmd, args),
                None => Ok(()),
            }
        }
        MappableCommand::Macro { name, .. } => Err(refused(
            name,
            "is a macro binding, whose keys run without being checked",
        )),
    }
}

// --- what is on screen -----------------------------------------------------------

/// A layer drawn above the editor.
pub(crate) enum Layer {
    Prompt {
        /// The `:` command line (the only prompt with command docs).
        command: bool,
        line: String,
        /// Identity, to tell prompts opened during a request from others.
        addr: usize,
    },
    Picker {
        /// The command palette.
        palette: bool,
    },
    /// Hover, signature help, menus: keys they ignore reach the layer below.
    Popup,
    Other(&'static str),
}

fn classify(layer: &mut Box<dyn Component>) -> Layer {
    let name = layer.type_name();
    let id = layer.id();
    let addr = (&**layer as *const dyn Component).cast::<()>() as usize;
    if let Some(prompt) = layer.as_any_mut().downcast_mut::<Prompt>() {
        return Layer::Prompt {
            command: (prompt.doc_fn)("write").is_some(),
            line: prompt.line().clone(),
            addr,
        };
    }
    if id == Some(picker::ID) {
        return Layer::Picker {
            palette: name.contains("MappableCommand"),
        };
    }
    if name.starts_with("helix_term::ui::popup::Popup<") {
        return Layer::Popup;
    }
    Layer::Other(name)
}

/// The layers above the editor, top first. Helix keeps them private, so
/// they are popped and pushed back in the same order; nothing runs between.
pub(crate) fn layers<R>(compositor: &mut Compositor, f: impl FnOnce(&[Layer]) -> R) -> R {
    let mut popped = Vec::new();
    while compositor.layer_count() > 1 {
        popped.extend(compositor.pop());
    }
    let found: Vec<Layer> = popped.iter_mut().map(classify).collect();
    for layer in popped.into_iter().rev() {
        compositor.push(layer);
    }
    f(&found)
}

fn prompt_addrs(compositor: &mut Compositor) -> Vec<usize> {
    if compositor.layer_count() == 1 {
        return Vec::new();
    }
    layers(compositor, |layers| {
        layers
            .iter()
            .filter_map(|layer| match layer {
                Layer::Prompt { addr, .. } => Some(*addr),
                _ => None,
            })
            .collect()
    })
}

/// What is left open after a command: the top layer, if any.
fn overlay(compositor: &mut Compositor) -> Option<&'static str> {
    if compositor.layer_count() == 1 {
        return None;
    }
    layers(compositor, |layers| {
        layers.first().map(|layer| match layer {
            Layer::Prompt { .. } => "prompt",
            Layer::Picker { .. } => "picker",
            Layer::Popup => "popup",
            Layer::Other(_) => "other",
        })
    })
}

// --- checking keys -----------------------------------------------------------------

const ESC: KeyEvent = KeyEvent {
    code: KeyCode::Esc,
    modifiers: KeyModifiers::NONE,
};
const ENTER: KeyEvent = KeyEvent {
    code: KeyCode::Enter,
    modifiers: KeyModifiers::NONE,
};
/// A key no map binds: dismisses pending keys, as Helix does for the mouse.
const NULL: KeyEvent = KeyEvent {
    code: KeyCode::Null,
    modifiers: KeyModifiers::NONE,
};

fn is_cancel(key: KeyEvent) -> bool {
    key == ESC
        || key
            == (KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
            })
}

/// As `EditorView` canonicalizes a key before looking it up.
fn canonical(mut key: KeyEvent) -> KeyEvent {
    if let KeyCode::Char(_) = key.code {
        key.modifiers.remove(KeyModifiers::SHIFT);
    }
    key
}

enum Peek {
    Run(Vec<MappableCommand>),
    Pending,
    NotFound,
    Cancelled(Vec<KeyEvent>),
}

/// `Keymaps::get` without changing any state: what `key` would do after
/// `pending`, and the pending keys afterwards.
fn peek(
    keymap: &KeyTrie,
    sticky: Option<&KeyTrieNode>,
    pending: &[KeyEvent],
    key: KeyEvent,
) -> (Peek, Vec<KeyEvent>) {
    if key == ESC && !pending.is_empty() {
        return (Peek::Cancelled(pending.to_vec()), Vec::new());
    }
    let sticky = if key == ESC { None } else { sticky };
    let first = pending.first().unwrap_or(&key);
    let root = match sticky {
        Some(node) => Cow::Owned(KeyTrie::Node(node.clone())),
        None => Cow::Borrowed(keymap),
    };
    let trie = match root.search(&[*first]) {
        Some(KeyTrie::MappableCommand(cmd)) => {
            return (Peek::Run(vec![cmd.clone()]), pending.to_vec())
        }
        Some(KeyTrie::Sequence(cmds)) => return (Peek::Run(cmds.clone()), pending.to_vec()),
        None => return (Peek::NotFound, pending.to_vec()),
        Some(trie) => trie,
    };
    let mut state = pending.to_vec();
    state.push(key);
    match trie.search(&state[1..]) {
        Some(KeyTrie::Node(node)) if node.is_sticky => (Peek::Pending, Vec::new()),
        Some(KeyTrie::Node(_)) => (Peek::Pending, state),
        Some(KeyTrie::MappableCommand(cmd)) => (Peek::Run(vec![cmd.clone()]), Vec::new()),
        Some(KeyTrie::Sequence(cmds)) => (Peek::Run(cmds.clone()), Vec::new()),
        None => (Peek::Cancelled(state), Vec::new()),
    }
}

/// Check every command a peek says would run. In insert mode, cancelled
/// keys that are not characters are looked up again (`EditorView::insert_mode`).
fn check_peek(
    editor: &Editor,
    keymaps: &Keymaps,
    keymap: &KeyTrie,
    mode: Mode,
    peeked: Peek,
) -> Result<(), RpcError> {
    match peeked {
        Peek::Run(cmds) => cmds.iter().try_for_each(|cmd| check_mappable(editor, cmd)),
        Peek::Cancelled(keys) if mode == Mode::Insert => {
            let mut pending = Vec::new();
            for key in keys.into_iter().filter(|k| k.char().is_none()) {
                let (peeked, next) = peek(keymap, keymaps.sticky(), &pending, key);
                if let Peek::Run(cmds) = peeked {
                    cmds.iter()
                        .try_for_each(|cmd| check_mappable(editor, cmd))?;
                }
                pending = next;
            }
            Ok(())
        }
        Peek::Cancelled(_) | Peek::Pending | Peek::NotFound => Ok(()),
    }
}

/// `.` repeats the command that entered insert mode and replays the keys
/// typed there; check both.
fn check_repeat(editor: &Editor, view: &EditorView) -> Result<(), RpcError> {
    let (cmd, events) = &view.last_insert;
    check_mappable(editor, cmd)?;
    let map = view.keymaps.map();
    let keymap = &map[&Mode::Insert];
    let mut pending = Vec::new();
    for event in events {
        if let InsertEvent::Key(key) = event {
            let (peeked, next) = peek(keymap, view.keymaps.sticky(), &pending, *key);
            check_peek(editor, &view.keymaps, keymap, Mode::Insert, peeked)
                .map_err(|_| refused(".", "repeats an insert whose keys ran a refused command"))?;
            pending = next;
        }
    }
    Ok(())
}

/// A key the editor itself handles: mirror `EditorView::command_mode` and
/// `insert_mode`, then the key map.
fn check_editor_key(editor: &Editor, view: &EditorView, key: KeyEvent) -> Result<(), RpcError> {
    let mode = editor.mode();
    let keymaps = &view.keymaps;
    if mode != Mode::Insert && key.modifiers.is_empty() {
        match (key.code, editor.count) {
            (KeyCode::Char('0'..='9'), Some(_)) => return Ok(()),
            (KeyCode::Char('1'..='9'), None) if !keymaps.contains_key(mode, key) => return Ok(()),
            (KeyCode::Char('.'), _) if keymaps.pending().is_empty() => {
                return check_repeat(editor, view)
            }
            _ => {}
        }
    }
    let map = keymaps.map();
    let keymap = &map[&mode];
    let (peeked, _) = peek(keymap, keymaps.sticky(), keymaps.pending(), key);
    check_peek(editor, keymaps, keymap, mode, peeked)
}

fn check_prompt_key(
    editor: &Editor,
    key: KeyEvent,
    command: bool,
    line: &str,
    opened_here: bool,
) -> Result<(), RpcError> {
    if key != ENTER {
        return Ok(());
    }
    if !command {
        if opened_here {
            return Ok(());
        }
        return Err(unsupported(
            "Enter in a prompt this request did not open is refused by the command policy",
        ));
    }
    if line.is_empty() {
        return Err(unsupported(
            "Enter on an empty command line runs the last command in its history; \
             it is refused by the command policy",
        ));
    }
    match resolve(line) {
        Some((cmd, args)) => check_typed(editor, cmd, args),
        None => Err(unsupported(format!(
            "`{line}` is not a command; Enter is refused by the command policy"
        ))),
    }
}

/// The `safe` check of one key, against whatever will receive it.
fn check_key(
    editor: &Editor,
    compositor: &mut Compositor,
    key: KeyEvent,
    start_prompts: &[usize],
) -> Result<(), RpcError> {
    let key = canonical(key);
    if compositor.layer_count() > 1 {
        let decided = layers(compositor, |layers| {
            for layer in layers {
                return Some(match layer {
                    // a key the popup ignores reaches the layer below
                    Layer::Popup => continue,
                    Layer::Prompt {
                        command,
                        line,
                        addr,
                    } => check_prompt_key(editor, key, *command, line, !start_prompts.contains(addr)),
                    Layer::Picker { palette: true } if !is_cancel(key) => Err(refused(
                        "command_palette",
                        "runs any command, out of reach of the policy",
                    )),
                    Layer::Picker { .. } => Ok(()),
                    Layer::Other(_) if is_cancel(key) => Ok(()),
                    Layer::Other(name) => Err(unsupported(format!(
                        "keys for `{name}` are refused by the command policy; send <esc> to close it"
                    ))),
                });
            }
            None
        });
        if let Some(decided) = decided {
            return decided;
        }
    }
    match compositor.find::<EditorView>() {
        Some(view) => check_editor_key(editor, view, key),
        None => Ok(()),
    }
}

// --- running -----------------------------------------------------------------------

/// A job queue for one command. Its async jobs go through the global queue
/// as usual; see `hand_over` for the rest.
fn scratch_jobs() -> Jobs {
    Jobs {
        wait_futures: FuturesUnordered::new(),
        callbacks: tokio::sync::mpsc::channel(1).1,
        status_messages: tokio::sync::mpsc::channel(1).1,
    }
}

/// Jobs that must finish before Helix exits (formatting and saving) go to
/// the real queue: the application adds the job a `Followup` returns.
fn hand_over(jobs: Jobs) {
    for future in jobs.wait_futures {
        let followup = Callback::Followup(Box::new(move |_| Some(Job { future, wait: true })));
        tokio::spawn(job::dispatch_callback(followup));
    }
}

fn run_typed(editor: &mut Editor, jobs: &mut Jobs, line: &str, safe: bool) -> Result<(), RpcError> {
    let (cmd, args) = resolve(line).ok_or_else(|| no_such_command(line))?;
    if safe {
        if let Some(reason) = typed_refusal(cmd.name) {
            return Err(refused(cmd.name, reason));
        }
    }
    let args = match parse_args(editor, cmd, args, safe) {
        Ok(args) => args,
        Err((true, _)) => return Err(refused(cmd.name, SHELL_EXPANSION)),
        Err((false, err)) => {
            editor.set_error(format!("'{}': {err}", cmd.name));
            return Ok(());
        }
    };
    if safe {
        check_typed_args(editor, cmd, &args)?;
    }
    let mut cx = compositor::Context {
        editor,
        jobs,
        scroll: None,
    };
    if let Err(err) = (cmd.fun)(&mut cx, args, PromptEvent::Validate) {
        cx.editor.set_error(format!("'{}': {err}", cmd.name));
    }
    Ok(())
}

fn no_such_command(line: &str) -> RpcError {
    let (name, _, _) = command_line::split(line);
    RpcError::invalid_params("typed", format!("no such command: '{name}'"))
}

/// Run a static command as `EditorView` runs a bound key: then the hooks,
/// then the compositor callbacks, then the view bookkeeping.
fn run_static(
    editor: &mut Editor,
    compositor: &mut Compositor,
    jobs: &mut Jobs,
    cmd: &MappableCommand,
    count: Option<NonZeroUsize>,
) -> Result<(), RpcError> {
    let old_mode = editor.mode();
    let mut cx = commands::Context {
        register: None,
        count,
        editor,
        callback: Vec::new(),
        on_next_key_callback: None,
        jobs,
    };
    cmd.execute(&mut cx);
    if cx.on_next_key_callback.take().is_some() {
        cx.editor.autoinfo = None;
        return Err(waits_for_key(cmd.name()));
    }
    helix_event::dispatch(PostCommand {
        command: cmd,
        cx: &mut cx,
    });
    let new_mode = cx.editor.mode();
    if new_mode != old_mode {
        helix_event::dispatch(OnModeSwitch {
            old_mode,
            new_mode,
            cx: &mut cx,
        });
        if new_mode == Mode::Insert {
            if let Some(view) = compositor.find::<EditorView>() {
                view.last_insert.0 = cmd.clone();
                view.last_insert.1.clear();
            }
        }
    }
    let callbacks = std::mem::take(&mut cx.callback);
    let mut cx = compositor::Context {
        editor: cx.editor,
        jobs: cx.jobs,
        scroll: None,
    };
    for callback in callbacks {
        callback(compositor, &mut cx);
    }
    let editor = cx.editor;
    if !editor.should_close() {
        let scrolloff = editor.config().scrolloff;
        let mode = editor.mode();
        let (view, doc) = current!(editor);
        view.ensure_cursor_in_view(doc, scrolloff);
        if mode != Mode::Insert {
            doc.append_changes_to_history(view);
        }
    }
    Ok(())
}

fn waits_for_key(name: &str) -> RpcError {
    unsupported(format!(
        "`{name}` waits for the next key; send both with `keys` instead"
    ))
}

/// Feed keys through the compositor as if typed. Under `safe` each key is
/// checked against what will receive it first; a refusal stops the
/// sequence and sends Escape.
fn run_keys(
    editor: &mut Editor,
    compositor: &mut Compositor,
    jobs: &mut Jobs,
    keys: &[KeyEvent],
    safe: bool,
) -> Result<(), RpcError> {
    let mut cx = compositor::Context {
        editor,
        jobs,
        scroll: None,
    };
    // Start clean: dismiss a key sequence or a "next key" command the user
    // (or an earlier request) left half done, so every key is checked
    // against what it will really do.
    if compositor.layer_count() == 1 && cx.editor.mode() != Mode::Insert {
        compositor.handle_event(&Event::Key(NULL), &mut cx);
    }
    let start_prompts = prompt_addrs(compositor);
    for (sent, &key) in keys.iter().enumerate() {
        if cx.editor.should_close() {
            break;
        }
        if safe {
            if let Err(mut err) = check_key(cx.editor, compositor, key, &start_prompts) {
                compositor.handle_event(&Event::Key(ESC), &mut cx);
                if let Some(Value::Object(data)) = &mut err.data {
                    data.insert("key".into(), key.key_sequence_format().into());
                    data.insert("sent".into(), sent.into());
                }
                return Err(err);
            }
        }
        compositor.handle_event(&Event::Key(key), &mut cx);
    }
    Ok(())
}

// --- the method ----------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandParams {
    typed: Option<String>,
    #[serde(rename = "static")]
    static_name: Option<String>,
    keys: Option<String>,
    count: Option<usize>,
    interrupt: Option<String>,
}

enum Form {
    Typed(String),
    Static(&'static MappableCommand, Option<NonZeroUsize>),
    Keys(String, Vec<KeyEvent>),
}

impl Form {
    fn parse(params: &CommandParams) -> Result<Form, RpcError> {
        let given = [
            params.typed.is_some(),
            params.static_name.is_some(),
            params.keys.is_some(),
        ];
        if given.iter().filter(|g| **g).count() != 1 {
            return Err(RpcError::invalid_params(
                "typed",
                "give exactly one of typed, static, keys",
            ));
        }
        if let Some(line) = &params.typed {
            let line = line.strip_prefix(':').unwrap_or(line);
            if resolve(line).is_none() {
                return Err(no_such_command(line));
            }
            return Ok(Form::Typed(line.to_owned()));
        }
        if let Some(name) = &params.static_name {
            let cmd = MappableCommand::STATIC_COMMAND_LIST
                .iter()
                .find(|cmd| cmd.name() == name)
                .ok_or_else(|| {
                    RpcError::invalid_params("static", format!("no such command: '{name}'"))
                })?;
            let count = match params.count {
                None => None,
                Some(n) => Some(NonZeroUsize::new(n).ok_or_else(|| {
                    RpcError::invalid_params("count", "count must be at least 1")
                })?),
            };
            return Ok(Form::Static(cmd, count));
        }
        let keys = params.keys.clone().unwrap_or_default();
        let parsed = helix_view::input::parse_macro(&keys)
            .map_err(|err| RpcError::invalid_params("keys", err.to_string()))?;
        Ok(Form::Keys(keys, parsed))
    }

    /// For the pending-jump notice: "bb wants to <this> (:bb-go)".
    fn describe(&self) -> String {
        match self {
            Form::Typed(line) => format!("run :{line}"),
            Form::Static(cmd, _) => format!("run {}", cmd.name()),
            Form::Keys(keys, _) => format!("press {keys}"),
        }
    }

    /// Refusals that do not depend on the moment, so a request that would
    /// be refused is not deferred first.
    fn precheck(&self, safe: bool) -> Result<(), RpcError> {
        match self {
            Form::Typed(line) => match resolve(line) {
                Some((cmd, _)) if safe => match typed_refusal(cmd.name) {
                    Some(reason) => Err(refused(cmd.name, reason)),
                    None => Ok(()),
                },
                _ => Ok(()),
            },
            Form::Static(cmd, _) => {
                if safe {
                    if let Some(reason) = static_refusal(cmd.name()) {
                        return Err(refused(cmd.name(), reason));
                    }
                }
                if WAITS_FOR_KEY.contains(&cmd.name()) {
                    return Err(waits_for_key(cmd.name()));
                }
                Ok(())
            }
            Form::Keys(..) => Ok(()),
        }
    }
}

fn enforced(editor: &Editor) -> Result<bool, RpcError> {
    match policy(editor) {
        BridgeCommandPolicy::None => Err(unsupported(
            "editor/command is disabled ([editor.bridge] commands = \"none\")",
        )),
        BridgeCommandPolicy::Safe => Ok(true),
        BridgeCommandPolicy::All => Ok(false),
    }
}

fn severity(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info | Severity::Hint => "info",
    }
}

/// Run a command now. Keys and commands run for a client do not count as
/// the user's key presses.
fn execute(editor: &mut Editor, compositor: &mut Compositor, form: Form) -> RpcResult {
    let safe = enforced(editor)?;
    form.precheck(safe)?;
    let before = editor.status_msg.take();
    let mut jobs = scratch_jobs();
    let ran = interrupt::as_bridge(|| match &form {
        Form::Typed(line) => run_typed(editor, &mut jobs, line, safe),
        Form::Static(cmd, count) => run_static(editor, compositor, &mut jobs, cmd, *count),
        Form::Keys(_, keys) => run_keys(editor, compositor, &mut jobs, keys, safe),
    });
    hand_over(jobs);
    let status = editor.status_msg.clone();
    if status.is_none() {
        editor.status_msg = before;
    }
    ran?;
    let mut result = json!({
        "deferred": false,
        "mode": mode_name(editor.mode()),
    });
    if let Some((message, level)) = status {
        result["status"] = json!({ "message": message, "severity": severity(level) });
    }
    if let Some(overlay) = overlay(compositor) {
        result["overlay"] = overlay.into();
    }
    if let Some(view) = compositor.find::<EditorView>() {
        let pending = view.keymaps.pending();
        if !pending.is_empty() {
            let keys: String = pending.iter().map(KeyEvent::key_sequence_format).collect();
            result["pendingKeys"] = keys.into();
        }
    }
    Ok(result)
}

pub fn command(_cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let params: CommandParams = parse_params(params)?;
        let mode = Interrupt::parse(params.interrupt.as_deref())?;
        let form = Form::parse(&params)?;
        on_editor(move |editor, compositor| {
            let safe = enforced(editor)?;
            form.precheck(safe)?;
            let what = form.describe();
            let moved =
                interrupt::jump(editor, compositor, mode, what, move |editor, compositor| {
                    execute(editor, compositor, form)
                })?;
            Ok(moved.unwrap_or_else(|| json!({ "deferred": true })))
        })
        .await?
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::typed::TYPABLE_COMMAND_LIST;

    fn token_refused(line: &str) -> bool {
        Tokenizer::new(line, true).any(|token| check_expansion(&token.unwrap()).is_err())
    }

    #[test]
    fn shell_expansions_are_found_at_any_depth() {
        assert!(token_refused("%sh{echo hi}"));
        assert!(token_refused("a %sh(echo hi) b"));
        assert!(token_refused(r#""%sh{echo hi}""#));
        assert!(token_refused(r#""x %{cursor_line} %sh|id| y""#));
        assert!(token_refused(r#""%%%sh{id}""#), "%% then a real expansion");
        // the raw rest of a command line is one `Expand` token
        assert!(check_expand_content("note %sh<id> here").is_err());
        assert!(check_expand_content("%u{20}%sh{id}").is_err());

        assert!(!token_refused("%{cursor_line} %u{25CF} %reg{a}"));
        assert!(!token_refused("'%sh{literal}' `%sh{literal}`"));
        assert!(!token_refused(r#""100%% sh{not}""#));
        assert!(check_expand_content("50%% done").is_ok());
        // a broken expansion fails before anything after it runs
        assert!(check_expand_content("%bad %sh{id}").is_ok());
    }

    #[test]
    fn aliases_resolve_to_the_refused_name() {
        for (alias, name) in [
            ("sh ls", "run-shell-command"),
            ("! ls", "run-shell-command"),
            ("| sort", "pipe"),
            ("q!", "quit!"),
            ("qa!", "quit-all!"),
            ("cq!", "cquit!"),
            ("bc!", "buffer-close!"),
            ("bclose!", "buffer-close!"),
            ("bca!", "buffer-close-all!"),
            ("bco!", "buffer-close-others!"),
            ("wq!", "write-quit!"),
            ("xa!", "write-quit-all!"),
            ("x!", "exit!"),
            ("w!", "write!"),
            ("dbg", "debug-start"),
        ] {
            let (cmd, _) = resolve(alias).unwrap();
            assert_eq!(cmd.name, name, "{alias}");
            assert!(typed_refusal(cmd.name).is_some(), "{alias}");
        }
        assert_eq!(resolve("42").unwrap().0.name, "goto");
        assert!(resolve("").is_none());
        assert!(resolve(" q!").is_none(), "a leading space is no command");
    }

    #[test]
    fn settings_are_read_as_set_option_reads_them() {
        for key in [
            "shell",
            "SHELL",
            "bridge",
            "bridge.commands",
            "Bridge.Commands",
            "bridge/commands",
            "clipboard-provider",
            "clipboard-provider.custom.yank",
            "terminal.command",
            "workspace-trust.level",
        ] {
            assert!(setting_refusal(key).is_some(), "{key}");
        }
        for key in [
            "scrolloff",
            "lsp.display-messages",
            "soft-wrap.enable",
            "x.shell",
        ] {
            assert!(setting_refusal(key).is_none(), "{key}");
        }
    }

    #[test]
    fn policy_lists_name_real_commands() {
        for (name, _) in REFUSED_TYPED.iter().chain(CONDITIONAL_TYPED) {
            assert!(TYPABLE_COMMAND_MAP.contains_key(name), "{name}");
        }
        let statics: Vec<&str> = MappableCommand::STATIC_COMMAND_LIST
            .iter()
            .map(MappableCommand::name)
            .collect();
        for (name, _) in REFUSED_STATIC {
            assert!(statics.contains(name), "{name}");
        }
        for name in WAITS_FOR_KEY {
            assert!(statics.contains(name), "{name}");
        }
    }

    /// bb-bridge/COMMAND_POLICY.md, generated part. Set
    /// `BRIDGE_UPDATE_POLICY=1` to rewrite it after reviewing a new command.
    fn policy_tables() -> String {
        let mut out = String::from(
            "## Typed commands\n\n| Command | Aliases | Under `safe` |\n|---|---|---|\n",
        );
        let mut typed: Vec<&TypableCommand> = TYPABLE_COMMAND_LIST
            .iter()
            .chain(crate::bridge::typable_commands())
            .collect();
        typed.sort_by_key(|cmd| cmd.name);
        for cmd in typed {
            let verdict = match (
                typed_refusal(cmd.name),
                CONDITIONAL_TYPED.iter().find(|(n, _)| *n == cmd.name),
            ) {
                (Some(reason), _) => format!("refused: {reason}"),
                (None, Some((_, when))) => when.to_string(),
                (None, None) => "allowed".into(),
            };
            let aliases: Vec<String> = cmd
                .aliases
                .iter()
                .map(|a| format!("`{}`", a.replace('|', "\\|")))
                .collect();
            out += &format!("| `{}` | {} | {verdict} |\n", cmd.name, aliases.join(" "));
        }
        out += "\nEvery typed command is also refused when an argument contains a `%sh{…}` expansion, at any depth.\n";
        out += "\n## Settings\n\n`:set-option` and `:toggle-option` are refused for these top-level settings and anything under them:\n\n| Setting | Why |\n|---|---|\n";
        for (name, reason) in REFUSED_SETTINGS {
            out += &format!("| `{name}` | {reason} |\n");
        }
        out += "\n## Static commands\n\n| Command | Under `safe` |\n|---|---|\n";
        let mut statics: Vec<&str> = MappableCommand::STATIC_COMMAND_LIST
            .iter()
            .map(MappableCommand::name)
            .collect();
        statics.sort_unstable();
        for name in statics {
            let verdict = match static_refusal(name) {
                Some(reason) => format!("refused: {reason}"),
                None if WAITS_FOR_KEY.contains(&name) => {
                    "allowed in `keys`; `static` answers `unsupported` (waits for the next key)"
                        .into()
                }
                None => "allowed".into(),
            };
            out += &format!("| `{name}` | {verdict} |\n");
        }
        out
    }

    #[test]
    fn policy_document_is_current() {
        const MARKER: &str = "<!-- generated: do not edit below this line -->\n";
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bb-bridge/COMMAND_POLICY.md"
        );
        let doc = std::fs::read_to_string(path).unwrap_or_default();
        let (head, tail) = doc.split_once(MARKER).unwrap_or((&doc, ""));
        let tables = policy_tables();
        if tail == tables {
            return;
        }
        if std::env::var_os("BRIDGE_UPDATE_POLICY").is_some() {
            std::fs::write(path, format!("{head}{MARKER}{tables}")).unwrap();
            return;
        }
        panic!(
            "bb-bridge/COMMAND_POLICY.md is out of date (a command was added, removed or \
             reclassified). Review the new verdicts, then rerun with BRIDGE_UPDATE_POLICY=1."
        );
    }
}
