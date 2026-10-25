# ADR 0007: Bundle Microsoft's ConPTY with tai on Windows

- **Status:** Accepted 2026-10-25 (owner). Implemented in
  `scripts/fetch-conpty.sh`, `scripts/cargo.sh` and `src/pty_windows.rs`;
  the owner saw a Kitty image drawn in tai on Windows through the bundled
  ConPTY.
- **Deciders:** owner. Written by the Claude session on the Windows dev host,
  with review from the Linux session.
- **Supersedes:** [ADR 0004](0004-windows-system-conpty.md) (use Windows'
  own ConPTY). Its reasons still hold; only the ConPTY it chose changes.
- **Related:** [ADR 0002](0002-windows-11-support.md) (ConPTY through
  `portable-pty`). ADRs 0005 (Kitty images) and 0006 (Sixel translated to
  Kitty), on their own branches, need image sequences to reach tai, which
  this record makes possible on Windows.

## Context

ADR 0004 made tai use Windows' own ConPTY, to stop another app's older
`conpty.dll` (WezTerm's) from garbling the screen after a resize, and to
close a DLL-planting hole. A probe on 2026-10-25 (Windows 11 26300) then
showed what that ConPTY does to image traffic. The child was Python that
enables VT output and raw VT input as Textual does, and writes a Sixel
image, a Kitty query (`a=q`) and DA1 (`ESC[c`):

| | Windows' own ConPTY (conhost) | Microsoft's ConPTY package 1.25 |
|---|---|---|
| Sixel DCS | swallowed | passed through byte for byte |
| Kitty APC | swallowed | passed through |
| DA1 query | answered by conhost: `ESC[?61;6;7;21;22;23;24;28;32;42c` (no Sixel) | passed to the terminal; the terminal's reply reached the child |

So with Windows' own ConPTY, no image protocol reaches tai, and tai's DA1
reply is never seen. Windows Terminal shows images because it ships its own
ConPTY. WezTerm and VS Code (`node-pty`) ship one too.

**The package:** `Microsoft.Windows.Console.ConPTY` 1.25.260930003 on
nuget.org, built from microsoft/terminal. It holds `conpty.dll` and
`OpenConsole.exe` (file version 1.25.2609.30003). Both are Authenticode-signed
by Microsoft Corporation. The nuspec declares the MIT licence, requires no
licence acceptance, and says the package works on Windows 10 17763 and later.
The package doesn't name the exact release tag it was built from; the nearest
1.25 release is v1.25.2733.0.

**Resize:** `portable-pty` always passes `PSEUDOCONSOLE_RESIZE_QUIRK`,
which garbled the screen with WezTerm's 2024 ConPTY (ADR 0004). In
microsoft/terminal v1.25.2733.0, `src/winconpty/winconpty.cpp` turns only
`PSEUDOCONSOLE_INHERIT_CURSOR`, `PSEUDOCONSOLE_AMBIGUOUS_IS_WIDE` and the
glyph-width bits into host arguments; the resize-quirk bit is ignored.

## Decision

1. **Bundle the pinned package next to `tai.exe`.**
   - `scripts/fetch-conpty.sh` downloads it from nuget.org into
     `.local/conpty/`, checking the sha256 of the package and of both
     binaries against pins.
   - `scripts/cargo.sh` copies `conpty.dll` and `OpenConsole.exe` into
     `target/debug` and `target/release`, so dev runs use it. Release
     packaging, when tai has it, ships the same two files and the licence.
   - `docs/third-party/microsoft-terminal-LICENSE` holds the MIT notice.
2. **Load only from tai's folder or System32.** Before creating the pseudo
   console, tai calls
   `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32)`
   (`restrict_dll_search` in `src/pty_windows.rs`). `portable-pty`'s bare
   `LoadLibraryW("conpty.dll")` then finds the bundled copy. The current
   directory and `PATH` stay out of the search, as in ADR 0004.
3. **Fall back to Windows' own ConPTY, and say so.** If `conpty.dll` isn't
   next to `tai.exe`, `portable-pty` uses the one in `kernel32.dll`: tai
   works, but without images. tai logs which ConPTY it loaded
   (`log_conpty`): `ConPTY: bundled <path>` or a warning that images won't
   reach tai.

Checked on the dev host: with the files in place, the shell runs under
`target\debug\OpenConsole.exe` and the log names the bundled `conpty.dll`.
With `conpty.dll` renamed away, the shell runs under the system conhost and
the warning is logged. fmt, clippy and the 29 tests pass.

## Why this and not the alternatives

- **Keep Windows' own ConPTY (ADR 0004):** no images on Windows, ever, for
  Sixel or Kitty.
- **Optional download** (tai, or a script, fetches the package only when a
  user asks for images): nothing extra to ship, but two ConPTY paths to test
  and support, network and verification code on users' machines, and most
  users would meet the no-images path first. The fallback in decision 3
  already covers a tai without the files.
- **Build OpenConsole from microsoft/terminal source:** full control, but a
  large C++ build to own. Microsoft's signed package is what other
  terminals use.
- **Use Windows Terminal's installed `OpenConsole.exe`:** it ships without a
  `conpty.dll`, and pairing it with WezTerm's 2024 `conpty.dll` didn't start
  (no output, no host process). It also ties tai to whether and which
  Windows Terminal is installed.

## Accepted risks

- **Trust in tai's folder:** any DLL in it can now load into tai. That's
  normal for an install directory, but tai's folder must not be writable by
  less-trusted users, and a dev `target\` folder is only as trusted as the
  checkout.
- **Updates are tai's job:** a ConPTY bug or security fix reaches users only
  when tai bumps the pin. Changing it means updating the version and all
  three hashes in `fetch-conpty.sh`, then re-running the passthrough and
  resize checks.
- **Version drift:** Windows Terminal, VS Code and tai may each run a
  different ConPTY on the same machine, so behaviour can differ between
  them.
- **Plain `cargo` skips the copy step,** like the other `cargo.sh` steps:
  such a build falls back to Windows' own ConPTY, and the log says so.

## Still to check

- Done: the owner saw a Kitty image (a direct placement) drawn in tai on
  2026-10-25, which only the bundled ConPTY lets through.
- Done: resize and large font-size steps with the bundled ConPTY, the
  ADR 0004 garbling case, showed no garbling (owner, 2026-10-25; note
  added 2026-10-25 after acceptance).
- Older Windows 10 and 11 builds: only Windows 11 26300 has been tried.

## Triggers to revisit

- A new ConPTY package fixes a bug tai hits, or a security advisory names
  ConPTY: bump the pin.
- Windows' own ConPTY starts passing image sequences and DA1 through: the
  bundle may become unnecessary.
- `portable-pty` changes how it loads ConPTY or which flags it passes.
