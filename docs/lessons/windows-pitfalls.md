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

## Ctrl+letter arrives as a control character

On Windows, miniquad's `WM_CHAR` handler passes the character Windows
produced, and for Ctrl+letter that is a control character: Ctrl+C gives
U+0003 (Enter, Tab and Backspace likewise give `\r`, `\t`, `\x08`). If
that reaches libghostty-vt's key encoder as the event's text
(`set_utf8`), the encoder emits CSI u, `ESC[3;5u` for Ctrl+C, instead of
the control byte. ConPTY doesn't treat that as Ctrl+C, so nothing gets
interrupted.

**What tai does:** `handle_keyboard_input` drops control characters
from `get_char_pressed()` before building the text
(`grep -n 'is_control' src/main.rs`). The encoder derives control bytes
from the key and modifiers itself.

## The "ignore Ctrl+C" flag is inherited

A Windows process inherits its parent's "ignore Ctrl+C" flag, and ConPTY's
child inherits it from the process that creates the pseudo console. If
whatever launched tai had set it, ConPTY never turns the 0x03 tai writes
into a `CTRL_C_EVENT`: a probe writing 0x03 into ConPTY left `ping -t`
running, and stopped it once the parent had called
`SetConsoleCtrlHandler(NULL, FALSE)`. This is the Windows counterpart of
an ignored `SIGINT` surviving `exec` on Unix.

**What tai does:** `allow_ctrl_c` in `src/pty_windows.rs` clears the flag
before spawning the shell (`grep -n SetConsoleCtrlHandler
src/pty_windows.rs`). Keep it for any child that should get Ctrl+C.

## Clone Ghostty with `core.autocrlf=false`

`scripts/fetch-ghostty.sh` refuses to move a checkout whose tracked files
are modified (`git status --porcelain --untracked-files=no`). With
`core.autocrlf=true`, which is common on Windows, line-ending conversion
makes the whole tree look modified. Set `core.autocrlf=false` in
`.local/ghostty` before the first checkout.

## tai may run another app's ConPTY, chosen by DLL search

`portable-pty` 0.9 (`src/win/psuedocon.rs`, `load_conpty`) prefers a
sideloaded ConPTY: it calls `LoadLibraryW("conpty.dll")` with a bare name
and falls back to the ConPTY in `kernel32.dll` only if that fails. The
standard DLL search order covers tai's folder, the system folders, the
current directory and every `PATH` entry. On 2026-10-25, with WezTerm
20240203 installed and on `PATH`, tai's shell ran under
`C:\Program Files\WezTerm\OpenConsole.exe` (loaded through WezTerm's
`conpty.dll`); with WezTerm removed from `PATH` it ran under
`C:\WINDOWS\system32\conhost.exe --headless`. Which ConPTY tai gets
therefore depends on what else is installed, and a `conpty.dll` in the
directory tai starts from would be loaded into tai.

That matters for resizing. ConPTY was rewritten in mid-2024
(microsoft/terminal 450eec48d, "A minor ConPTY refactoring: Goodbye
VtEngine Edition"): before it, ConPTY re-rendered the console's screen
itself; since, it passes the program's output through. WezTerm 20240203
bundles the older kind. `portable-pty` always passes
`PSEUDOCONSOLE_RESIZE_QUIRK`, which in the older ConPTY means "don't
repaint the viewport on resize; the terminal handles its own buffer
specifically for ConPTY" (old `src/host/VtIo.cpp`,
`IsResizeQuirkEnabled`, citing GH#3490, #4354, #4741). That was written
for Windows Terminal. libghostty-vt reflows its own way, so after a large
resize (a big font-size step, then Ctrl+0) tai's screen and ConPTY's
disagree: lines appear out of order until the shell repaints (Ctrl+L or
`clear`). The rewritten ConPTY dropped the flag: the system conhost was
started without `--resizeQuirk`, while WezTerm's OpenConsole got it.

To see which ConPTY a running tai uses, list its child processes
(`Get-CimInstance Win32_Process -Filter "ParentProcessId=<tai pid>"`) or
its loaded modules (`conpty.dll`).

**What tai does:** `use_system_conpty` in `src/pty_windows.rs` restricts
DLL search to System32 before the pseudo console is created, so
`portable-pty` falls back to the system ConPTY (ADR 0004,
`grep -n SetDefaultDllDirectories src/pty_windows.rs`). With WezTerm
removed from `PATH`, the owner saw no garbling after the same resize
steps.

Even with the system ConPTY, text printed while the grid was narrow stays
narrow after it widens. On 2026-10-25, `ls` run at the largest font size
left each file name one character per line after Ctrl+0, in order and not
garbled. The rewritten ConPTY turns some line wraps into hard line breaks
(450eec48d lists it as unresolved: "`WriteCharsLegacy` emits a `\r\n` …
This breaks text reflow on window resize"), and hard breaks can't be
rejoined. Output printed after the resize lays out at the new width.
