# Step 2: shared rules for the three parallel pieces

Step 1 (socket, core methods, events, commands, tests, benchmarks) is merged
on `bb-bridge`. Step 2 is three pieces built at the same time by three agents,
each in its own git worktree branched from `bb-bridge`:

- 2a: reload, locations, applyEdits
- 2b: diagnostics and language-server queries
- 2c: panes, selections, `editor/command`, not yanking the user

Read `bb-bridge/ARCHITECTURE.md`, `bb-bridge/PROTOCOL.md`,
`bb-bridge/PROTOCOL_NOTES.md` (interpretations from step 1; they are part of
the contract) and `bb-bridge/UPSTREAM_TOUCHPOINTS.md`, then the step 1 code in
`helix-term/src/bridge/` and its tests, before writing anything.

## Staying out of each other's way

- Step 1 left a stub file for every method group, each with its own
  capability constant, typed-command list and hook registration. Fill in
  **only the files your brief names**. In shared files (`dispatch.rs`,
  `mod.rs`, `protocol.rs`, `events.rs`) add lines for your methods and types
  and change nothing else; never reorder or reformat. The coordinator merges
  the three branches, and small additive edits merge cleanly.
- Put your serde types next to your methods (in your own file) unless they
  are already in `protocol.rs`.
- New upstream touchpoints need a line in `UPSTREAM_TOUCHPOINTS.md` and a
  reason. Prefer `helix_event::register_hook!`.
- Extend `bb-bridge/bench/live.py verify` with checks for your methods, in
  your own clearly marked section, and add integration tests in a new file
  under `helix-term/tests/test/` (for example `bridge_reload.rs`) rather than
  growing `bridge.rs`.

## Building on this machine

```sh
export PATH=$HOME/.cargo/bin:$PATH
export CARGO_TARGET_DIR=/Users/marianodsr/projects/helix-bb/target   # shared by all three
export CARGO_BUILD_JOBS=4
export HELIX_RUNTIME=/Users/marianodsr/projects/helix-bb/runtime     # grammars are built there
export HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1
```

- The target directory is shared so dependencies compile once. Cargo locks
  it: when another agent is building you will see "Blocking waiting for file
  lock on build directory". Wait; do not work around it.
- Cargo writes outside your worktree, so run it with the sandbox disabled.
  Add `--offline`; dependencies are already fetched.
- Debug profile while iterating. Release only for the benchmark rows that
  belong to your piece.
- The machine has 8 cores and 16 GB and the owner is using it. One heavy
  command at a time.

## The owner is using the bridge right now

They run the fork as `hxb` with their real configuration, connected to bb.

- Never connect to, signal or start instances in the real bridge directory
  (`~/.cache/helix/bridge`). Run every instance you start with a throwaway
  `HOME` or `XDG_CACHE_HOME` and `XDG_CONFIG_HOME`, as `live.py` does.
- Never write to `~/.config/helix` or `~/.config/ghostty`.
- Do not touch `/Users/marianodsr/projects/helix-bb/try/` or
  `~/.local/bin/hxb`.

## Done when

- `cargo build`, your tests, the step 1 bridge tests, upstream's integration
  suite, `cargo fmt --check` and clippy on the bridge are clean.
- A real `hx` from your branch passes your `live.py` checks under a pty.
- Your capability appears in `bridge/hello` and the stub answer is gone.
- The performance rules in ARCHITECTURE.md still hold: no cost when no
  client is subscribed, nothing slow on the main thread, never block the
  editor on a client. State what you measured.

Commit in small commits on your worktree's branch. Do not push, do not merge,
do not spawn bb threads. Final report to the coordinator: what exists, how
you verified it, numbers, every shared-file and upstream edit, anything that
differs from PROTOCOL.md (record it in PROTOCOL_NOTES.md too), and what you
could not verify.
