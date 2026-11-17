# Brief: find and fix the slow cursor moves in wide Go buffers

The owner works in Go all day in this fork of Helix, and it is slow where it
matters: on a 4,100-line Go file (`net/http/server.go` from the Go source), a
cursor move takes about 26 ms in the fork against about 1.9 ms in stock Helix
25.07 (Homebrew `hx`). Your job is to find the cause precisely and fix it in
the fork.

## What is already known (measured by another agent, in a real pty)

- It is not the bridge: the same numbers with the bridge connected, not
  connected, or disabled, and with the owner's config or a throwaway one.
- It depends on the terminal width: the same instance answers in 1.8 ms at 53
  columns and 26 ms at 100 or more columns. The number of rows does not
  matter.
- It depends on Go highlighting: with Go's `highlights.scm` and `locals.scm`
  emptied in a copy of the runtime it is back to 1.9 ms. Emptying
  `injections.scm` does not help. Reverting upstream #15997 (the newest
  change to those queries) does not help.
- A TypeScript file is equally fast in both. Very large Go and TypeScript
  files were reported under 5 ms p95 in another measurement, so check what
  about this file or this width triggers it.
- The fork's base is upstream master `49986e667`, 1011 commits after the
  `25.07.1` tag. The step 1 key-press benchmark (integration harness, no real
  terminal) did not show it.
- Notes and drivers from that measurement:
  `/Users/marianodsr/projects/bb-plugins/plugins/helix-bridge/spike/RESULTS.md`
  (last sections) and the scripts beside it. `bb-bridge/bench/` in this repo
  has `live.py` (drives `hx` under a pty with a throwaway HOME).

## Do this

1. **Reproduce without any of the bridge**, with a small script that starts a
   given `hx` binary under a pty at a given size with a throwaway `HOME`
   and config, opens the Go file, sends N cursor moves one at a time and
   measures the time from each key to the end of the output it causes.
   Put it in `bb-bridge/bench/` (for example `move_latency.py`): it becomes
   the regression check. Confirm the numbers above for: Homebrew `hx` 25.07,
   the fork's release build, and the widths 53 / 100 / 140.
2. **Rule out the build.** Build tag `25.07.1` and upstream `49986e667` from
   source with the same release profile and measure both. If `25.07.1` built
   here is fast and `49986e667` is slow, it is a code regression.
3. **Bisect** between them with `git bisect run` and your script (the gap is
   more than tenfold, so it is robust even on a busy machine). Rebuild only
   what each step needs. Mind that the runtime queries and grammars changed
   along the way: use each commit's own `runtime/` queries with the grammars
   already built in `/Users/marianodsr/projects/helix-bb/runtime/grammars`,
   and if a step fails to build or load, `git bisect skip` it.
4. **Understand it.** Name the commit, what it changed, and why width and
   this file trigger it (profile a release build with debug symbols if that
   helps: `sample`, or `cargo flamegraph` is not installed, so `sample <pid>`
   and reading the hot frames is the simple route).
5. **Check upstream** before writing a fix: search helix-editor/helix issues
   and pull requests (`gh` is logged in) for the same symptom and for a fix
   already merged after `49986e667` or open. Prefer taking upstream's fix.
6. **Fix it in the fork** on your branch, in the smallest way that is
   correct: cherry-pick upstream's fix, or revert the offending change, or
   patch it. Record it in `bb-bridge/UPSTREAM_TOUCHPOINTS.md` (a new section
   "Fixes carried on top of upstream") with the reason and how to drop it
   when upstream has it. Do not paper over it by disabling highlighting.
7. **Prove it**: the script's numbers before and after for Go at all three
   widths and for TypeScript, upstream's integration suite still green, and
   no change in how the file is highlighted (compare a rendered screen
   before and after).

## Rules

- You work in your own git worktree. Build in `target/` inside it, seeded
  with `cp -c -R /Users/marianodsr/projects/helix-bb/target ./target` so the
  dependencies do not recompile; never set `CARGO_TARGET_DIR` to a directory
  another worktree uses. `PATH=$HOME/.cargo/bin:$PATH`, `CARGO_BUILD_JOBS=4`,
  `HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1`, `--offline` where it works (older
  commits may need dependencies that are not fetched; fetching from
  crates.io is fine). Cargo needs the sandbox disabled.
- Another agent is building on this machine too; one cargo command at a time.
- The owner is using the fork right now: never start instances in the real
  bridge directory (`~/.cache/helix/bridge`) or with the real `HOME`; never
  write to `~/.config/helix`, `~/.config/ghostty`, `~/.local/bin`, or
  `/Users/marianodsr/projects/helix-bb/try/`.
- Commit on your worktree's branch. Do not push, do not merge, do not spawn
  bb threads.

If the bisect shows something that makes a small fix impossible (for example
the cost is spread over a redesign), stop and report the options with their
costs, including rebasing the bridge onto `25.07.1`.

Report: the cause in two sentences, the commit, the fix, the numbers before
and after, what upstream has, and anything you could not verify.
