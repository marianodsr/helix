# Helix configuration

`helix/` is the owner's own Helix configuration, committed as it is on their
machine: `config.toml` (editor settings and key bindings), `languages.toml`
and `themes/`. The installer copies it to `~/.config/helix` on a machine that
has no Helix configuration yet, and never overwrites an existing one.

`ghostty/config` is the owner's Ghostty configuration, also as it is on their
machine. The bb editor surface reads the Ghostty configuration of the machine
it runs on to match font, size and Option-as-Alt, so the installer copies this
file to `~/.config/ghostty/config` under the same rule: only when none exists.
It names the font FiraCode Nerd Font Mono, which must be installed for the
editor to look the same (`brew install --cask font-fira-code-nerd-font`).

`keys-bb.example.toml` holds suggested bindings for the bridge commands. They
are not part of the owner's configuration until copied into `config.toml`.
