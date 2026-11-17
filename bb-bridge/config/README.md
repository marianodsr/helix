# Helix configuration

`helix/` is the owner's own Helix configuration, committed as it is on their
machine: `config.toml` (editor settings and key bindings), `languages.toml`
and `themes/`. The installer copies it to `~/.config/helix` on a machine that
has no Helix configuration yet, and never overwrites an existing one.

`keys-bb.example.toml` holds suggested bindings for the bridge commands. They
are not part of the owner's configuration until copied into `config.toml`.
