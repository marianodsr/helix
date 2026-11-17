#!/bin/sh
# install.sh: install hxb (Helix with the bb bridge) and its bb plugins.
#
#   curl -fsSL https://raw.githubusercontent.com/marianodsr/helix/bb-bridge/bb-bridge/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --dry-run
#   bb-bridge/install.sh [options]              # from a checkout
#
# Options:
#   --prefix DIR      install under DIR (default ~/.local): DIR/bin/hxb and
#                     DIR/lib/hxb/<version>-<commit>/
#   --from-source     build from source even where a prebuilt archive exists
#   --archive FILE    install from a local archive (FILE.sha256 must be beside it)
#   --no-config       do not copy the Helix or Ghostty configuration
#   --no-plugins      do not install the bb plugins (and skip bb hx doctor)
#   --dry-run         print what it would do, change nothing
#
# On Apple silicon it installs the prebuilt archive from the fork's latest
# GitHub release; otherwise, or with --from-source, it builds from source
# (needs the Xcode command line tools and Rust). It copies the Helix and
# Ghostty configuration only where none exists, installs the bb plugins when
# `bb` is present and runs `bb hx doctor`. Safe to run twice: it never
# deletes or overwrites anything of yours. Each version goes in a directory
# of its own; DIR/bin/hxb is a link it only moves if it made it.

set -eu

REPO=${HXB_REPO:-marianodsr/helix}
BRANCH=${HXB_BRANCH:-bb-bridge}
PLUGINS_SOURCE=git:https://github.com/marianodsr/bb-plugins.git@main
PLUGINS="helix-bridge review-editor"
TARGET=aarch64-apple-darwin

done_list=""
skip_list=""
todo_list=""
did() { done_list="$done_list
  - $*"; if [ "$dry_run" = 1 ]; then printf '    would be done: %s\n' "$*"; else printf '    done: %s\n' "$*"; fi; }
skipped() { skip_list="$skip_list
  - $*"; printf '    skipped: %s\n' "$*"; }
todo() { todo_list="$todo_list
  - $*"; }
say() { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

# Run a command that changes something, or print it under --dry-run.
act() {
    if [ "$dry_run" = 1 ]; then
        printf '    would run: %s\n' "$*"
    else
        "$@"
    fi
}

usage() {
    if [ -f "$1" ]; then
        sed -n '2,24p' "$1" | sed 's/^# \{0,1\}//'
    else
        say "Options: --prefix DIR, --from-source, --archive FILE, --no-config, --no-plugins, --dry-run"
        say "See https://github.com/$REPO/blob/$BRANCH/bb-bridge/README.md"
    fi
}

apple_silicon() {
    # uname -m says x86_64 under Rosetta; the sysctl knows better.
    [ "$(uname -s)" = Darwin ] || return 1
    [ "$(uname -m)" = arm64 ] || [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]
}

manifest_get() { sed -n "s/^$1=//p" "$2" | head -n 1; }

sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }

# ---------------------------------------------------------------------------
# Getting an archive

# Sets release_url to the archive of the latest release for $TARGET, or
# empty and release_why to the reason.
find_release() {
    release_url=""
    api=https://api.github.com/repos/$REPO/releases/latest
    code=$(curl -sSL -o "$work/release.json" -w '%{http_code}' \
        -H 'Accept: application/vnd.github+json' "$api" 2>/dev/null) || code=000
    case $code in
        200) ;;
        404) release_why="$REPO has no release yet"; return 0 ;;
        000) release_why="cannot reach api.github.com"; return 0 ;;
        *) release_why="api.github.com answered $code"; return 0 ;;
    esac
    release_url=$(sed -n 's/.*"browser_download_url": *"\([^"]*-'"$TARGET"'\.tar\.gz\)".*/\1/p' \
        "$work/release.json" | head -n 1)
    release_why="the latest release of $REPO has no $TARGET archive"
}

download_release() {
    say "    downloading $release_url"
    curl -fL --progress-bar -o "$work/$(basename "$release_url")" "$release_url" ||
        die "download failed: $release_url"
    curl -fsSL -o "$work/$(basename "$release_url").sha256" "$release_url.sha256" ||
        die "the release has no $(basename "$release_url").sha256; refusing an archive that cannot be checked"
    archive=$work/$(basename "$release_url")
}

check_build_tools() {
    missing=""
    if [ "$(uname -s)" = Darwin ]; then
        if ! xcode-select -p >/dev/null 2>&1; then
            missing="$missing
    - Xcode command line tools (compiler, git): run  xcode-select --install"
        fi
    else
        command -v cc >/dev/null 2>&1 || missing="$missing
    - a C compiler (cc): install your system's build tools"
        command -v git >/dev/null 2>&1 || missing="$missing
    - git"
    fi
    if ! command -v cargo >/dev/null 2>&1 && [ -x "$HOME/.cargo/bin/cargo" ]; then
        PATH=$HOME/.cargo/bin:$PATH
        export PATH
    fi
    if ! command -v cargo >/dev/null 2>&1; then
        missing="$missing
    - Rust: run  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
      then open a new terminal (or run  . \"\$HOME/.cargo/env\")"
    elif ! command -v rustup >/dev/null 2>&1; then
        say "    note: cargo without rustup; the build needs Rust $(rust_channel) or newer"
    fi
}

rust_channel() {
    if [ -n "$checkout" ]; then
        sed -n 's/^channel *= *"\(.*\)"/\1/p' "$checkout/rust-toolchain.toml" 2>/dev/null
    else
        echo "(see rust-toolchain.toml)"
    fi
}

# Builds an archive from source with release.sh; sets $archive.
build_from_source() {
    check_build_tools
    if [ -n "$missing" ]; then
        say "    building from source needs:$missing"
        if [ "$dry_run" = 1 ]; then
            say "    (a real run stops here, before changing anything)"
            todo "install the build tools listed above, then run this again"
            archive=""
            return 0
        fi
        die "install what is listed above, then run this again. Nothing was changed."
    fi
    say "    build tools: Xcode command line tools and $(cargo --version)"

    if [ -n "$checkout" ]; then
        src=$checkout
        say "    building this checkout as it is: $src ($(git -C "$src" rev-parse --short=8 HEAD))"
    else
        src=${HXB_SRC:-$HOME/src/helix-bb}
        if [ -d "$src/.git" ]; then
            if [ -n "$(git -C "$src" status --porcelain --untracked-files=no)" ]; then
                say "    $src has uncommitted changes: building it as it is, not updating it"
                skipped "updating $src: it has uncommitted changes"
            elif [ "$(git -C "$src" rev-parse --abbrev-ref HEAD)" != "$BRANCH" ]; then
                say "    $src is not on $BRANCH: building it as it is, not updating it"
                skipped "updating $src: it is not on $BRANCH"
            else
                act git -C "$src" pull --ff-only --quiet origin "$BRANCH" ||
                    die "cannot fast-forward $src to origin/$BRANCH; update it by hand"
            fi
        elif [ -e "$src" ]; then
            die "$src exists and is not a git checkout; set HXB_SRC to another directory"
        else
            act mkdir -p "$(dirname "$src")"
            act git clone --quiet --branch "$BRANCH" "https://github.com/$REPO.git" "$src"
            did "cloned $REPO ($BRANCH) into $src"
        fi
    fi

    if [ "$dry_run" = 1 ]; then
        say "    would build: sh $src/bb-bridge/release.sh --allow-dirty (release binary and all grammars; several minutes)"
        archive=""
        return 0
    fi
    say "    building (release binary and all grammars; this takes several minutes)"
    sh "$src/bb-bridge/release.sh" --allow-dirty --no-publish-hint --output "$work/build" ||
        die "the build failed; the output above says why. Nothing was installed."
    for archive in "$work"/build/hxb-*.tar.gz; do break; done
    did "built hxb from $src"
}

# ---------------------------------------------------------------------------
# Installing an archive

install_archive() {
    [ -f "$archive.sha256" ] ||
        die "no $(basename "$archive").sha256 beside $archive; refusing an archive that cannot be checked (release.sh writes one)"
    expected=$(awk '{print $1; exit}' "$archive.sha256")
    actual=$(sha256_of "$archive")
    [ "$expected" = "$actual" ] || die "sha256 mismatch for $archive:
    expected $expected
    got      $actual"
    say "    sha256 ok ($actual)"

    if tar -tzf "$archive" | grep -Eq '^/|(^|/)\.\.(/|$)'; then
        die "$archive has absolute or .. paths; refusing it"
    fi
    top=$(tar -tzf "$archive" | head -n 1 | cut -d/ -f1)
    case $top in hxb-*) ;; *) die "$archive does not look like an hxb archive (top directory '$top')" ;; esac

    lib=$prefix/lib/hxb
    if [ "$dry_run" = 1 ]; then
        mkdir -p "$work/peek"
        tar -xzf "$archive" -C "$work/peek" "$top/MANIFEST"
        manifest=$work/peek/$top/MANIFEST
    else
        mkdir -p "$lib"
        incoming=$lib/.incoming-$$
        mkdir "$incoming"
        tar -xzf "$archive" -C "$incoming"
        manifest=$incoming/$top/MANIFEST
    fi
    [ -f "$manifest" ] || die "$archive has no MANIFEST"
    version=$(manifest_get version "$manifest")
    label=$(manifest_get label "$manifest")
    protocol=$(manifest_get protocol "$manifest")
    target=$(manifest_get target "$manifest")
    [ -n "$version" ] && [ -n "$label" ] || die "$archive has an incomplete MANIFEST"
    say "    hxb $version ($label), protocol $protocol, $target"
    case $(uname -s)/$target in
        Darwin/*-apple-darwin | Linux/*-linux-*) ;;
        *) die "$archive is for $target, not this machine ($(uname -s) $(uname -m))" ;;
    esac
    dest=$lib/$version-$label

    if [ "$dry_run" = 1 ]; then
        if [ -d "$dest" ]; then
            skipped "hxb $version ($label): already installed in $dest"
        else
            say "    would unpack to $dest (after checking every file against SHA256SUMS)"
            did "installed hxb $version ($label) in $dest"
        fi
        config_src=${checkout:+$checkout/bb-bridge/config}
        [ -d "$dest/config" ] && config_src=$dest/config
        return 0
    fi

    pkg=$incoming/$top
    (cd "$pkg" && shasum -a 256 -c SHA256SUMS >/dev/null 2>&1) ||
        die "files in $archive do not match its SHA256SUMS"
    [ "$(sha256_of "$pkg/hx")" = "$(manifest_get sha256-hx "$manifest")" ] ||
        die "hx in $archive does not match its MANIFEST"
    # A browser download is quarantined, and tar passes that on to every file
    # it unpacks; these files were just checked, so clear it on them.
    xattr -dr com.apple.quarantine "$pkg" 2>/dev/null || true
    # In a scratch HOME: even --version creates ~/.config/helix and ~/.cache/helix.
    got=$(env HOME="$work/home" XDG_CONFIG_HOME="$work/home/.config" XDG_CACHE_HOME="$work/home/.cache" \
        "$pkg/hxb" --version 2>&1) || die "the unpacked hxb does not run: $got"
    say "    $got"

    if [ -d "$dest" ]; then
        rm -rf "$incoming"
        skipped "hxb $version ($label): already installed in $dest"
    else
        mv "$pkg" "$dest"
        rm -rf "$incoming"
        did "installed hxb $version ($label) in $dest"
    fi
    config_src=$dest/config
}

link_hxb() {
    bin=$prefix/bin/hxb
    want=$dest/hxb
    if [ -L "$bin" ]; then
        cur=$(readlink "$bin")
        case $cur in
            "$want")
                skipped "$bin already points at this version" ;;
            "$prefix/lib/hxb/"*)
                act ln -sfn "$want" "$bin"
                did "pointed $bin at $want (was $cur; that version stays installed, remove it by hand when you no longer need it)" ;;
            *)
                skipped "$bin is your own link to $cur: left alone"
                todo "to use this install instead: ln -sfn $want $bin" ;;
        esac
    elif [ -e "$bin" ]; then
        skipped "$bin exists and is not a link: left alone"
        todo "to use this install, move $bin aside and run: ln -s $want $bin"
    else
        act mkdir -p "$prefix/bin"
        act ln -s "$want" "$bin"
        did "linked $bin -> $want"
    fi
}

check_path() {
    bin=$prefix/bin/hxb
    case ":$PATH:" in
        *":$prefix/bin:"*)
            found=$(command -v hxb 2>/dev/null || true)
            if [ -n "$found" ] && [ "$found" != "$bin" ]; then
                todo "another hxb comes first on PATH ($found); remove it or put $prefix/bin before it"
            fi ;;
        *)
            todo "add $prefix/bin to PATH: echo 'export PATH=\"$prefix/bin:\$PATH\"' >> ~/.zprofile, then open a new terminal" ;;
    esac
}

# ---------------------------------------------------------------------------
# Configuration

install_config() {
    if [ "$no_config" = 1 ]; then
        skipped "configuration: --no-config"
        return 0
    fi
    if [ ! -d "$config_src/helix" ]; then
        if [ "$dry_run" = 1 ]; then
            say "    would copy the Helix and Ghostty configuration from the archive where none exists"
            return 0
        fi
        skipped "configuration: none in ${config_src:-the archive}"
        return 0
    fi
    config_home=${XDG_CONFIG_HOME:-$HOME/.config}

    # Any hx run creates an empty ~/.config/helix; that is no configuration.
    if [ -d "$config_home/helix" ] && [ -z "$(ls -A "$config_home/helix")" ]; then
        act cp -R "$config_src/helix/." "$config_home/helix"
        did "copied the Helix configuration into $config_home/helix (it existed but was empty)"
    elif [ -e "$config_home/helix" ]; then
        skipped "$config_home/helix exists: left alone (the fork's is in $config_src/helix to compare)"
    else
        act mkdir -p "$config_home"
        act cp -R "$config_src/helix" "$config_home/helix"
        did "copied the Helix configuration to $config_home/helix"
    fi

    ghostty=""
    for f in "$config_home/ghostty/config" "$config_home/ghostty/config.ghostty" \
        "$HOME/Library/Application Support/com.mitchellh.ghostty/config" \
        "$HOME/Library/Application Support/com.mitchellh.ghostty/config.ghostty"; do
        if [ -e "$f" ]; then ghostty=$f; break; fi
    done
    if [ -n "$ghostty" ]; then
        skipped "Ghostty configuration exists ($ghostty): left alone (the fork's is $config_src/ghostty/config)"
    else
        act mkdir -p "$config_home/ghostty"
        act cp "$config_src/ghostty/config" "$config_home/ghostty/config"
        did "copied the Ghostty configuration to $config_home/ghostty/config"
        ghostty=$config_home/ghostty/config
    fi

    font=""
    [ -f "$ghostty" ] && font=$(sed -n 's/^font-family *= *"\{0,1\}\([^"]*\)"\{0,1\} *$/\1/p' "$ghostty" | head -n 1)
    [ "$dry_run" = 1 ] && [ ! -f "$ghostty" ] && font=$(sed -n 's/^font-family *= *"\{0,1\}\([^"]*\)"\{0,1\} *$/\1/p' "$config_src/ghostty/config" 2>/dev/null | head -n 1)
    case $font in
        *FiraCode*Nerd*)
            if [ -z "$(find "$HOME/Library/Fonts" /Library/Fonts -iname '*firacode*nerd*' 2>/dev/null | head -n 1)" ]; then
                todo "install the font the Ghostty configuration names ($font): brew install --cask font-fira-code-nerd-font"
            fi ;;
    esac
}

# ---------------------------------------------------------------------------
# bb plugins and doctor

install_plugins() {
    if [ "$no_plugins" = 1 ]; then
        skipped "bb plugins: --no-plugins"
        return 0
    fi
    if ! command -v bb >/dev/null 2>&1; then
        skipped "bb plugins: bb is not on PATH"
        for id in $PLUGINS; do
            todo "once bb is installed: bb plugin install $PLUGINS_SOURCE --subdirectory plugins/$id"
        done
        return 0
    fi
    if ! installed=$(bb plugin list --json </dev/null 2>/dev/null); then
        if [ "$dry_run" = 1 ]; then
            say "    bb did not answer bb plugin list; would install both plugins:"
            for id in $PLUGINS; do
                act bb plugin install "$PLUGINS_SOURCE" --subdirectory "plugins/$id" --yes
                did "installed the bb plugin $id"
            done
            return 0
        fi
        skipped "bb plugins: bb is installed but did not answer (is the app running?)"
        for id in $PLUGINS; do
            todo "start bb, then: bb plugin install $PLUGINS_SOURCE --subdirectory plugins/$id"
        done
        return 0
    fi
    for id in $PLUGINS; do
        if printf '%s\n' "$installed" | grep -Eq "\"id\": *\"$id\""; then
            skipped "bb plugin $id: already installed (update it with: bb plugin update $id)"
        elif act bb plugin install "$PLUGINS_SOURCE" --subdirectory "plugins/$id" --yes </dev/null; then
            did "installed the bb plugin $id"
        else
            todo "bb plugin $id did not install (output above); retry: bb plugin install $PLUGINS_SOURCE --subdirectory plugins/$id"
        fi
    done
}

run_doctor() {
    [ "$no_plugins" = 1 ] && return 0
    command -v bb >/dev/null 2>&1 || return 0
    step "bb hx doctor"
    if [ "$dry_run" = 1 ]; then
        say "    would run: bb hx doctor"
        return 0
    fi
    if report=$(bb hx doctor </dev/null 2>&1); then ok=1; else ok=0; fi
    printf '%s\n' "$report" | sed 's/^/    /'
    case $report in
        *"is not on PATH"*)
            todo "bb does not see hxb on its PATH: bb plugin config helix-bridge set hxbPath $prefix/bin/hxb (or start bb from a shell that has $prefix/bin on PATH)" ;;
    esac
    [ "$ok" = 1 ] || todo "bb hx doctor reported problems (above); each one names its fix"
}

# ---------------------------------------------------------------------------

main() {
    prefix=$HOME/.local
    from_source=0
    archive=""
    no_config=0
    no_plugins=0
    dry_run=0
    while [ $# -gt 0 ]; do
        case $1 in
            --prefix) [ $# -ge 2 ] || die "--prefix needs a directory"; prefix=$2; shift ;;
            --prefix=*) prefix=${1#--prefix=} ;;
            --from-source) from_source=1 ;;
            --archive) [ $# -ge 2 ] || die "--archive needs a file"; archive=$2; shift ;;
            --archive=*) archive=${1#--archive=} ;;
            --no-config) no_config=1 ;;
            --no-plugins) no_plugins=1 ;;
            --dry-run) dry_run=1 ;;
            -h|--help) usage "$0"; exit 0 ;;
            *) die "unknown option $1 (see --help)" ;;
        esac
        shift
    done
    case $prefix in /*) ;; *) die "--prefix must be an absolute path" ;; esac
    prefix=${prefix%/}
    if [ -n "$archive" ]; then
        [ -f "$archive" ] || die "no such archive: $archive"
        archive=$(cd "$(dirname "$archive")" && pwd -P)/$(basename "$archive")
    fi

    # Run from a checkout (bb-bridge/install.sh), or piped from curl.
    checkout=""
    if [ -f "$0" ]; then
        d=$(cd "$(dirname "$0")" && pwd -P)
        if [ -f "$d/hxb" ] && [ -f "$d/release.sh" ] && [ -f "$d/../Cargo.toml" ]; then
            checkout=$(dirname "$d")
        fi
    fi

    work=$(mktemp -d "${TMPDIR:-/tmp}/hxb-install.XXXXXX")
    trap 'rm -rf "$work" ${incoming:+"$incoming"}' EXIT
    incoming=""

    [ "$dry_run" = 1 ] && say "Dry run: nothing will be changed."
    say "Installing hxb under $prefix"

    step "hxb"
    if [ -n "$archive" ]; then
        say "    from $archive"
    elif [ "$from_source" = 1 ]; then
        say "    from source (--from-source)"
        build_from_source
    elif ! apple_silicon; then
        say "    from source: prebuilt archives are for Apple silicon only"
        build_from_source
    else
        find_release
        if [ -n "$release_url" ]; then
            say "    latest release: $release_url"
            if [ "$dry_run" = 1 ]; then
                say "    would download it and its .sha256, check it, and unpack it"
            else
                download_release
            fi
        else
            say "    no prebuilt archive ($release_why): building from source"
            build_from_source
        fi
    fi

    dest=""
    config_src=${checkout:+$checkout/bb-bridge/config}
    if [ -n "$archive" ]; then
        install_archive
        link_hxb
    elif [ "$dry_run" = 1 ]; then
        say "    would unpack to $prefix/lib/hxb/<version>-<commit> and link $prefix/bin/hxb to it"
    fi

    step "configuration"
    install_config

    step "bb plugins"
    install_plugins

    check_path
    run_doctor

    gotools=""
    command -v gopls >/dev/null 2>&1 || gotools="go install golang.org/x/tools/gopls@latest"
    command -v goimports >/dev/null 2>&1 || gotools="${gotools:+$gotools && }go install golang.org/x/tools/cmd/goimports@latest"
    if [ -n "$gotools" ]; then
        command -v go >/dev/null 2>&1 || gotools="brew install go && $gotools"
        todo "Go language server and import fixing for the configuration's Go setup: $gotools (and have ~/go/bin on PATH)"
    fi
    keys=${config_src:-$prefix/lib/hxb/<version>/config}/keys-bb.example.toml
    todo "key bindings for the :bb-* commands are not set for you: copy the ones you want from $keys into ~/.config/helix/config.toml"

    step "Summary"
    [ "$dry_run" = 1 ] && say "Dry run: nothing was changed. \"Done\" is what a real run would do."
    say "Done:${done_list:-
  - nothing}"
    say "Skipped:${skip_list:-
  - nothing}"
    say "Still to do by hand:${todo_list}"
}

main "$@"
