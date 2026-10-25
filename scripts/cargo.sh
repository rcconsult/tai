#!/bin/sh
# Run cargo with the pinned Zig first on PATH, fetching it and the pinned
# Ghostty source on first use. Use this instead of plain `cargo` for builds:
# libghostty-vt's build script runs whichever `zig` is on PATH.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
"$root/scripts/fetch-zig.sh" >/dev/null
"$root/scripts/fetch-ghostty.sh" >/dev/null
"$root/scripts/prefetch-zig-deps.sh" >/dev/null

# libghostty-vt-sys compiles Ghostty in Zig Debug mode for a dev build,
# which turns on Ghostty's slow_runtime_safety: every new scrollback page
# is re-verified cell by cell, and heavy output can stall tai for minutes.
# Use ReleaseSafe for dev builds instead; it keeps Zig's safety checks.
# Release builds keep the build script's default (ReleaseFast), and an
# exported LIBGHOSTTY_VT_SYS_OPTIMIZE wins. See
# docs/lessons/libghostty-vt-pitfalls.md.
release=false
prev=
for arg in "$@"; do
    # Arguments after `--` belong to the program, not to cargo.
    [ "$arg" = -- ] && break
    case $arg in
    -r | --release | --profile=release*) release=true ;;
    release*) [ "$prev" = --profile ] && release=true ;;
    esac
    prev=$arg
done
if [ "$release" = false ] && [ -z "${LIBGHOSTTY_VT_SYS_OPTIMIZE:-}" ]; then
    LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseSafe
    export LIBGHOSTTY_VT_SYS_OPTIMIZE
fi

# libghostty-vt-sys's build script doesn't watch the Ghostty source, so a
# new pin or patch would leave the old library in the target directory.
# Rebuild it once whenever the checkout's HEAD changes.
# The stamp lives in the target directory, since each one holds its own build.
ghostty_head=$(git -C "${GHOSTTY_SOURCE_DIR:-$root/.local/ghostty}" rev-parse HEAD)
target_dir=${CARGO_TARGET_DIR:-$root/target}
built="$target_dir/.tai-ghostty-head"
if [ "$(cat "$built" 2>/dev/null || true)" != "$ghostty_head" ]; then
    # `cargo clean -p` cleans only the dev profile unless told otherwise.
    PATH="$root/.local/zig:$PATH" cargo clean -q -p libghostty-vt-sys
    PATH="$root/.local/zig:$PATH" cargo clean -q -p libghostty-vt-sys --release
    mkdir -p "$target_dir"
    echo "$ghostty_head" >"$built"
fi

# On Windows, put the bundled ConPTY (ADR 0007) next to tai.exe for both
# profiles, so `cargo.sh run` and the built binaries use it. A running tai
# keeps OpenConsole.exe open, so an identical file is left alone.
"$root/scripts/fetch-conpty.sh" >/dev/null
if [ -d "$root/.local/conpty" ]; then
    for profile in debug release; do
        mkdir -p "$target_dir/$profile"
        for file in conpty.dll OpenConsole.exe; do
            cmp -s "$root/.local/conpty/$file" "$target_dir/$profile/$file" ||
                cp "$root/.local/conpty/$file" "$target_dir/$profile/$file"
        done
    done
fi

PATH="$root/.local/zig:$PATH" exec cargo "$@"
