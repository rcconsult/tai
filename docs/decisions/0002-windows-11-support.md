# ADR 0002: Windows 11 support

- **Status:** Proposed 2026-10-24. Not accepted, not implemented.
- **Deciders:** owner. Written by the Claude session on the Windows dev host
  (Windows 11 Pro x86_64), with input from the Linux and macOS sessions.
- **Related:** [ADR 0001](0001-stack-rust-libghostty-vt-ppxai-server.md),
  which this amends for Windows (decision 4 and the build), without changing
  it for Linux or macOS.

## Context

ADR 0001 covers Linux and macOS only. At `6931ebe` ("docs/lessons: link the
Metal leak report (miniquad#697)") tai does not compile on Windows. Every
Windows question below was settled by a probe on the Windows dev host on
2026-10-24, not by reading docs. The probes were throwaway crates outside
the repo.

**Toolchain:** rustc 1.99.1, `stable-x86_64-pc-windows-msvc`, VS 2022 Build
Tools 17.14 (MSVC x64, Windows SDK 10.0.26100), Zig 0.15.2
(`zig-x86_64-windows-0.15.2.zip`, sha256 checked against ziglang.org's
`index.json`), and Ghostty `a887df42` ("terminal: bound page map probe
lengths without tombstones (#13294)"), cloned with `core.autocrlf=false`.

**What the probes found:**

1. **libghostty-vt builds and runs on MSVC.** A crate depending on
   `libghostty-vt =0.2.2` built and ran `Terminal::new` and `vt_write`.
   The build script links `ghostty-vt-static.lib` statically
   (`rustc-link-lib=static:+verbatim=ghostty-vt-static.lib`), so the `.exe`
   needs no DLL. The GNU target was not tried, because MSVC worked.
2. **Zig's fetch is unreliable on Windows.** Zig 0.15.2's HTTP client
   intermittently fails on github.com tarballs with `invalid HTTP response:
   HttpHeadersOversize`, and retrying only sometimes helps. It hit
   `ocornut/imgui` (from Ghostty's `pkg/dcimgui`), `zigimg` and `uucode`
   (both from `vaxis`). Downloading each with curl or git and running
   `zig fetch <local file or dir>` put it in Zig's package cache with the
   hash its `build.zig.zon` expects, and the build then went through.
   `deps.files.ghostty.org` downloads did not fail.
3. **miniquad's Windows clipboard is real**, unlike its macOS stub.
   Through the vendored macroquad 0.4.16, `clipboard_get` read text set by
   the OS, and `clipboard_set` of non-ASCII text (`ü ï ☃`) round-tripped
   and was visible to the OS afterwards. The window opened under WGL.
   Display scaling on the host is 100% (96 DPI) and `screen_dpi_scale()` was
   1, so scaling above 100% is **untested**.
4. **ConPTY works from Rust** through `portable-pty` 0.9, with `cmd.exe`
   and `pwsh.exe`. Exit codes came through (3 and 7), and a resize to 120x40
   reached the shell (`mode con` and `$Host.UI.RawUI.WindowSize` both
   reported 120 columns).
5. **ConPTY waits for an answer at startup.** It first emits
   `ESC[?9001h ESC[?1004h ESC[6n` and processes no input until the cursor
   position query (`ESC[6n`) is answered. A probe that didn't answer hung
   indefinitely. libghostty-vt answers it itself through `on_pty_write`
   (`ESC[1;1R`, and `ESC[3;5R` after moving the cursor), and tai already
   wires `on_pty_write` to the PTY (`src/main.rs`, "write_pty effect"). So
   tai will get past the wait, but any test harness that drives ConPTY
   without a terminal must answer it too. libghostty-vt ignores `?9001h`
   (win32-input-mode), so ConPTY keeps taking plain VT input.
6. **ConPTY is not a transparent pipe.** It re-renders the screen and emits
   its own sequences (window titles, cursor hide and show, `CSI 8 t`
   resize reports), so the byte stream differs from a Unix PTY's for the
   same program.

**What blocks compiling on Windows (from reading the code):**

- `mod pty` in `src/main.rs` is POSIX through `nix`: `forkpty`,
  `std::os::unix::process::CommandExt`, signal reset in the child,
  a non-blocking master fd read every frame, `TIOCSWINSZ`, `SIGHUP` on
  close, and `nix::sys::wait`. `nix` is an unconditional dependency.
- The shell comes from `$SHELL`, then the passwd entry, then `/bin/sh`.
- `scripts/fetch-zig.sh` maps only `Linux` and `Darwin`, expects
  `.tar.xz`, and runs `zig`, not `zig.exe`. `scripts/fetch-ghostty.sh`
  refuses a checkout with modified tracked files, which a clone with
  `core.autocrlf=true` would trip.

**The engine link (ppxai `feature/v1.19.5` at `81dab79b`, "docs(adr-0016):
owner accepts ~1.8 s server probes for now"):**

- `ppxai-server` refuses `--uds`, `--announce` and `--detach` on win32,
  saying "use --host/--port on Windows" (`ppxai/server/http.py`, the
  `sys.platform == "win32"` check after the `--reload` check;
  `ppxai/server/registry.py` raises for `--uds` on win32 too). `--announce`
  is socket-only by design ("never TCP"). So ADR 0001 decision 4 can't work
  on Windows as written.
- A bearer-token check (`auth_middleware`, v1.18.3) applies in every mode.
  But which token it enforces depends on the configured secrets-provider
  chain (`_provider_enforces_auth` in `ppxai/server/auth.py`). Only the
  `--announce` path forces an `EnvSecretProvider` for `PPXAI_API_TOKEN` to
  the front of that chain. In plain `--host/--port` mode, a user config with
  a `file` provider means a token tai puts in the child's environment can be
  ignored.
- `_is_bootstrap_mint` in `auth.py` lets a loopback caller mint a token
  without one when a mutable token store is configured.
- The `--announce` path binds port 0 on a socket it creates itself. No TCP
  mode was found that reports a bound port back to the parent.

## Decision (proposed)

1. **Target `x86_64-pc-windows-msvc`**, linking libghostty-vt statically as
   on the other platforms. Windows 11 is the supported Windows.
2. **Split `mod pty` by platform** behind one interface for what `main.rs`
   uses (spawn, read, write, resize, exit status, close). Unix keeps the
   current `nix` code unchanged under `cfg(unix)`, and `nix` moves to
   `[target.'cfg(unix)'.dependencies]`. Windows uses ConPTY through
   `portable-pty` under `cfg(windows)`. Its reader blocks, so a reader thread
   feeds a channel that the frame loop drains, in place of the per-frame
   non-blocking read.
3. **Default shell on Windows:** `pwsh.exe` if it is on `PATH`, otherwise
   `%COMSPEC%`, otherwise `cmd.exe`. The Unix signal reset has no Windows
   equivalent and isn't needed.
4. **Build scripts stay POSIX sh and run under Git Bash** (Git for Windows
   is already needed for the Ghostty clone). `fetch-zig.sh` gains a Windows
   case (`.zip`, `unzip`) and `fetch-ghostty.sh` clones with
   `core.autocrlf=false`. One script prefetches the github.com packages that
   Zig's own fetch fails on, with `curl`/`git` and `zig fetch`, before
   `cargo` runs. `cargo.sh` needs no change: Git Bash converts its
   `:`-separated `PATH` to Windows form for native programs (checked with
   `where zig`).
5. **Keep the Linux shortcut mapping on Windows** (Ctrl+Shift+C/V, Ctrl+= /
   - / 0, plain Ctrl+C/V to the shell). It matches Windows Terminal, and
   `src/shortcuts.rs` already gives it to every non-macOS host.
6. **Keep miniquad's clipboard on Windows**: `src/clipboard.rs` already
   routes non-macOS hosts to it, and probe 3 showed it works. The vendored
   macroquad clipboard fix applies on Windows too.
7. **Engine link on Windows: loopback TCP**, with tai as the parent picking
   the port and generating the token. The two gaps below must be closed
   before milestone (b) implements it on Windows. Until then, Windows gets
   milestone (a) only, which needs no engine.

## Open questions

- **Auth over TCP (for the owner, and for ppxai's owner if it needs a ppxai
  change):** either
  (a) ppxai gets a flag or override that puts the env provider first in
  `--host/--port` mode, as `--announce` does, or
  (b) tai sends an unauthenticated request at startup and refuses any server
  that doesn't answer 401.
  (a) is the stronger guarantee. (b) needs no ppxai change but only detects
  the problem.
- **Port race:** if tai picks a free port and passes it down, another
  process can take it first. Avoiding that needs ppxai to bind port 0 and
  report the port it got. That too would be a ppxai change.
- **TCP everywhere?** Using loopback TCP on every platform would give one
  code path, but loses the 0600 socket's file-permission protection on
  Linux and macOS. The proposal keeps the socket there.
- **Is `_is_bootstrap_mint` acceptable** for a tai-owned server on a
  multi-user Windows machine?
- **Display scaling** above 100% and per-monitor DPI on Windows are
  untested. The macOS sessions hit double scaling once (see
  `docs/lessons/macroquad-miniquad-pitfalls.md`).

## Why this and not the alternatives

- **`windows-sys` directly instead of `portable-pty`:** fewer dependencies
  and full control over the ConPTY flags, but tai would own the overlapped
  pipe plumbing and the process and handle lifetimes. `portable-pty` is
  WezTerm's and widely used, and it worked on the first probe once the
  startup query was answered. Revisit if its flags (it sets
  `PSEUDOCONSOLE_INHERIT_CURSOR`) or its threads get in the way.
- **`portable-pty` on Unix too:** one code path, but it rewrites working,
  tested Unix code for no Unix gain, and the Linux and macOS sessions would
  have to re-test it.
- **PowerShell twins of the build scripts:** native on Windows, but two
  copies of each script to keep in step with the three pins.
- **`x86_64-pc-windows-gnu`:** not needed, because MSVC works. It would
  bring a MinGW toolchain.
- **Named pipes for the engine:** closest to a 0600 socket on Windows, but
  ppxai has no named-pipe mode, so it would be a larger ppxai change than
  either TCP gap above.

## Accepted risks

- ConPTY output differs from a Unix PTY's (probe 6), so rendering bugs may
  appear only on Windows.
- Zig's flaky fetch (probe 2) may also hit packages not seen here. The
  prefetch list has to grow when it does.
- Windows sees only what this one host has: x86_64, a single display at
  100%, Windows 11 26300.

## Triggers to revisit

- ppxai allows `--uds`/`--announce` on Windows. Windows 10 1803+ has
  AF_UNIX, but whether ppxai's server stack can serve on it there is
  unverified. Then the Windows engine link can follow ADR 0001.
- ppxai adds a TCP mode that forces the env token and reports its port.
  Then both open questions close.
- A Zig release fixes `HttpHeadersOversize`, and the libghostty-vt pin moves
  to a Ghostty that accepts it. Then drop the prefetch step.
- `portable-pty` blocks something tai needs from ConPTY.
