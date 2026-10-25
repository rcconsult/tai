#!/bin/sh
# Fetch Microsoft's ConPTY package into .local/conpty/ (Windows only).
#
# tai bundles this ConPTY next to tai.exe (ADR 0007): the one built into
# Windows swallows Sixel and Kitty image sequences and answers DA1 itself.
# The package is Microsoft.Windows.Console.ConPTY from nuget.org, built from
# microsoft/terminal and MIT-licensed (docs/third-party/microsoft-terminal-LICENSE).
# Both binaries are Authenticode-signed by Microsoft Corporation. Change the
# version and all three hashes together.
set -eu

# Windows means Git Bash or MSYS2, as for the other scripts (ADR 0002).
case "$(uname -s)" in
    MINGW* | MSYS*) ;;
    *) exit 0 ;;
esac

CONPTY_VERSION="1.25.260930003"
NUPKG_SHA256="02b07b349af66d801159bdf9e440d4a1ce78bb951f37fc8609731665afdae7ee"
CONPTY_DLL_SHA256="feeef341d891643c62d30b6b07800bc70f0bc148f44cb8c3bee84aa557ae805a"
OPENCONSOLE_SHA256="3d66b23d0a71bb8eed2b77edc8b9df9bf54ce6c8fb74c863a30e760997f80586"

root=$(cd "$(dirname "$0")/.." && pwd)
dest="$root/.local/conpty"

sha() { sha256sum "$1" | cut -d' ' -f1; }

if [ -f "$dest/conpty.dll" ] && [ -f "$dest/OpenConsole.exe" ] &&
    [ "$(sha "$dest/conpty.dll")" = "$CONPTY_DLL_SHA256" ] &&
    [ "$(sha "$dest/OpenConsole.exe")" = "$OPENCONSOLE_SHA256" ]; then
    echo "conpty $CONPTY_VERSION in $dest"
    exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

id=microsoft.windows.console.conpty
echo "fetching ConPTY $CONPTY_VERSION from nuget.org" >&2
curl -sSfL "https://api.nuget.org/v3-flatcontainer/$id/$CONPTY_VERSION/$id.$CONPTY_VERSION.nupkg" \
    -o "$tmp/conpty.nupkg"
check() {
    got=$(sha "$1")
    if [ "$got" != "$2" ]; then
        echo "sha256 mismatch for $1: got $got, want $2" >&2
        exit 1
    fi
}
check "$tmp/conpty.nupkg" "$NUPKG_SHA256"

mkdir -p "$tmp/x"
unzip -q "$tmp/conpty.nupkg" -d "$tmp/x"
check "$tmp/x/runtimes/win-x64/native/conpty.dll" "$CONPTY_DLL_SHA256"
check "$tmp/x/build/native/runtimes/x64/OpenConsole.exe" "$OPENCONSOLE_SHA256"

rm -rf "$dest"
mkdir -p "$dest"
cp "$tmp/x/runtimes/win-x64/native/conpty.dll" "$tmp/x/build/native/runtimes/x64/OpenConsole.exe" "$dest/"
echo "conpty $CONPTY_VERSION in $dest"
