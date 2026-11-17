# Brief: step 2c, panes, selections, editor/command, not yanking the user

You own `methods/views.rs`, `methods/command.rs` and `methods/interrupt.rs`.
Read `bb-bridge/briefs/step-2-common.md` first; its rules are binding.

The point of this piece: an agent can drive the editor (open panes, put the
owner on exact lines, run anything they could run from the keyboard) without
getting around bb's permission mode, without losing their work, and without
yanking them away while they type.

## Build

- **`view/list`, `view/focus`, `view/close`, `editor/select`** exactly as
  PROTOCOL.md. `viewId` values are the ones step 1 already returns. The
  layout tree mirrors Helix's own tree. Capability `views`.
- **`editor/command`** with `typed`, `static` and `keys`, and the command
  policy (`safe`, `all`, `none`) enforced here, in Helix. Capability
  `command`. The step 1 agent studied the command and key-map code and
  proposed this, which you should start from and verify:
  - Every path runs on the main loop, records `editor.status_msg` before and
    after, and returns the new message as `status`.
  - `typed`: tokenise with `helix_core::command_line`; under `safe` refuse
    any `%sh{…}` expansion; resolve name then aliases in the typed command
    map and check the policy lists (`sh`/`run-shell-command`, `pipe`,
    `pipe-to`, `insert-output`, `append-output`, and the force variants;
    `reload!` only when a target buffer is modified); then run it through
    `typed::execute_command` with `PromptEvent::Validate`. That needs the
    editor's job queue, so the bridge is handed what it needs at start.
  - `static`: resolve through the static command list; refuse the shell
    commands (`shell_pipe`, `shell_pipe_to`, `shell_insert_output`,
    `shell_append_output`, `shell_keep_pipe`) and, under `safe`,
    `replay_macro`; run with a `commands::Context` and apply its callbacks to
    the compositor as the editor view does. A command that waits for a
    following key (`f`, `r`, `m`, …) is refused as `unsupported` with a hint
    to use `keys`.
  - `keys`: parse with `parse_macro` and feed them one at a time through the
    compositor. Before each key, if the editor is on top, look the pending
    sequence plus this key up in the key map without changing state and apply
    the static policy to each command it would run; if a command-mode prompt
    is on top and the key is Enter, apply the typed policy to the prompt's
    line. A refusal stops the sequence and sends Escape. Keys typed in insert
    mode are text and need no check.
  - The owner's own key map is what `keys` resolves against, so their
    bindings work for an agent too.
- **Not yanking the user** (`methods/interrupt.rs`), exactly as PROTOCOL.md:
  track the time of the last key press and the mode with no upstream change
  if the existing events allow it; `interrupt: "whenIdle"` (default) defers
  when the user is in insert mode or pressed a key in the last 1500 ms;
  one pending-jump slot, newest wins; the status-line notice; `:bb-go`
  performs it; every focus-moving method returns `deferred`. Wire it into
  `editor/open` (step 1 code in `methods/editor.rs`: the smallest possible
  edit), `view/focus`, `editor/select` and `editor/command`. Expose one
  function the locations code can call for its `goto`; step 2a is leaving a
  single call site for it, which the coordinator wires at merge.

## Things to get right

- **The policy is a security boundary.** An agent must not be able to run a
  shell through any of the three forms, including through an alias, a key
  bound by the user to a shell command, a macro, a typed command reached by
  keys, or an expansion. Write the tests as an attacker would. Under `none`
  the method is refused outright; under `all` everything runs.
- **Unsaved work.** No `safe` path may discard it. Test each force variant.
- **Re-entrancy.** A command that opens a prompt or picker leaves the editor
  in that state; say so in the result rather than hanging. A command that
  needs more input than was given must not block the main loop.
- **Idle tracking costs nothing on the key path** beyond storing a timestamp.

Report as the common file says, and list every command the `safe` policy
refuses and why.
