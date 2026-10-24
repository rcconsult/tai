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

PATH="$root/.local/zig:$PATH" exec cargo "$@"
