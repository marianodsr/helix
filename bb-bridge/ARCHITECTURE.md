# Helix bridge: architecture

A fork of Helix with a local socket that lets bb (and its agents) work with the
editor natively: see what the user is looking at, open and reload files, walk
an agent's changes, and use the language server Helix already runs.

Base: upstream `master` at `49986e667` (1011 commits after 25.07.1). Branch
`bb-bridge`. Remotes: `origin` = github.com/marianodsr/helix, `upstream` =
helix-editor/helix. Licence MPL-2.0: changes to upstream files stay published.

The wire contract is [PROTOCOL.md](PROTOCOL.md). This file says how it is
built and what "good" means.

## What the user asked for

These are the acceptance criteria; everything else serves them.

1. **Their Helix, unchanged.** Every key binding, theme and language setting
   in their own `~/.config/helix` works. The fork reads the normal config and
   adds only typed commands, which they bind themselves.
2. **bb always knows where they are.** Switching files or moving the cursor
   in Helix is reflected in bb (current file, line, selection) without the
   user doing anything, and agents can use it.
3. **Select, comment, send.** Select lines, write a comment, and send it to
   the agent, one at a time or several comments in one go.
4. **A full editor, not a preview.** They will live in it all day inside bb,
   and will add a file tree later, so the editor surface must take the whole
   work area when wanted, pass every key through, and stay fast.

5. **The agent can drive the editor.** Asked "take me to the entry point of
   X", the agent opens the right file in the right pane with the cursor on
   the line, and can walk the user through several places in order. Beyond
   that it has full control: anything the user can do from the keyboard, it
   can do through the bridge.

On the file tree: upstream Helix has a built-in file explorer picker
(`space e`) but no plugin system; that is still an open pull request
(helix-editor/helix#8675, branch `steel-event-system` of mattwparas/helix).
Third-party tree plugins only run there. Keeping this patch additive is what
lets it be carried onto that branch later; do not rely on anything that would
make that hard.

## Two halves

```
+-------------------------- Helix (this repo) ---------------------------+
| helix-term/src/bridge/                                                 |
|   server task (tokio) -- job::dispatch --> main loop (&mut Editor)     |
|   hooks (helix_event) --> coalescer --> per-client outbound queues     |
+----------------------------------|-------------------------------------+
                       unix socket, NDJSON JSON-RPC
+----------------------------------|-------------------------------------+
| bb plugin `helix-bridge` (repo bb-plugins, plugins/helix-bridge)        |
|   host entry: finds instances, holds sockets, forwards events          |
|   server: maps instance <-> thread, RPC, `bb hx` CLI, agent tools       |
|   app: Helix tab (hx in a bb terminal), status, "send to thread"        |
+------------------------------------------------------------------------+
```

Helix works the same in a bb terminal tab and in any other terminal: the
bridge is the socket, not the terminal.

## Rules for the Helix patch

The fork must stay cheap to rebase, and must not make the editor slower.

1. **Additive.** New code lives in `helix-term/src/bridge/` (and
   `bb-bridge/` for docs and tools). Edits to upstream files are hooks of a
   few lines each, listed in `bb-bridge/UPSTREAM_TOUCHPOINTS.md` with the
   reason for each. Prefer `helix_event::register_hook!` on existing events
   over editing call sites. Do not reformat or reorganise upstream code.
2. **The main loop is not changed.** The server is a tokio task. Anything
   that needs the editor goes through `helix_term::job::dispatch` with a
   oneshot for the answer; this is the same path Helix's own async jobs use.
3. **No cost when unused.** Every hook starts with one relaxed atomic load
   ("is anyone subscribed to this kind?") and returns. No allocation, no
   formatting, no locking on the key-press path when no client listens.
4. **Bounded cost when used.** A hook captures only small copy data (ids,
   positions) and hands it to the bridge task over a channel. Coalescing,
   text extraction, JSON and I/O happen off the main thread. `Rope` clones
   are O(1); slice to `String` off the main thread.
5. **Never block the editor on a client.** Outbound queues are bounded
   (PROTOCOL.md). A dead or slow client is dropped, not waited for.
6. **No file watcher.** Watching a huge monorepo is expensive and
   unreliable. Reload is driven from outside (`editor/reload`, sent by the
   plugin when bb reports file changes) plus a cheap mtime check of open
   buffers when the terminal regains focus.
7. **Never lose the user's work.** Reload skips buffers with unsaved
   changes and reports them. `applyEdits` is one undoable transaction and
   honours `version`.
8. **Language-server reuse.** `lsp/query` uses Helix's existing clients and
   their typed request methods (`goto_definition`, `goto_reference`, …).
   No second language server is started by the bridge.
9. **Full control, with two guards.** `editor/command` can run any typed
   command, named command or key sequence. Guard one is the command policy
   (PROTOCOL.md): by default no shell-running commands and no discarding of
   unsaved work, because an agent must not get around bb's permission mode
   through the editor, and must not lose the user's edits. The policy is
   enforced inside Helix, so no client can skip it. Guard two is "not
   yanking the user": focus-moving requests wait while the user is typing.
   Prefer the structured methods (`editor/open`, `locations/set`, `view/*`,
   `editor/select`, `editor/applyEdits`) for everything they cover; they are
   typed, testable and stable across Helix versions. `editor/command` is the
   escape hatch.
10. **Security.** Local socket only, directory `0700`, socket `0600`.

## Module layout (designed so later work does not collide)

```
helix-term/src/bridge/
  mod.rs          start/stop, config, registry file, public hooks
  server.rs       listener, per-client reader/writer, framing, cancel, queues
  protocol.rs     serde types for every method and event, error codes
  convert.rs      positions/ranges <-> Helix selections, paths
  events.rs       helix_event hooks, subscription flags, coalescer
  dispatch.rs     method table: name -> handler
  methods/
    editor.rs     hello, state, open, text, selection, status   (step 1)
    send.rs       :bb-send, :bb-ask, :bb-status commands        (step 1)
    reload.rs     editor/reload, focus-gained check             (step 2a)
    locations.rs  locations/*, picker, :bb-next/:bb-prev        (step 2a)
    edits.rs      editor/applyEdits                             (step 2a)
    diagnostics.rs  diagnostics/get                             (step 2b)
    lsp.rs        lsp/servers, lsp/query                        (step 2b)
    views.rs      view/*, editor/select                         (step 2c)
    command.rs    editor/command, command policy                (step 2c)
    interrupt.rs  idle tracking, pending jump, :bb-go           (step 2c)
    annotations.rs  annotations/set                             (step 3)
```

Step 1 creates every file, with the later ones as stubs that answer
`method_not_found` and are absent from `capabilities`. Steps 2a, 2b and 2c
then touch disjoint files and can run in parallel worktrees. In step 1,
`editor/open` accepts `interrupt`, `viewId` and `note` but always moves at
once; step 2c adds the waiting behaviour for every focus-moving method.

## Performance targets

Measured on this machine (M1 Pro), release build, with one subscribed client.

| What | Target |
|---|---|
| Key-press handling with no client connected | no measurable change against the base commit |
| Key-press handling with a client subscribed to everything | under 2% slower in the bench below |
| `editor/state` round trip | p50 under 1 ms, p99 under 5 ms |
| `editor/open` of an already-open file | p50 under 3 ms |
| `event/selection` latency after the debounce window | under 5 ms |
| Startup cost of the bridge | under 2 ms, off the first-frame path |
| `lsp/query` overhead above the server's own time | under 3 ms |
| Memory with an idle client | under 1 MiB |

`bb-bridge/bench/` holds the tools that produce these numbers: a small Rust
or Python client that measures round trips, and an editor benchmark that
replays a fixed key sequence over a large Go file in the integration harness
with and without subscribers. A change that misses a target is not done.

## Tests

- Unit tests for protocol types, conversions (multi-byte text, CRLF, end of
  file) and the coalescer.
- Integration tests in `helix-term/tests/` using Helix's own harness
  (`--features integration`): start the app, connect over the socket, drive
  each method and event, including error paths and two simultaneous clients.
- `cargo fmt --check`, `cargo clippy` on the bridge with no new warnings,
  and upstream's test suite still green.

## The plugin half (summary; details in bb-plugins/PLAN.md)

- **Host entry** runs on the machine where Helix runs. It lists
  `<cache>/bridge/*.json`, connects, subscribes, keeps the connection while
  an instance lives, and forwards events as host signals.
- **Instance to thread mapping**: the `BB_*` variables in the registry file
  when Helix was started in a bb terminal; otherwise the bb environment whose
  path contains the instance's workspace root, and within it an explicit
  binding or the most recently active thread.
- **Agent to editor**: on bb's file-change events and at the end of a turn,
  call `editor/reload`, then `changes/set` with the hunks from bb's Git
  plugin. Tools let the agent open a location for the user, read what the
  user has selected, read diagnostics and run `lsp/query`.
- **Editor to agent**: `event/send` becomes a draft in the thread's composer
  (or a sent message when `sendNow`), carrying path, lines and code.

## Running the fork beside stock Helix

The fork is installed as `hxb` (a wrapper that sets `HELIX_RUNTIME` to this
checkout's `runtime/` and runs `target/release/hx`), so the user's Homebrew
`hx` is untouched. Grammars are fetched and built once with
`hx --grammar fetch && hx --grammar build`.
