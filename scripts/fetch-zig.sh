#!/bin/sh
# Fetch the Zig that the pinned Ghostty needs into .local/zig/.
#
# Ghostty requires an exact Zig major.minor (src/build/zig.zig requireZig),
# and the commit pinned in fetch-ghostty.sh declares minimum_zig_version
# 0.15.2 in build.zig.zon. Change this together with that pin.
set -eu

ZIG_VERSION="0.15.2"

root=$(cd "$(dirname "$0")/.." && pwd)
dest="$root/.local/zig"

if [ -x "$dest/zig" ] && [ "$("$dest/zig" version)" = "$ZIG_VERSION" ]; then
    echo "zig $ZIG_VERSION in $dest"
    exit 0
fi

case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
esac
case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

name="zig-$arch-$os-$ZIG_VERSION"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -sSfL "https://ziglang.org/download/index.json" -o "$tmp/index.json"
want=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]][sys.argv[3]]["shasum"])' \
    "$tmp/index.json" "$ZIG_VERSION" "$arch-$os")

curl -sSfL "https://ziglang.org/download/$ZIG_VERSION/$name.tar.xz" -o "$tmp/$name.tar.xz"
if command -v sha256sum >/dev/null; then
    got=$(sha256sum "$tmp/$name.tar.xz" | cut -d' ' -f1)
else
    got=$(shasum -a 256 "$tmp/$name.tar.xz" | cut -d' ' -f1)
fi
if [ "$got" != "$want" ]; then
    echo "sha256 mismatch for $name.tar.xz: got $got, want $want" >&2
    exit 1
fi

mkdir -p "$tmp/x"
tar -xJf "$tmp/$name.tar.xz" -C "$tmp/x"
rm -rf "$dest"
mkdir -p "$root/.local"
mv "$tmp/x/$name" "$dest"
echo "zig $("$dest/zig" version) in $dest"
