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

6. **Watching the agent write.** Text an agent writes appears in the editor
   as it lands, shown as if typed, without the user reloading anything.
7. **Comments are visible in the code.** Leaving a comment marks the lines
   in the gutter and shows the comment, and later the agent's reply, under
   them.

8. **Reviewing changes in the editor.** The same editor shows a set of
   changes (the working tree, the thread's branch or pull request, one agent
   turn, a commit) as a diff: a list of changed files, removed and added
   lines drawn in place, movement from change to change. The user comments
   on lines and sends the comments to the agent, as in requirement 3. The
   files stay real and editable, with the language server working.
9. **A file tree beside the editor**, using yazi, which the user already
   has: choosing a file in the tree opens it in Helix, and the tree follows
   the file Helix shows. This is in the bb plugin; the fork needs nothing
   for it beyond what the bridge already offers.

On "as it lands": bb does not give plugins the contents of an edit while the
model is still generating it, only each edit when it is made. So the unit is
one edit, revealed with an animation the moment it happens, not one token.

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
    annotations.rs  annotations/*, :bb-annotations              (step 2d)
    reveal.rs     editor/reveal, the reveal animation           (step 2d)
    review.rs     review/*, :bb-review, :bb-diff, inline diff   (step 5)
```

Step 1 creates every file, with the later ones as stubs that answer
`method_not_found` and are absent from `capabilities`. Steps 2a, 2b, 2c and
2d then touch disjoint files and can run in parallel worktrees. `reveal.rs`
is new in 2d. After the merge, `editor/reload` with `reveal` calls into it
with the ranges its own diff produced. In step 1,
`editor/open` accepts `interrupt`, `viewId` and `note` but always moves at
once; step 2c adds the waiting behaviour for every focus-moving method.

## Performance targets

Measured on this machine (M1 Pro), release build, with one subscribed client.

| What | Target | Measured 2026-11-17 (quiet machine, `bb-bridge` at `5d5adcaf2`) |
|---|---|---|
| Key-press handling with no client connected | no measurable change against the base commit | 1608 / 1641 µs per key (two rounds) against 1613 / 1614 µs at `49986e667`, 8,400 moves over a 20k-line Go file: within the 1% drift between rounds. Edits: 22.9 ms per key on both |
| Key-press handling with a client subscribed to everything | under 2% slower in the bench below | -0.1% against the bridge off, in both rounds (1607.5 / 1621.6 µs against 1609.4 / 1623.8) |
| `editor/state` round trip | p50 under 1 ms, p99 under 5 ms | p50 0.042 ms, p99 0.078 ms (12.5k-line Go file in view; read-only requests draw no frame) |
| `editor/open` of an already-open file | p50 under 3 ms | p50 1.66 ms, p99 2.19 ms (one frame of the 12.5k-line file) |
| `event/selection` for a single move (leading edge) | p50 under 2 ms | key to event p50 0.68 ms, p99 7.1 ms |
| `event/selection` trailing flush of a burst | late by under 10 ms at p99 (macOS coalesces timers; measured 6 ms) | late by p50 2.8 ms, p99 4.0 ms (measured inside Helix) |
| Startup cost of the bridge | under 2 ms, off the first-frame path | 0.47 ms on the main thread; binding and the registry file 1.3 ms in a background task |
| `lsp/query` overhead above the server's own time | under 3 ms | Small module: p50 0.06 to 0.93 ms for every kind. Kubernetes v1.34.0 (16k Go files, gopls 4 GB, after its workspace load): `definition`, `hover`, `references`, `implementation` p50 0.08 to 1.0 ms. Symbol kinds, re-measured 2026-11-17 after `previews` (off by default for them) and partial, parallel reads of files that are not open, with a packaging build running (load average 9 falling to 4): `workspaceSymbols` 100 ms apart p50 2.40 ms, p90 3.1 ms (was 3.9 ms); `documentSymbols` (kubelet.go, open) 100 ms apart p50 2.8 and 3.2 ms in two runs, at the target's edge (was 3.4 ms); back to back 0.78 ms. An idle CPU runs the same work about 3x slower |
| Memory with an idle client | under 1 MiB | 62 KiB per idle subscribed client (hx at 36 MiB with no client) |

Cursor moves in a real terminal (`bench/move_latency.py`, key to the end of
its output, p50 at 53 / 100 / 140 columns): Go `net/http/server.go`
(4,100 lines) 6.1 / 6.4 / 5.3 ms in the fork against 2.9 / 3.1 / 3.2 ms in
Homebrew's Helix 25.07; TypeScript (a 3,059-line `server.ts`) 2.5 / 2.5 /
2.7 ms against 2.8 / 3.1 / 3.2 ms. The bridge is not involved (it runs with
no client). The base commit was not measured here (the benchmark's build of
it is the integration-test one, which has no terminal).

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

## Distribution: installing on another machine

The owner will install this on a second Mac (Apple silicon). Everything
needed must be in the two GitHub repositories, and nothing may depend on
paths from the machine it was built on.

- **Nothing essential untracked.** Source, tests, benchmarks, the wrapper,
  the installer, the configuration under `bb-bridge/config/` and the plugin
  are all committed and pushed.
- **No absolute paths at run time.** `hxb` finds its runtime directory
  relative to itself. The plugin looks for `hxb` on `PATH` (setting to
  override) and finds the socket directory from Helix's cache directory.
- **`bb-bridge/install.sh`**, one command on the target machine:
  1. installs the editor as `hxb` under `~/.local` (binary plus `runtime/`
     with grammars and queries): from the prebuilt `aarch64-apple-darwin`
     archive attached to a GitHub release of the fork when one matches,
     otherwise by building from source (it checks for the Xcode command line
     tools and Rust, and says what to install if they are missing);
  2. copies `bb-bridge/config/helix` to `~/.config/helix` only if that
     directory does not exist, and otherwise leaves it alone and says so;
     The same rule applies to `bb-bridge/config/ghostty/config` and
     `~/.config/ghostty/config`;
  3. installs the bb plugin from the bb-plugins repository when `bb` is
     present;
  4. runs `bb hx doctor`, which checks the binary, the protocol version, the
     socket directory, the tools the configuration expects (`gopls`,
     `goimports`) and the font the Ghostty configuration names, printing the
     command for each one that is missing.
  It is safe to run twice and never deletes anything.
- **Release archive.** A script builds the release binary, fetches and builds
  the grammars, and packs `hxb` plus `runtime/` into the archive the
  installer downloads. It is built on this machine and uploaded with the
  GitHub CLI; a binary fetched with `curl` is not quarantined by macOS.
- **Verified by doing it.** The installer is tested in a clean throwaway
  `HOME` on this machine (no Rust on `PATH`, no Helix config) before it is
  called done.

## Integration step (after 2a to 2d are merged)

Work that crosses the pieces, done once on the merged branch:

1. **No redraw for requests that change nothing.** Helix redraws after every
   job it dispatches to the main loop, so each bridge request costs a frame:
   0.2 ms on a small file, about 7 ms on a very large one, which is paid by
   the user's own key presses when a client asks often. Each method declares
   whether it can change what is on screen; after a read-only one the redraw
   is skipped. This is the one deliberate change to the main loop: a single
   condition around the existing `render()` call for dispatched jobs, listed
   in UPSTREAM_TOUCHPOINTS.md. Rule 2 above otherwise stands.
2. **Wire the pieces together**: `editor/reload` with `reveal` calls
   `reveal::play` with the ranges its diff produced; `locations` `goto`,
   `editor/reveal` `follow` and `editor/reload` `follow` go through the
   interrupt gate from 2c.
3. **Kitty keyboard from the environment.** Inside bb, bb's terminal answers
   the capability query before the editor surface can, so Helix never turns
   the protocol on. `HELIX_KITTY_KEYBOARD=enabled|disabled` overrides the
   `kitty-keyboard-protocol` setting at start; the plugin sets it on sessions
   it starts.
4. **Full benchmark on a quiet machine**, every row of the performance
   table, including `lsp/query` overhead on a large Go repository.
5. **Build hygiene.** Worktrees never share a cargo target directory: cargo
   then reuses another worktree's build of a crate. Seed each worktree's own
   `target/` as a copy-on-write clone (`cp -c -R`) instead.
