# Windows pitfalls

Hazards found while probing Windows 11 support (ADR 0002). Each one holds
on any Windows machine and can be checked with grep against the named file.

## Zig's fetch fails on github.com tarballs

Zig 0.15.2's HTTP client on Windows fails intermittently on github.com
downloads with `invalid HTTP response: HttpHeadersOversize` (or `unable to
discover remote git server capabilities: HttpHeadersOversize` for `git+`
URLs). Retrying only sometimes helps, so a first build of libghostty-vt can
fail in libghostty-vt-sys's build script with `zig build failed`. On
2026-10-24 it hit `ocornut/imgui` (Ghostty's `pkg/dcimgui/build.zig.zon`),
and `zigimg` and `uucode` (the `vaxis` package's `build.zig.zon`, in Zig's
package cache). Downloads from `deps.files.ghostty.org` did not fail.

To find the candidates: `grep -rn 'github.com' --include=build.zig.zon
.local/ghostty`, plus the same over Zig's package cache
(`%LOCALAPPDATA%\zig\p`) for dependencies of dependencies.

**What to do:** download the URL with `curl` (or `git clone` and check out
the commit, then delete `.git`, for a `git+` URL) and run `zig fetch <file
or dir>`. It prints the package hash. If that equals the `.hash` in the
`build.zig.zon`, the package is now cached and the build won't fetch it.

## ConPTY waits for a cursor-position answer before taking input

At startup ConPTY emits `ESC[?9001h ESC[?1004h ESC[6n` and processes no
input until something answers the `ESC[6n` query (with `ESC[<row>;<col>R`).
A client that doesn't answer hangs forever, with the shell running but
idle. `portable-pty` opens the console with `PSEUDOCONSOLE_INHERIT_CURSOR`,
which is what makes ConPTY ask.

**What tai does:** libghostty-vt answers `ESC[6n` itself through the
`on_pty_write` effect, and tai writes that answer to the PTY
(`grep -n on_pty_write src/main.rs`). Keep that wiring on Windows. Any
test or tool that drives ConPTY without a libghostty-vt terminal must
answer the query itself.

## Clone Ghostty with `core.autocrlf=false`

`scripts/fetch-ghostty.sh` refuses to move a checkout whose tracked files
are modified (`git status --porcelain --untracked-files=no`). With
`core.autocrlf=true`, which is common on Windows, line-ending conversion
makes the whole tree look modified. Set `core.autocrlf=false` in
`.local/ghostty` before the first checkout.
