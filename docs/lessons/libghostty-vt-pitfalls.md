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

`.cargo/config.toml` sets `LIBGHOSTTY_VT_SYS_OPTIMIZE = "ReleaseSafe"`,
which the build script honours over `DEBUG`. ReleaseSafe keeps Zig's
safety checks (bounds, overflow) but not `slow_runtime_safety`. To debug
libghostty-vt itself, override it for one build:
`LIBGHOSTTY_VT_SYS_OPTIMIZE=Debug scripts/cargo.sh build`, and expect
heavy output to be slow.
