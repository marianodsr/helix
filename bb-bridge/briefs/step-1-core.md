# Brief: Helix bridge, step 1 (core)

You are implementing the first step of a socket bridge inside a fork of the
Helix editor. Work in `/Users/marianodsr/projects/helix-bb`, branch
`bb-bridge` (already checked out, based on upstream master `49986e667`).

Read `bb-bridge/ARCHITECTURE.md` and `bb-bridge/PROTOCOL.md` first. They are
the design and the wire contract; another agent is building the client side
against PROTOCOL.md at the same time, so implement it exactly. If something in
it cannot be implemented as written, stop guessing: pick the closest behaviour,
record the difference in `bb-bridge/PROTOCOL_NOTES.md`, and say so in your
report so the coordinator can change the contract for both sides.

## Scope of this step

Everything marked "step 1" in the module layout of ARCHITECTURE.md:

- `[editor.bridge] enable` config (default true) and `HELIX_BRIDGE=0`.
- Socket, registry file, lifecycle (clean up on exit, tolerate a stale socket
  file from a crashed process with the same pid).
- Framing, request dispatch through `job::dispatch`, errors, `$/cancel`,
  several clients at once, bounded outbound queues with `event/overflow`.
- Methods: `bridge/hello`, `editor/state`, `editor/open`, `editor/text`,
  `editor/selection`, `editor/status`.
- `events/subscribe` / `unsubscribe` and these events: `selection`,
  `documents` (opened, closed, changed, saved, focus), `mode`, `send`.
  `diagnostics`, `lsp` and `externalChange` kinds are accepted by subscribe
  but emit nothing yet.
- Typed commands `:bb-send`, `:bb-send!`, `:bb-ask`, `:bb-status`.
- Stub files for the later steps exactly as the layout shows, each answering
  `method_not_found`, so two agents can fill them in parallel afterwards
  without touching the same files. Put the method table in `dispatch.rs` so
  adding a method is one line there.
- `bb-bridge/UPSTREAM_TOUCHPOINTS.md`: every upstream file you edited, the
  lines, and why. Keep this list as short as you can.
- `bb-bridge/bench/`: a round-trip client that reports p50/p99 for a method,
  and the key-press benchmark described in ARCHITECTURE.md. Report the
  numbers for every row of the performance table that applies to step 1,
  against the base commit where the row asks for it.
- Tests as described under "Tests" in ARCHITECTURE.md.
- `bb-bridge/hxb`: the wrapper script described at the end of
  ARCHITECTURE.md, and the grammars for at least go, rust, typescript, tsx,
  json, toml, yaml, markdown, bash, python, starlark and protobuf built so
  the fork is usable.

## What is already known

- Rust 1.90.0 is installed with rustup; binaries are in `~/.cargo/bin`, which
  is not on PATH: call `~/.cargo/bin/cargo` or export PATH yourself.
  Dependencies are already fetched.
- `helix_term::job::dispatch(|editor, compositor| …)` runs a closure on the
  main loop with `&mut Editor`; `helix_event::register_hook!` subscribes to
  the events in `helix-view/src/events.rs` and `helix-term/src/events.rs`
  (there is no "document saved" event there; find the smallest hook).
  `helix-term/src/handlers.rs` shows how existing handlers are set up.
- Helix has an integration test harness under `helix-term/tests/` behind
  `--features integration`.
- The agent sandbox blocks writes outside this folder, so cargo's registry
  lock in `~/.cargo` will be refused: run cargo with the sandbox disabled
  when that happens. Chrome and other GUI apps are not needed here.
- This machine has 8 cores and 16 GB and is shared with another agent's
  work. Run one cargo command at a time and set `CARGO_BUILD_JOBS=4`.
  Use the debug profile while iterating; build release only for benchmarks.

## Rules

- The patch rules in ARCHITECTURE.md are binding, especially: no change to
  the main loop, no cost when unused, never block the editor on a client.
- Do not reformat, rename or move upstream code. Do not touch files outside
  this repository. Do not push; commit to `bb-bridge` in small commits with
  clear messages. Do not spawn bb threads; in-session subagents are fine.
- Do not start Helix inside the user's own terminals or change their Helix
  config (`~/.config/helix`). Test with the integration harness and with
  your own instances under a throwaway `HOME`/config where needed.

## Done when

- `cargo build`, the bridge's tests, upstream's existing tests,
  `cargo fmt --check` and clippy on the bridge are clean.
- You have driven a real `hx` built from this branch over the socket (for
  example in a bb terminal you create, or under a pty) and seen every method
  and event of this step work, including two clients at once and a client
  that stops reading.
- The benchmark numbers meet the targets, or you report exactly which do not
  and why.

Final report to the coordinator: what exists, the measured numbers, how you
verified it, every upstream touchpoint, any difference from PROTOCOL.md, and
what you could not verify.
