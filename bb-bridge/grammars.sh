#!/bin/sh
# Fetch and build the tree-sitter grammars this fork ships with, into the
# checkout's runtime/grammars. Run after `cargo build --release`.
#
#   bb-bridge/grammars.sh            # the list below
#   GRAMMARS="go rust" bb-bridge/grammars.sh
#   GRAMMARS=all bb-bridge/grammars.sh   # every grammar in languages.toml
#
# Uses a throwaway config directory, so the user's languages.toml (and its
# `use-grammars`) is neither read nor changed.
set -eu

here=$(cd "$(dirname "$0")" && pwd -P)
root=$(dirname "$here")
hx=${HX:-$root/target/release/hx}
[ -x "$hx" ] || { echo "grammars.sh: build first: cargo build --release" >&2; exit 1; }

GRAMMARS=${GRAMMARS:-"go gomod gowork gotmpl go-format-string rust rust-format-args \
typescript tsx javascript jsdoc json json5 toml yaml markdown markdown_inline \
bash python proto comment regex diff gitcommit git-rebase git-config gitignore \
gitattributes dockerfile make html css elixir heex eex ruby embedded-template sql hcl lua"}

tmp=$(mktemp -d "${TMPDIR:-/tmp}/hxb-grammars.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/config/helix" "$tmp/cache"
if [ "$GRAMMARS" = all ]; then
    : > "$tmp/config/helix/languages.toml"
else
    list=""
    for g in $GRAMMARS; do list="$list\"$g\", "; done
    printf 'use-grammars = { only = [ %s ] }\n' "$list" > "$tmp/config/helix/languages.toml"
fi

export HELIX_RUNTIME="$root/runtime" XDG_CONFIG_HOME="$tmp/config" XDG_CACHE_HOME="$tmp/cache"
"$hx" --grammar fetch
"$hx" --grammar build
