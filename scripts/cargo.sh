#!/bin/sh
# Run cargo with the pinned Zig first on PATH, fetching it and the pinned
# Ghostty source on first use. Use this instead of plain `cargo` for builds:
# libghostty-vt's build script runs whichever `zig` is on PATH.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
"$root/scripts/fetch-zig.sh" >/dev/null
"$root/scripts/fetch-ghostty.sh" >/dev/null
"$root/scripts/prefetch-zig-deps.sh" >/dev/null

PATH="$root/.local/zig:$PATH" exec cargo "$@"
