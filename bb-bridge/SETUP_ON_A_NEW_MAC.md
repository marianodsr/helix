# Setting this up on a new Mac: instructions for an agent

You are an agent asked to set up the owner's Helix-in-bb environment on a Mac
that has never had it. Everything you need is public in two repositories:

- `github.com/marianodsr/helix`, branch `bb-bridge`: a fork of the Helix
  editor with a local socket ("the bridge"). It installs as `hxb`, beside any
  stock `hx`. This file lives in its `bb-bridge/` directory, with the
  [owner's guide](README.md).
- `github.com/marianodsr/bb-plugins`, branch `main`: the bb plugins, among
  them Helix Bridge (`plugins/helix-bridge`), which runs `hxb` inside bb and
  connects it to threads, and Review Editor, which holds the comments made
  in Helix.

The goal: `hxb` runs, bb's Helix Bridge plugin sees it, and `bb hx doctor`
reports no errors. Work through the steps in order and report at the end.

## Before you start

1. Confirm the machine: `uname -sm` should print `Darwin arm64` (Apple
   silicon). On anything else the installer builds from source, which needs
   the Xcode command line tools and Rust and takes several minutes; say so
   and ask before going that way.
2. Check bb: `bb status` should answer. If `bb` is not on `PATH`, the
   installer still installs the editor and skips the plugins; tell the owner
   bb is missing rather than installing it yourself.
3. Look, do not touch: note whether `~/.config/helix`, `~/.config/ghostty/config`
   and `~/.local/bin/hxb` already exist. The installer leaves existing ones
   alone; you should too.

## Install

Preview first, then run it:

```sh
curl -fsSL https://raw.githubusercontent.com/marianodsr/helix/bb-bridge/bb-bridge/install.sh | sh -s -- --dry-run
curl -fsSL https://raw.githubusercontent.com/marianodsr/helix/bb-bridge/bb-bridge/install.sh | sh
```

It needs network access to `raw.githubusercontent.com` and `github.com`, and
writes under `~/.local`, `~/.config` (only where nothing exists) and bb's
plugin directory. If your sandbox blocks those, say what was blocked instead
of working around it.

What it does, and what success looks like:

| Step | Success |
|---|---|
| Downloads the prebuilt archive from the fork's latest release and checks its sha256 | `sha256 ok`, then `installed hxb <version> (<commit>)` |
| Links `~/.local/bin/hxb` | `linked ... -> ~/.local/lib/hxb/<version>-<commit>/hxb` |
| Copies the owner's Helix and Ghostty configuration, only where none exists | `copied ...` or `skipped ...: already there` |
| Installs the Helix Bridge and Review Editor plugins into bb | `bb plugin install ...` for each, or skipped as already installed |
| Runs `bb hx doctor` | every line starts with `ok` or `warn`, none with `error` |

It ends with three lists: done, skipped (with the reason), and still to do by
hand. Read all three. It is safe to run again; a second run changes nothing.

## What is left by hand

The installer prints the exact commands for this machine. Typically:

- **`PATH`**: `~/.local/bin` must be on it for `hxb` to be found in a
  terminal. Adding it means editing the owner's shell profile: show them the
  line and let them decide, unless they asked you to do it.
- **Font**: the Ghostty configuration names FiraCode Nerd Font Mono
  (`brew install --cask font-fira-code-nerd-font`). The editor works without
  it and looks different.
- **Go tools** the Helix configuration expects: `gopls` and `goimports`
  (`go install golang.org/x/tools/gopls@latest` and
  `go install golang.org/x/tools/cmd/goimports@latest`, with `~/go/bin` on
  `PATH`).
- **Key bindings** for the bridge commands are not installed. Suggested ones
  are in `~/.local/lib/hxb/<version>-<commit>/config/keys-bb.example.toml`
  (under `space u`). Changing `~/.config/helix/config.toml` is the owner's
  call: offer the lines, do not add them unasked. Helix rejects a config
  that names a command it does not have, so only offer lines for commands
  this build has (`:bb-status` lists what it offers).

## Verify

```sh
~/.local/bin/hxb --version        # helix <version> (<commit>), the commit of the release
bb hx doctor                      # every line ok or warn, none error
bb plugin list | grep -E "helix-bridge|review-editor"   # both running
```

Then check the bridge itself. Either ask the owner to open the editor in bb
(the **Helix** button in a thread's header) and run `bb hx list`, which
should show one instance bound to that thread; or start `hxb` yourself in a
terminal you control, run `bb hx list` and `bb hx state`, and quit it with
`:q`. `bb hx state` should list ten capabilities: `editor, reload,
locations, views, command, diagnostics, lsp, applyEdits, annotations,
reveal`.

A warning from `bb hx doctor` that no instance is running is expected until
an `hxb` is started.

## Do not

- Overwrite or edit `~/.config/helix`, `~/.config/ghostty` or a
  `~/.local/bin/hxb` the installer did not create. If one of them is in the
  way, report it.
- Replace or uninstall a stock `hx`. `hxb` is separate on purpose.
- Use `--from-source` when a prebuilt archive exists, unless asked.
- Change bb settings other than installing the two plugins. If `bb hx
  doctor` cannot find `hxb`, the fix it prints
  (`bb plugin config helix-bridge set hxbPath <path>`) is the one setting
  you may set.
- Disable a check, or retry a command that a permission system refused.

## If something fails

| Symptom | Likely cause and what to do |
|---|---|
| `has no release yet` or a download error | Network or GitHub access. Check `curl -I https://github.com/marianodsr/helix/releases/latest`. Do not fall back to a source build silently. |
| `sha256` mismatch | The download is corrupt or was altered. Run again once; if it repeats, stop and report. |
| Source build stops asking for Xcode tools or Rust | Not Apple silicon, or `--from-source`. Report what is missing; installing them is the owner's decision. |
| `bb plugin install` fails | Copy the error. bb builds the plugin on install; the usual causes are no network for npm or an older bb than the plugins need (they were built against bb 0.49.0). |
| `bb hx doctor`: editor binary not found | `~/.local/bin` is not on the `PATH` bb's server sees. Set the `hxbPath` setting as above. |
| `bb hx list` shows nothing while `hxb` runs | The bridge is disabled (`HELIX_BRIDGE=0`, or `[editor.bridge] enable = false` in the Helix config), or the editor is a stock `hx`. `:bb-status` inside the editor says. |
| Helix refuses its config at start | A key binding names a command this build lacks, or a theme file has an unknown key. Helix prints the line; report it, do not edit the config. |

## What has and has not been checked

Checked on the machine it was built on (an M1 Pro): the installer from the
real release in an empty home folder, a second run changing nothing, existing
configuration left alone, and the installed editor answering over its socket
with Go highlighting and the owner's theme.

Not yet checked anywhere: the plugin step installing into a real bb for the
first time (it was only run as a dry run), and the message shown when the
Xcode command line tools are missing. If either misbehaves, the full output
is the most useful thing to report.

## Report back

Say, in this order: whether `hxb` runs and its version; the `bb hx doctor`
output; which plugins are installed and running; each item left by hand and
whether the owner wants it done; and anything that did not go as this file
says.

## The other plugins

The same repository has three more bb plugins the owner uses. Install them
only if asked:

```sh
bb plugin install git:https://github.com/marianodsr/bb-plugins.git@main --subdirectory plugins/task-board
bb plugin install git:https://github.com/marianodsr/bb-plugins.git@main --subdirectory plugins/jira-viewer
```

Jira Viewer needs the owner's Jira site, email and API token, entered by them
in bb's settings; never ask for the token in chat.
