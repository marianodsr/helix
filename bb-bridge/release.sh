#!/bin/sh
# release.sh: build the archive install.sh downloads.
#
#   bb-bridge/release.sh                 # target/hxb-release/hxb-<version>-<commit>-<target>.tar.gz
#   bb-bridge/release.sh --output DIR
#   bb-bridge/release.sh --allow-dirty   # uncommitted changes: commit is marked -dirty
#   GRAMMARS="go rust" bb-bridge/release.sh   # fewer grammars (default: all)
#
# Rebuilds helix-loader from clean so `hx --version` names the commit being
# released (cargo does not rerun its build script when only the commit
# changes in a worktree), builds the release binary, fetches and builds the
# grammars, and packs:
#
#   hxb-<version>-<commit>-<target>/
#     hxb  hx  runtime/  config/  LICENSE  MANIFEST  SHA256SUMS
#
# runtime/ has the queries, themes, tutor and built grammars, without grammar
# sources or .dSYM. config/ is bb-bridge/config, which install.sh copies only
# where the user has none. MANIFEST records the commit, the bridge protocol
# version and the sha256 of hx; SHA256SUMS covers every file. The archive's
# own sha256 goes to <archive>.sha256 beside it.
#
# It prints the commands that publish the release; it does not publish.
set -eu

here=$(cd "$(dirname "$0")" && pwd -P)
root=$(dirname "$here")
out=$root/target/hxb-release
allow_dirty=0
publish_hint=1

while [ $# -gt 0 ]; do
    case $1 in
        --output) [ $# -ge 2 ] || { echo "release.sh: --output needs a directory" >&2; exit 2; }
                  out=${2%/}; shift ;;
        --allow-dirty) allow_dirty=1 ;;
        --no-publish-hint) publish_hint=0 ;;
        -h|--help) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "release.sh: unknown option $1" >&2; exit 2 ;;
    esac
    shift
done

say() { printf 'release.sh: %s\n' "$*"; }
die() { printf 'release.sh: %s\n' "$*" >&2; exit 1; }

case $out in /*) ;; ?*) out=$(pwd -P)/$out ;; *) die "--output needs a directory" ;; esac
command -v cargo >/dev/null 2>&1 || die "cargo is not on PATH (install Rust: https://rustup.rs)"
command -v git >/dev/null 2>&1 || die "git is not on PATH (xcode-select --install)"
cd "$root"

commit=$(git rev-parse HEAD)
short=$(git rev-parse --short=8 HEAD)
label=$short
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    [ "$allow_dirty" = 1 ] || die "uncommitted changes: commit them, or pass --allow-dirty"
    label=$short-dirty.$(date -u +%Y%m%d%H%M%S)
fi
target=$(rustc -vV | sed -n 's/^host: //p')
protocol=$(sed -n 's/^pub const PROTOCOL_VERSION: u32 = \([0-9][0-9]*\);.*/\1/p' helix-term/src/bridge/protocol.rs)
[ -n "$protocol" ] || die "cannot read PROTOCOL_VERSION from helix-term/src/bridge/protocol.rs"

say "building $commit for $target"
cargo clean --release -p helix-loader
cargo build --release --locked
hx=$root/target/release/hx
version_line=$("$hx" --version)
case $version_line in
    *"($short)"*) ;;
    *) die "hx --version says '$version_line', expected commit $short" ;;
esac
version=$(printf '%s\n' "$version_line" | awk '{print $2}')

say "fetching and building grammars (${GRAMMARS:-all})"
GRAMMARS=${GRAMMARS:-all} HX=$hx sh "$here/grammars.sh"

name=hxb-$version-$label-$target
stage=$out/stage
dir=$stage/$name
mkdir -p "$out"
rm -rf "$stage"
mkdir -p "$dir"

cp "$hx" "$dir/hx"
cp "$here/hxb" "$dir/hxb"
chmod 755 "$dir/hx" "$dir/hxb"
rsync -a --exclude '/grammars/sources/' --exclude '*.dSYM' --exclude '.DS_Store' \
    "$root/runtime/" "$dir/runtime/"
rsync -a --exclude '.DS_Store' "$here/config/" "$dir/config/"
cp "$root/LICENSE" "$dir/LICENSE"

# A grammar's install name is the path it was built at on this machine.
# Nothing reads it (Helix loads grammars by path), but it should not ship.
if [ "$(uname -s)" = Darwin ]; then
    for lib in "$dir"/runtime/grammars/*.dylib; do
        [ -e "$lib" ] || continue
        install_name_tool -id "$(basename "$lib")" "$lib" 2>/dev/null ||
            die "install_name_tool failed on $lib"
        codesign --force --sign - "$lib" 2>/dev/null || die "codesign failed on $lib"
    done
fi
grammars=$(find "$dir/runtime/grammars" -name '*.dylib' -o -name '*.so' | wc -l | tr -d ' ')

cat > "$dir/MANIFEST" <<EOF
name=hxb
version=$version
commit=$commit
label=$label
target=$target
protocol=$protocol
grammars=$grammars
built=$(date -u +%Y-%m-%dT%H:%M:%SZ)
sha256-hx=$(shasum -a 256 "$dir/hx" | awk '{print $1}')
EOF
(cd "$dir" && find . -type f | sed 's|^\./||' | LC_ALL=C sort |
    while IFS= read -r f; do shasum -a 256 "$f"; done > "$stage/SHA256SUMS")
mv "$stage/SHA256SUMS" "$dir/SHA256SUMS"

archive=$out/$name.tar.gz
COPYFILE_DISABLE=1 tar -C "$stage" -czf "$archive" "$name"
(cd "$out" && shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256")
rm -rf "$stage"
sha=$(awk '{print $1}' "$archive.sha256")
size=$(du -h "$archive" | awk '{print $1}')

say "built $archive ($size, $grammars grammars)"
say "sha256 $sha"

[ "$publish_hint" = 1 ] || exit 0

remote=$(git remote get-url origin 2>/dev/null || true)
repo=$(printf '%s\n' "$remote" | sed -n 's|.*github\.com[:/]\([^/]*/[^/]*\)$|\1|p' | sed 's/\.git$//')
repo=${repo:-marianodsr/helix}
tag=hxb-$version-$label
notes=$out/$name.notes.md
cat > "$notes" <<EOF
hxb $version ($short): Helix with the bb bridge, protocol $protocol, $target.

Install or update:

    curl -fsSL https://raw.githubusercontent.com/$repo/bb-bridge/bb-bridge/install.sh | sh

sha256 of the archive: \`$sha\`
EOF

if ! git branch -r --contains "$commit" 2>/dev/null | grep -q .; then
    say "note: $short is not on any remote branch yet; push it before publishing"
fi
case $label in *-dirty.*) say "note: built from uncommitted changes; do not publish this one" ;; esac
cat <<EOF

To publish: the commit must be on origin/bb-bridge (install.sh is fetched
from there), then

  git push origin bb-bridge
  gh release create $tag --repo $repo --target $commit \\
      --title "hxb $version ($short)" --notes-file "$notes" --latest \\
      "$archive" "$archive.sha256"
EOF
