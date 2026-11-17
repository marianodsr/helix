#!/bin/sh
# Key-press benchmark: this branch (bridge off / listening with no client /
# one client subscribed to everything) against the base commit.
#
#   bb-bridge/bench/keypress.sh [BASE_COMMIT]     (default 49986e667)
#
# Release builds in their own target directories, so target/release/hx is
# left alone. The base commit gets a detached worktree with this benchmark
# copied in (bridge lines removed). Both use this checkout's runtime/ so the
# Go grammar is the same. Each tree runs twice, alternating, to spread drift
# on a shared machine.
set -eu

here=$(cd "$(dirname "$0")" && pwd -P)
root=$(cd "$here/../.." && pwd -P)
base=${1:-49986e667}
tmp=${TMPDIR:-/tmp}
wt=$tmp/helix-bench-base
export HELIX_RUNTIME="$root/runtime" HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}

if [ ! -d "$wt" ]; then
    git -C "$root" worktree add --detach "$wt" "$base"
fi
sed '/bridge:begin/,/bridge:end/d' "$here/keypress.rs" > "$wt/helix-term/tests/test/keypress_bench.rs"
if ! grep -q 'mod keypress_bench;' "$wt/helix-term/tests/integration.rs"; then
    sed -i.orig 's/^    mod splits;$/    mod splits;\
    mod keypress_bench;/' "$wt/helix-term/tests/integration.rs"
fi

bench() { # dir target-dir label
    (cd "$1" && CARGO_TARGET_DIR="$2" cargo test --release --features integration \
        -p helix-term --test integration keypress_bench -- --ignored --nocapture --test-threads=1) \
        2>/dev/null | sed -n "s/^KEYPRESS /$3 /p"
}

for round in 1 2; do
    bench "$root" "$root/target/bench" "branch"
    bench "$wt" "$root/target/bench-base" "base"
done
