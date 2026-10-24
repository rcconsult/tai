#!/bin/sh
# Reproduces the hyperlink crash and freeze recorded in
# docs/lessons/libghostty-vt-pitfalls.md: runs a recursive `ls` with OSC 8
# hyperlinks inside tai and reports whether tai finished, froze or crashed.
#
# Usage: scripts/repro-hyperlink-ls.sh path/to/tai [dir]
#
# tai execs $SHELL with no arguments, so this script launches tai with
# itself as $SHELL; TAI_REPRO_OUT tells the two roles apart. Needs GNU ls
# (`gls` on macOS, from coreutils) for --hyperlink.
#
# Unix only: Windows tai ignores $SHELL (src/pty_windows.rs, default_shell).
set -eu

if [ -n "${TAI_REPRO_OUT:-}" ]; then
    # Inside tai: draw the listing, record how long it took, then idle.
    ls="ls"
    command -v gls >/dev/null && ls=gls
    start=$(date +%s)
    "$ls" -R --hyperlink=always "$TAI_REPRO_DIR"
    echo "$(($(date +%s) - start)) s" >"$TAI_REPRO_OUT"
    exec sleep 600
fi

tai=${1:?usage: $0 path/to/tai [dir]}
dir=${2:-/usr/share}
self=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
tmp=$(mktemp -d)
out=$tmp/result
log=$tmp/tai.log

TAI_REPRO_OUT=$out TAI_REPRO_DIR=$dir SHELL=$self "$tai" >"$log" 2>&1 &
pid=$!
t=0
while [ ! -s "$out" ] && [ $t -lt 300 ] && kill -0 $pid 2>/dev/null; do
    sleep 2
    t=$((t + 2))
done

if [ -s "$out" ]; then
    result="finished in $(cat "$out")"
elif kill -0 $pid 2>/dev/null; then
    result="FROZE: still drawing after $t s"
else
    result="CRASHED: tai exited before the listing finished"
fi
kill $pid 2>/dev/null || true
wait $pid 2>/dev/null || true

echo "$result"
grep -o 'optimize: [A-Za-z]*' "$log" || echo 'optimize: not logged'
# Thousands here mean the failed hyperlink-grow path ran.
echo "OutOfSpace warnings: $(grep -c OutOfSpace "$log" || true)"
echo "log: $log"
