# libghostty-vt pitfalls

Hazards in how tai builds and uses libghostty-vt. Each one holds on every
OS and can be checked with grep against the named file.

## A debug build of tai stalls on heavy output

`libghostty-vt-sys`'s build script (`build.rs`, `zig_optimize_mode`)
compiles Ghostty in Zig `Debug` mode whenever cargo sets `DEBUG=true`, as
it does for the `dev` profile. `[profile.dev.package."*"] opt-level` does
not change that. Ghostty's `Debug` mode turns on `slow_runtime_safety`
(`src/build/Config.zig`), and with it `Page.assertIntegrity`
(`src/terminal/page.zig`) re-verifies a whole page, cell by cell, each time
the scrollback grows. Each cell's check includes a hyperlink-map lookup
that can probe the whole map.

On 2026-10-24 a debug tai on Windows froze ("Not Responding", one core at
100%, memory flat) during a recursive `ls` in WSL after font-size steps.
Every stack sample was `linefeed` → `PageList.grow` →
`Page.assertIntegrity` → `verifyIntegrity` → `lookupHyperlink`.

`scripts/cargo.sh` exports `LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseSafe` unless
the build is `-r`, `--release` or `--profile release*`. The build script honours
that variable over `DEBUG`, and applies it to every profile, which is why
it isn't set in `.cargo/config.toml`: there it would also turn release
builds from ReleaseFast into ReleaseSafe. ReleaseSafe keeps Zig's safety
checks (bounds, overflow) but not `slow_runtime_safety`. Plain `cargo`
skips the wrapper and gets the Debug mode back.

tai logs the mode at startup: `tai | simd: enabled, optimize: ReleaseSafe`.
To debug libghostty-vt itself, override it for one build:
`LIBGHOSTTY_VT_SYS_OPTIMIZE=Debug scripts/cargo.sh build`, and expect
heavy output to be slow.

## Output with many hyperlinks crashes the pinned Ghostty

At the pin (a887df42), `Terminal.print` and `printCell`
(`src/terminal/Terminal.zig`) keep cell pointers across calls that can grow,
and so replace, the cursor's page. When a page runs out of hyperlink space,
they read or write freed memory. On 2026-10-25 a ReleaseSafe tai on macOS
crashed in `Terminal.print` within seconds of
`ls -R --hyperlink=always /usr/share`, right after the log line "error
reallocating for more hyperlink space". Upstream fixed it in 33cda4dc5
"terminal: reload cell pointers when print grows a page", which needs Zig
0.16. `patches/ghostty/0001-reload-cell-pointers-when-print-grows-a-page.patch`
backports its code hunks, and `scripts/fetch-ghostty.sh` applies it.

`scripts/repro-hyperlink-ls.sh target/debug/tai` reproduces it. It runs
the listing inside tai and prints whether tai finished, froze or crashed,
the Ghostty optimize mode, and how many "OutOfSpace" warnings were logged.
Thousands of warnings with no crash mean the patch works. Run it on dev and
release builds after moving the pin or dropping the patch. On 2026-10-25,
on macOS with 16,950 lines: the unpatched ReleaseSafe build crashed in
about 4 s, Zig Debug froze for 300 s, and the patched builds finished in
about 15 s (ReleaseSafe) and 9 s (ReleaseFast).

## Changing the Ghostty source doesn't rebuild libghostty-vt

`libghostty-vt-sys`'s build script reruns only when its environment
variables change (`rerun-if-env-changed`), not when files under
`GHOSTTY_SOURCE_DIR` change. A new pin or patch would leave the old library
in `target/`. `scripts/cargo.sh` therefore keeps the Ghostty checkout's
HEAD in `<target dir>/.tai-ghostty-head` and runs
`cargo clean -p libghostty-vt-sys` when it differs. Plain `cargo` skips
that check.
