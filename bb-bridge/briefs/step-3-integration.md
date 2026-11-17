# Brief: integration of the bridge

You are integrating the Helix bridge on the main branch. Work in
`/Users/marianodsr/projects/helix-bb` itself, on branch `bb-bridge` (not a
worktree). Step 1 and steps 2b (diagnostics, language-server queries) and 2c
(panes, `editor/command`, not yanking the user) are merged. Step 2d
(annotations, reveal) is finished on its branch and conflicts in five files.
Step 2a (reload, locations, applyEdits) is still being built by another agent
in its own worktree; the coordinator will tell you when it is ready.

Read `bb-bridge/ARCHITECTURE.md` (especially "What the user asked for", the
patch rules, and "Integration step"), `bb-bridge/PROTOCOL.md`,
`bb-bridge/PROTOCOL_NOTES.md`, `bb-bridge/UPSTREAM_TOUCHPOINTS.md`,
`bb-bridge/COMMAND_POLICY.md` and `bb-bridge/briefs/step-2-common.md`, then
the bridge code.

## Work, in this order

1. **Merge 2d**: `bb/helix-bridge-annotations-and-reveal-2d-thr_9ax9h5amfp`.
   The conflicts are in `PROTOCOL_NOTES.md`, `UPSTREAM_TOUCHPOINTS.md`,
   `bench/live.py`, `tests/integration.rs` and `tests/test/bridge.rs`; keep
   both sides' content. Regenerate `COMMAND_POLICY.md`
   (`BRIDGE_UPDATE_POLICY=1`) and check the new `:bb-*` commands get a
   sensible verdict (commands that speak to bb in the user's name stay
   refused under `safe`).
2. **No redraw for read-only requests** ("Integration step" item 1). Each
   method declares whether it can change the screen; the redraw after a
   dispatched job is skipped when the job was a read-only bridge request and
   nothing else asked for a redraw. One condition in `application.rs`, added
   to the touchpoints list. Prove it with numbers on a large file (a 20k-line
   Go file is in the benchmarks): `editor/state` and a cheap `lsp/query`
   round trip before and after, and that mutating methods still repaint.
3. **Kitty keyboard from the environment** (item 3):
   `HELIX_KITTY_KEYBOARD=enabled|disabled` overrides the
   `kitty-keyboard-protocol` setting at start. Smallest possible touchpoint.
4. **Flaky timing assertions.** Two step 1 tests assert absolute times that
   fail on a loaded machine (1000 key presses under 5 s in a debug build;
   the stuck-client check's 50 ms round trip). Make them robust (compare
   against a baseline measured in the same run, or assert the property
   without a wall-clock bound) without weakening what they prove.
5. **Limits reported by 2d**: an annotation on the last line of a file with
   no final newline is cut off when that line is at the bottom of the view;
   during a reveal, soft-wrap arrows and indent guides of covered lines show
   early. Fix the first; fix the second if it is cheap.
6. **When 2a is ready** (the coordinator will message you): merge it, then
   wire the pieces ("Integration step" item 2): `editor/reload` with `reveal`
   calls `reveal::play` with the ranges 2a kept; `locations/set` `goto`,
   `editor/reload` `follow` and `editor/reveal` `follow` go through
   `interrupt::jump` from 2c. Regenerate the command policy again.
7. **Verify everything together**: build, all bridge unit and integration
   tests, upstream's integration suite, fmt, clippy, and `live.py verify`
   under a pty with a release build. Every capability must be in
   `bridge/hello`.
8. **Refresh the owner's build** after step 7 passes, and again after later
   verified changes: build release here and replace the binary atomically
   (`cp target/release/hx try/hx.new && mv try/hx.new try/hx`; also copy
   `bb-bridge/hxb` to `try/hxb` if it changed). The owner runs `try/hxb` all
   the time, so never leave a half-written or unverified binary there.
9. **Benchmarks**: when the coordinator tells you the machine is quiet, run
   every row of the performance table in ARCHITECTURE.md against the base
   commit, plus `lsp/query` overhead on a large Go repository.

## Rules

- Build in this checkout's own `target/` with `PATH=$HOME/.cargo/bin:$PATH`,
  `CARGO_BUILD_JOBS=4`, `HELIX_RUNTIME=/Users/marianodsr/projects/helix-bb/runtime`,
  `HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1`, `--offline`, sandbox disabled for
  cargo. Never set `CARGO_TARGET_DIR` to a directory another worktree uses.
- The owner is using the bridge now: never connect to or start instances in
  the real bridge directory (`~/.cache/helix/bridge`); use a throwaway
  `HOME`/`XDG_*` as `live.py` does. Never write to `~/.config/helix`,
  `~/.config/ghostty` or `~/.local/bin`. In `try/`, touch only what step 8
  says.
- The patch rules in ARCHITECTURE.md are binding. Every upstream edit is
  listed in UPSTREAM_TOUCHPOINTS.md with its reason.
- Commit in small commits on `bb-bridge`. Do not push (the coordinator
  pushes). Do not spawn bb threads.

Report after step 7 (and tell the coordinator at once if something blocks
you): what is merged and wired, what you verified and how, numbers, anything
that differs from PROTOCOL.md, and what is left.
