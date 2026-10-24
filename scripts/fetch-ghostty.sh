#!/bin/sh
# Fetch the pinned Ghostty source that libghostty-vt is built from.
#
# The pin must match the commit the `libghostty-vt` crate's bindings were
# generated for (GHOSTTY_COMMIT in crates/libghostty-vt-sys/build.rs at the
# crate version in Cargo.toml). Change both together.
#
# patches/ghostty/*.patch are applied on top of the pin, in name order, as
# local commits, so the checkout stays clean. Each patch file says where it
# comes from and when to drop it.
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
    # With autocrlf=true (common on Windows) every file would look modified
    # and trip the local-changes check below.
    git -C "$dest" config core.autocrlf false
    git -C "$dest" remote add origin "$GHOSTTY_REPO"
fi

# This checkout is ours to move to the pin. Refuse to touch a different
# clone someone placed here, or one with edits to tracked files (the
# build leaves untracked output such as zig-pkg/, which is fine).
origin=$(git -C "$dest" remote get-url origin 2>/dev/null || true)
if [ "$origin" != "$GHOSTTY_REPO" ]; then
    echo "$dest: origin is '$origin', not $GHOSTTY_REPO; not touching it" >&2
    exit 1
fi
if [ -n "$(git -C "$dest" status --porcelain --untracked-files=no)" ]; then
    echo "$dest has local changes; not moving it to the pin" >&2
    exit 1
fi

set -- "$root"/patches/ghostty/*.patch
[ -e "$1" ] || set --
# What the checkout should be: the pin plus these patches, byte for byte.
want=$({ echo "$GHOSTTY_COMMIT"; cat "$@"; } | git hash-object --stdin)
stamp="$dest/.git/tai-patches"
base=$(git -C "$dest" rev-parse -q --verify "HEAD~$#" 2>/dev/null || true)

if [ "$base" != "$GHOSTTY_COMMIT" ] || [ "$(cat "$stamp" 2>/dev/null || true)" != "$want" ]; then
    if ! git -C "$dest" cat-file -e "$GHOSTTY_COMMIT^{commit}" 2>/dev/null; then
        git -C "$dest" fetch -q --depth 1 origin "$GHOSTTY_COMMIT"
    fi
    git -C "$dest" checkout -q --detach "$GHOSTTY_COMMIT"
    for patch in "$@"; do
        git -C "$dest" apply "$patch"
        git -C "$dest" -c user.name=tai -c user.email=tai@localhost             commit -q -a -m "tai patch: $(basename "$patch")"
    done
    echo "$want" >"$stamp"
fi

echo "ghostty at $(git -C "$dest" rev-parse --short "HEAD~$#") + $# patch(es) in $dest"
