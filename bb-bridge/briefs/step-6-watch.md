# Brief: Helix reloads open files by itself when they change on disk

You integrated the bridge on this branch, so you know `methods/reload.rs`,
`methods/reveal.rs` and the interrupt gate. Work in
`/Users/marianodsr/projects/helix-bb` on `bb-bridge` as before (another agent
is building the review feature in its own worktree; it does not touch reload
or reveal).

## Why

The owner's words: Helix should work with a file watcher. They do not want
the model to insert code into Helix through tools, which costs context and
tool calls. Helix simply reacts to a file being changed and shows it.

Today a reload happens only when a client asks (`editor/reload`, which the bb
plugin sends when bb reports an agent's edit) or when the terminal regains
focus. So nothing happens for Helix in an ordinary terminal with an agent
running elsewhere, for git operations, for formatters. Rule 6 in
ARCHITECTURE.md is rewritten: Helix watches the files it has open, and
nothing else. The contract is the new section "Files changing on disk
(capability `watch`)" in PROTOCOL.md, with `event/reloaded` and
`[editor.bridge] watch`.

## Build (`methods/watch.rs`, capability `watch`)

- Watch the file of every open buffer: start on open, stop on close, follow
  a buffer whose path changes (`:write other`, `:move`). Only open buffers,
  never directories recursively.
- Editors and tools replace files by writing a temp file and renaming it
  over the original, so a watch on the file's inode alone goes blind after
  the first save. Handle create, write, rename-over and delete-then-create.
  Watching the parent directory non-recursively for that one name is the
  usual way; choose what is correct on macOS first and Linux second.
- No idle wake-ups: an event-driven watch (kqueue or FSEvents through the
  `notify` crate, or the platform API directly), not a polling timer. If you
  must poll somewhere, say where and why. Adding the `notify` crate is
  acceptable; list it in UPSTREAM_TOUCHPOINTS.md and regenerate
  `Cargo.lock`.
- On a change: coalesce a burst (about 30 ms), then go through the same code
  as `editor/reload` for that path, with reveal according to the setting.
  Clean buffers reload; modified buffers get the status message and
  `event/externalChange` once per change on disk; a buffer whose own save
  caused the event is not reloaded (compare with what Helix just wrote).
- `event/reloaded` after every reload, whatever its origin, with the changed
  ranges and removed-line count that reload already keeps.
- `[editor.bridge] watch = false` disables it; `HELIX_BRIDGE=0` too.

## Things to get right

- **Latency**: from the write finishing to the new text on screen, under
  50 ms on a normal file. Measure it under the pty.
- **Cost**: nothing on the key path; no measurable CPU when nothing changes
  with 200 buffers open; no descriptor exhaustion with 1,000 open buffers
  (check the limit and what happens at it).
- **No loops and no lost edits**: Helix's own writes (`:w`, auto-save,
  format on save) must not trigger a reload of the buffer that wrote; a
  write that lands while the user has unsaved changes never touches the
  buffer; a write during an in-flight reload is not lost.
- **An agent editing a file many times in a few seconds** (ten writes one
  second apart, and ten writes 20 ms apart): every final state is shown, the
  editor stays responsive, and the reveal of a newer change replaces an
  older one cleanly.
- **Other cases**: the file deleted and recreated, the directory renamed,
  a symlinked path, a file on a different volume, `git checkout` changing
  twenty open files at once, a 20k-line file rewritten completely.
- The focus-gained check stays, and must not reload twice what the watch
  already reloaded.

## Verify and ship

Unit and integration tests, live checks under the pty in `live.py` (its own
section), upstream's suite, fmt, clippy, `live.py verify` in full. Then
refresh the owner's build exactly as before (`try/hx`, atomically, only after
everything passes). Commit on `bb-bridge` in small commits; do not push.

Report: how it watches and why that way, the latency and cost numbers, every
upstream and shared-file edit, anything that differs from PROTOCOL.md, and
what you could not verify.
