# hxb: Helix with the bb bridge

This branch (`bb-bridge` of github.com/marianodsr/helix) is Helix plus a
local socket that lets bb and its agents talk to the editor: send a
selection or a comment to a thread, follow the agent through its changes,
reload what it wrote, read diagnostics and ask the language server. It runs
as `hxb`, beside a stock `hx`, and reads the usual `~/.config/helix`. The bb
side is the Helix Bridge plugin in github.com/marianodsr/bb-plugins.

| Document | |
|---|---|
| [SETUP_ON_A_NEW_MAC.md](SETUP_ON_A_NEW_MAC.md) | step-by-step setup on a new machine, written for an agent |
| [ARCHITECTURE.md](ARCHITECTURE.md) | how it fits together, and why |
| [PROTOCOL.md](PROTOCOL.md) | the socket protocol (version 1) |
| [COMMAND_POLICY.md](COMMAND_POLICY.md) | what an agent may run in the editor |
| [UPSTREAM_TOUCHPOINTS.md](UPSTREAM_TOUCHPOINTS.md) | every change to upstream files, and the fixes carried |
| [config/README.md](config/README.md) | the Helix and Ghostty configuration shipped here |
| [bench/README.md](bench/README.md) | latency benchmarks |

## Install

On a Mac:

```sh
curl -fsSL https://raw.githubusercontent.com/marianodsr/helix/bb-bridge/bb-bridge/install.sh | sh
```

or from a checkout, `bb-bridge/install.sh`. Pass options after `sh -s --`
when piping (`... | sh -s -- --dry-run`):

| Option | |
|---|---|
| `--dry-run` | print what it would do, change nothing |
| `--prefix DIR` | install under `DIR` instead of `~/.local` |
| `--from-source` | build even where a prebuilt archive exists |
| `--archive FILE` | install a local archive (`FILE.sha256` must be beside it) |
| `--no-config` | do not copy the Helix or Ghostty configuration |
| `--no-plugins` | do not install the bb plugins or run `bb hx doctor` |

What it does:

1. **hxb.** On Apple silicon it downloads the archive attached to the
   fork's latest GitHub release and checks its sha256 and every file in it.
   Elsewhere, or with `--from-source`, it builds one with `release.sh`; that
   needs the Xcode command line tools (`xcode-select --install`) and Rust
   (https://rustup.rs), and it says which is missing before changing
   anything. A build from `curl | sh` clones the branch into `~/src/helix-bb`
   (`HXB_SRC` overrides). Each version is unpacked into
   `~/.local/lib/hxb/<version>-<commit>/` and `~/.local/bin/hxb` links to it.
2. **Configuration.** Copies [config/helix](config/helix) to
   `~/.config/helix` and [config/ghostty/config](config/ghostty/config) to
   `~/.config/ghostty/config`, each only when there is none.
3. **bb plugins**, when `bb` is on `PATH`: Helix Bridge and Review Editor
   (comments made in Helix arrive as Review Editor comments), from
   `git:https://github.com/marianodsr/bb-plugins.git@main`, skipping one
   already installed.
4. **`bb hx doctor`**, then a summary: what it did, what it skipped and why,
   and what is left to do by hand.

It is safe to run twice and never deletes or overwrites anything of yours:
an existing configuration, a `~/.local/bin/hxb` it did not make, or an
installed plugin is left alone and named in the summary.

Left to you, as the summary says: `~/.local/bin` on `PATH`, the font
(`brew install --cask font-fira-code-nerd-font`), `gopls` and `goimports`
(`go install golang.org/x/tools/gopls@latest`,
`go install golang.org/x/tools/cmd/goimports@latest`), and key bindings.

### Update

Run the installer again. A new release goes into a directory of its own and
the link moves to it; the previous version stays until you remove it. Update
the plugins with `bb plugin update helix-bridge` and
`bb plugin update review-editor`: the editor and the plugin must speak the
same protocol version, which `bb hx doctor` checks.

### Uninstall

There is no uninstall flag. By hand:

```sh
rm ~/.local/bin/hxb
rm -rf ~/.local/lib/hxb
bb plugin remove helix-bridge          # and review-editor, if you want
```

`~/.config/helix`, `~/.config/ghostty/config` and `~/src/helix-bb` are
yours: delete them only if you want them gone.

### Building a release

`bb-bridge/release.sh` (on an Apple silicon Mac, from a clean tree) builds
`target/hxb-release/hxb-<version>-<commit>-aarch64-apple-darwin.tar.gz`
and its `.sha256`, and prints the `gh release create` command that
publishes them. The installer takes the archive from the release GitHub
marks as latest.

## Commands

All are typed commands (`:bb-…`) and complete on the command line.

| Command | |
|---|---|
| `:bb-send [note]` | the selections (or the cursor line) to the thread's composer, as a draft |
| `:bb-send! [note]` | the same, sent at once |
| `:bb-ask <question>` | ask the thread about the selections |
| `:bb-comment [text]` | a review comment on the selected lines; prompts without text |
| `:bb-comments-send` | send every pending comment to the agent |
| `:bb-status` | socket, connected clients and capabilities |
| `:bb-locations [list]` | pick from a list the agent made (its changes, a tour) |
| `:bb-next`, `:bb-prev` | next or previous item of the active list |
| `:bb-go` | go where the agent asked to take you while you were typing |
| `:bb-annotations` | show or hide the agent's comments drawn in the code |
| `:bb-review [target]` | pick a file of the review bb set (its diff in the preview); with `uncommitted`, `branch`, `turn`, `commit`, `pr` or `off`, start or end one |
| `:bb-diff` | show or hide removed and added lines in place (against the review's base, or `HEAD`) |
| `:bb-emit <name> [words]` | ask bb to do something it names (bind keys to bb's own actions) |
| `:bb-files` | the file tree: bb's own inside bb, otherwise the file manager (yazi) full screen, opening what you pick |

### Key bindings

None are set for you. [config/keys-bb.example.toml](config/keys-bb.example.toml)
suggests a set under `space t`, which Helix leaves free:

```toml
[keys.normal.space.t]
s = ":bb-send"
c = ":bb-comment"
C = ":bb-comments-send"
i = ":bb-status"
n = ":bb-next"
p = ":bb-prev"
```

Copy the lines you want into `~/.config/helix/config.toml`.

## Settings

```toml
[editor.bridge]
enable = true       # listen on the socket (default true)
commands = "safe"   # what editor/command may run: safe (default), all, none
reveal = true       # animate text an agent just wrote (default true)
watch = true        # reload open buffers when their file changes on disk (default true)
inline-diff = false # draw the inline diff from the start, against HEAD (:bb-diff)
# what :bb-files runs outside bb; {chooser} is the file it writes the picked
# paths to, {path} the focused file
file-manager = ["yazi", "--chooser-file", "{chooser}", "{path}"]
```

To open the tree on `space e` (Helix's own file explorer otherwise), bind it
in both modes:

```toml
[keys.normal.space]
e = ":bb-files"

[keys.select.space]
e = ":bb-files"
```

| Environment | |
|---|---|
| `HELIX_BRIDGE=0` | no socket for this run, whatever the config says |
| `HELIX_KITTY_KEYBOARD=enabled\|disabled` | overrides `kitty-keyboard-protocol` at start; the plugin sets `enabled` on the sessions it starts, because bb's terminal answers Helix's capability query itself |

The socket and a registry file are in `~/.cache/helix/bridge/` (or
`$XDG_CACHE_HOME/helix/bridge/`), mode 700, removed when Helix exits.

**Command policy.** Under `safe` an agent can drive the editor but cannot
run a shell command or throw away unsaved work, by any route (typed
commands, keys, macros, `%sh{}`); `all` and `none` are what they say, and a
running editor can only be made stricter. The full rules are in
[COMMAND_POLICY.md](COMMAND_POLICY.md).

## Using it

**Inside bb**: open a thread's Helix tab, or the editor page beside the
chat. The plugin starts `hxb` in a bb terminal session, so the editor is
bound to that thread.

**In any other terminal** (Ghostty, iTerm, …): run `hxb` in the project.
bb's host finds it through the bridge directory and binds it to the latest
thread whose environment contains that workspace; `bb hx bind` binds it to
another thread, `bb hx list` shows what is bound to what. Without bb running,
`hxb` is just Helix.

## Troubleshooting

`bb hx doctor` checks the binary and its version, the protocol, the bridge
directory, `gopls`, `goimports` and the font the Ghostty configuration
names, and prints the command that fixes each missing piece. Then:

- **bb cannot find `hxb`**: bb's server does not have `~/.local/bin` on its
  `PATH`. `bb plugin config helix-bridge set hxbPath ~/.local/bin/hxb`.
- **`:bb-status` says the bridge is off**: `HELIX_BRIDGE=0` in the
  environment, or `enable = false` under `[editor.bridge]`.
- **A key binding makes Helix refuse the config**: it names a `:bb-*`
  command this build does not have; see `:bb-status` for its capabilities.
- **Grammar missing** (no highlighting for a language): the archive ships
  every grammar in `languages.toml`; for a source build, run
  `GRAMMARS=all bb-bridge/grammars.sh` in the checkout.
- `hxb --version` names the commit; `~/.local/lib/hxb/<version>/MANIFEST`
  has the commit and protocol version of what is installed.

## Keeping current with upstream

The branch is upstream `master` plus commits on top; the base is named in
[UPSTREAM_TOUCHPOINTS.md](UPSTREAM_TOUCHPOINTS.md), which lists every hunk
in an upstream file, with the command that regenerates the list. Almost
all of the bridge is new files (`helix-term/src/bridge/`, `bb-bridge/`), so a
rebase conflicts only in those listed lines, nearly all of which add one
line to a list of commands or config fields.

```sh
git fetch upstream
git rebase upstream/master                  # conflicts: UPSTREAM_TOUCHPOINTS.md says what each hunk is for
cargo test --workspace && cargo integration-test
python3 bb-bridge/bench/move_latency.py --check 10
```

Then update the base commit and the line numbers in UPSTREAM_TOUCHPOINTS.md.

**Carried fix.** `runtime/queries/go/highlights.scm` carries a fix for the
upstream Go doc-comment query that made every cursor move in a Go file with
long comments take ~27 ms. It is not bridge code. When upstream's query no
longer has the uncaptured `(comment)*`, take upstream's side of the
conflict; "Fixes carried on top of upstream" in UPSTREAM_TOUCHPOINTS.md has
the check.
