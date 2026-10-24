# ADR 0004: Use Windows' own ConPTY

- **Status:** Proposed 2026-10-25.
- **Deciders:** owner. Written by the Claude session on the Windows dev host.
- **Related:** [ADR 0002](0002-windows-11-support.md), decision 2 (ConPTY
  through `portable-pty`), which this amends without replacing.

## Context

On Windows, a large resize or font-size change (a big step up, then
Ctrl+0) left lines out of order until the shell repainted. Investigated on
2026-10-25 (`docs/lessons/windows-pitfalls.md`, "tai may run another app's
ConPTY"):

- `portable-pty` 0.9 (`src/win/psuedocon.rs`, `load_conpty`) first calls
  `LoadLibraryW("conpty.dll")` by bare name, and uses the ConPTY in
  `kernel32.dll` only if that fails. The DLL search covers tai's folder,
  the current directory and `PATH`.
- With WezTerm 20240203 on `PATH`, tai ran WezTerm's `conpty.dll` and
  `OpenConsole.exe`. That OpenConsole predates the 2024 ConPTY rewrite
  (microsoft/terminal 450eec48d, "A minor ConPTY refactoring: Goodbye
  VtEngine Edition"). `portable-pty` always passes
  `PSEUDOCONSOLE_RESIZE_QUIRK`, which tells the old ConPTY not to repaint
  after a resize, because Windows Terminal reflowed its buffer the way
  ConPTY expected. libghostty-vt doesn't, so the two screens disagreed.
- Started with WezTerm removed from `PATH`, tai ran under
  `C:\WINDOWS\system32\conhost.exe --headless`, and the owner saw no
  garbling after the same steps.
- Because the search includes the current directory, a `conpty.dll` in the
  folder tai starts from would be loaded into tai.
- No `conpty.dll` exists in System32 or SysWOW64 on the dev host
  (Windows 11 26300).

## Decision

On Windows, tai calls `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32)`
before creating the pseudo console (`use_system_conpty` in
`src/pty_windows.rs`). `portable-pty`'s bare `conpty.dll` lookup then
finds nothing, and it falls back to the system ConPTY in `kernel32.dll`.
tai keeps `portable-pty` for everything else.

Checked on the dev host with WezTerm on `PATH`: the shell runs under the
system `conhost.exe --headless`, no `conpty.dll` is loaded, and the
window renders. fmt, clippy and the 29 tests pass.

## Why this and not the alternatives

- **Patch or vendor `portable-pty`** to load `kernel32.dll` only, or to
  drop `PSEUDOCONSOLE_RESIZE_QUIRK`: a second vendored crate to keep in
  step, for the same effect.
- **Ship a known OpenConsole and `conpty.dll`** next to `tai.exe`, as
  Windows Terminal and WezTerm do: tai would choose its ConPTY version,
  but it would have to build, sign and update Microsoft's binaries. The
  system ConPTY on current Windows 11 already has the rewrite.
- **`windows-sys` directly instead of `portable-pty`** (ADR 0002's
  rejected alternative): full control over the flags, but tai would own
  the pipe and process plumbing. Not needed for this problem.
- **Document a `PATH` workaround only:** leaves the garbling and the
  DLL-planting hazard in place for anyone with WezTerm or a stray
  `conpty.dll`.

## Accepted risks

- `SetDefaultDllDirectories` applies to the whole tai process: any later
  `LoadLibrary` by bare name searches only System32. The window and OpenGL
  are set up before the PTY (`Pty::new` runs inside `macroquad::main`,
  after miniquad created the window and GL context), so their DLLs are
  already loaded. A future feature that loads a DLL from tai's own folder
  would have to pass a full path or add that folder with
  `AddDllDirectory`.
- Some DLLs load lazily later, such as text-service (IME) modules on first
  focus and GL driver helpers. These normally load by full path from the
  registry, so the System32-only default shouldn't affect them. Untested:
  no IME was used on the dev host.
- It doesn't reach the shell: Microsoft documents that the process DLL
  search path "applies only to the calling process" (`SetDefaultDllDirectories`,
  Remarks). Programs run inside tai keep the normal DLL search.
  (`SetDllDirectory`, by contrast, is passed on to child processes.)
- tai gets whatever ConPTY the Windows version ships. Older Windows 11
  builds may have a pre-rewrite system ConPTY, with which
  `PSEUDOCONSOLE_RESIZE_QUIRK` could cause the same garbling. Only the dev
  host's build has been tried.

## Triggers to revisit

- Garbling after a resize with the system ConPTY, on any Windows build.
  Then drop `PSEUDOCONSOLE_RESIZE_QUIRK` (patch `portable-pty`) or ship a
  known OpenConsole.
- `portable-pty` changes how it loads ConPTY or which flags it passes.
- tai needs to load a DLL from its own folder.
