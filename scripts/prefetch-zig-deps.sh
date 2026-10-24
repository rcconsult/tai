#!/bin/sh
# Put the Ghostty build's github.com packages into Zig's package cache
# ourselves. On Windows, Zig 0.15.2's own fetch of them fails intermittently
# with HttpHeadersOversize (docs/lessons/windows-pitfalls.md). Elsewhere it
# works, so this only does something on Windows.
#
# The list is what the Ghostty commit pinned in fetch-ghostty.sh needs,
# taken from its build.zig.zon files and those of its packages. Change it
# together with that pin.
set -eu

case "$(uname -s)" in
    MINGW* | MSYS*) ;;
    *) exit 0 ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
zig="$root/.local/zig/zig"
cache="${ZIG_GLOBAL_CACHE_DIR:-$LOCALAPPDATA/zig}"

# hash url (a git+ url ends in #commit)
deps="
N-V-__8AAEbOfQBnvcFcCX2W5z7tDaN8vaNZGamEQtNOe0UI https://github.com/ocornut/imgui/archive/refs/tags/v1.92.5-docking.tar.gz
zigimg-0.1.0-8_eo2vHnEwCIVW34Q14Ec-xUlzIoVg86-7FU2ypPtxms https://github.com/ivanstepanovftw/zigimg/archive/d7b7ab0ba0899643831ef042bd73289510b39906.tar.gz
uucode-0.1.0-ZZjBPj96QADXyt5sqwBJUnhaDYs_qBeeKijZvlRa0eqM git+https://github.com/jacobsandlund/uucode#5f05f8f83a75caea201f12cc8ea32a2d82ea9732
"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "$deps" | while read -r hash url; do
    [ -n "$hash" ] || continue
    if [ -d "$cache/p/$hash" ]; then
        continue
    fi
    case "$url" in
        git+*)
            repo=${url#git+}
            repo=${repo%%#*}
            commit=${url##*#}
            src="$tmp/$hash"
            git -c core.autocrlf=false clone -q "$repo" "$src"
            git -C "$src" checkout -q "$commit"
            rm -rf "$src/.git"
            ;;
        *)
            src="$tmp/$hash.tar.gz"
            curl -sSfL "$url" -o "$src"
            ;;
    esac
    got=$("$zig" fetch "$src")
    if [ "$got" != "$hash" ]; then
        echo "hash mismatch for $url: got $got, want $hash" >&2
        exit 1
    fi
    echo "prefetched $hash"
done
