#!/bin/sh
# Fetch the pinned Ghostty source that libghostty-vt is built from.
#
# The pin must match the commit the `libghostty-vt` crate's bindings were
# generated for (GHOSTTY_COMMIT in crates/libghostty-vt-sys/build.rs at the
# crate version in Cargo.toml). Change both together.
set -eu

GHOSTTY_REPO="https://github.com/ghostty-org/ghostty.git"
# ghostty-org/ghostty a887df42 "terminal: bound page map probe lengths
# without tombstones (#13294)", pinned by libghostty-vt 0.2.2.
GHOSTTY_COMMIT="a887df42c56f6de86c0fe6da9c4eeca37931e083"

root=$(cd "$(dirname "$0")/.." && pwd)
dest="$root/.local/ghostty"

if [ ! -d "$dest/.git" ]; then
    mkdir -p "$dest"
    git -C "$dest" init -q
    git -C "$dest" remote add origin "$GHOSTTY_REPO"
fi

if [ "$(git -C "$dest" rev-parse -q --verify HEAD 2>/dev/null || true)" != "$GHOSTTY_COMMIT" ]; then
    git -C "$dest" fetch -q --depth 1 origin "$GHOSTTY_COMMIT"
    git -C "$dest" checkout -q --detach FETCH_HEAD
fi

echo "ghostty at $(git -C "$dest" rev-parse --short HEAD) in $dest"
