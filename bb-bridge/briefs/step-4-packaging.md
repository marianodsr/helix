# Brief: make the fork and plugin installable on another Mac

The bridge is built and verified. The owner will install it on a second Mac
(Apple silicon, an M5) and wants one command to do it. Read "Distribution:
installing on another machine" in `bb-bridge/ARCHITECTURE.md`; it is the
specification. Also read `bb-bridge/config/README.md`, `bb-bridge/hxb`,
`bb-bridge/grammars.sh` and
`/Users/marianodsr/projects/bb-plugins/docs/HELIX_BRIDGE.md` ("Installable on
another machine").

You work in your own git worktree of the fork, branched from `bb-bridge`.

## Deliver

1. **`bb-bridge/release.sh`**: builds the archive the installer downloads.
   Clean rebuild of `helix-loader` so `hx --version` carries the commit being
   released (it currently shows a stale hash because cargo does not rerun
   that build step), release build, all grammars fetched and built, then an
   archive `hxb-<version>-<commit>-aarch64-apple-darwin.tar.gz` holding
   `hxb`, `hx`, `runtime/` (no grammar sources, no `.dSYM`), the licence,
   and a manifest with the commit, protocol version and a sha256. It prints
   the `gh release` command to publish it; it does not publish.
2. **`bb-bridge/install.sh`**: the one command, exactly as ARCHITECTURE.md
   describes (prebuilt archive from the fork's latest GitHub release when the
   machine is Apple silicon, otherwise or with `--from-source` a source
   build with clear messages about Xcode command line tools and Rust;
   `hxb` under `~/.local`; Helix and Ghostty configuration copied only where
   none exists; the bb plugins installed when `bb` is present; `bb hx
   doctor` at the end). Requirements:
   - Runs with `curl -fsSL <raw url>/install.sh | sh` and from a checkout.
   - Safe to run twice; never deletes or overwrites user files; verifies
     the archive's sha256; `--prefix`, `--from-source`, `--no-plugins`,
     `--no-config`, `--archive <file>` (install from a local archive) and
     `--dry-run` (print what it would do).
   - Installs the plugins from
     `git:https://github.com/marianodsr/bb-plugins.git@main` with
     `--subdirectory plugins/helix-bridge` and `plugins/review-editor`
     (comments made in Helix become Review Editor comments), skipping any
     already installed.
   - Says plainly what it did, what it skipped and why, and what the owner
     must still do by hand (for example the font, `gopls`, `goimports`,
     adding `~/.local/bin` to `PATH`, copying key bindings from
     `keys-bb.example.toml`).
   - An `uninstall` note in the README rather than a destructive flag.
3. **`bb-bridge/README.md`**: the owner's guide. What this fork is, install,
   update, the commands (`:bb-*`), suggested key bindings, the
   `[editor.bridge]` settings and `HELIX_BRIDGE` / `HELIX_KITTY_KEYBOARD`,
   the command policy in two lines with a pointer to COMMAND_POLICY.md,
   using it inside bb and in any other terminal, troubleshooting with
   `bb hx doctor`, and how the fork is kept current with upstream (rebase
   notes, the touchpoints list, the carried Go query fix). Plain and short;
   link to the other documents instead of repeating them.
4. **Proof.** Run the installer for real in a throwaway `HOME` on this
   machine with `PATH` stripped of Rust and of the existing `hxb`: from a
   local archive, a second run (nothing changes), with an existing Helix
   config (left alone), `--dry-run`, and the source path at least as far as
   its tool checks. Start the installed `hxb` under a pty with that `HOME`
   and check `bridge/hello` over its socket, and that Go highlighting and a
   theme load. Do not install plugins into the owner's real bb during tests:
   use `--no-plugins` and check the plugin commands with `--dry-run`.

## Rules

- **Do not run cargo, grammar builds or anything else heavy until the
  coordinator tells you the benchmark is finished.** Another agent is
  measuring performance on this machine now. Write and review the scripts
  and the README first; that is most of the work.
- When you may build: own `target/` in your worktree, seeded with
  `cp -c -R /Users/marianodsr/projects/helix-bb/target ./target`;
  `PATH=$HOME/.cargo/bin:$PATH`, `CARGO_BUILD_JOBS=4`; cargo and the grammar
  fetch need the sandbox disabled and network access to GitHub and other
  git hosts.
- No absolute path from this machine in anything that ships.
- The owner is using the fork: never touch `~/.local/bin/hxb`,
  `/Users/marianodsr/projects/helix-bb/try/`, `~/.config/helix`,
  `~/.config/ghostty`, the real bridge directory, or their bb plugins and
  settings.
- Do not publish a release, push, or spawn bb threads. Commit on your
  branch. Leave the archive you built in your worktree and say where it is.

Report: what exists, the proof runs with their output summarised, the
archive's path, size and sha256, the exact commands the coordinator should
run to publish, and anything you could not verify.
