# tai

A terminal app for the ppxai engine, built on
[libghostty-vt](https://github.com/ghostty-org/ghostty) (Ghostty's
embeddable terminal library). It runs on Linux, macOS and Windows 11.

**Status:** two binaries.

- `tai` (milestone (a)): a terminal running your `$SHELL`.
- `tai-gui` (milestone (b), a prototype): tab stacks of terminal panes
  and a chat pane that talks to ppxai's engine
  ([ADR 0011](docs/decisions/0011-native-gui-egui.md)).

[ADR 0001](docs/decisions/0001-stack-rust-libghostty-vt-ppxai-server.md)
explains the stack.

## Build and run

```sh
scripts/cargo.sh run --release
```

Always go through `scripts/cargo.sh`. On first use it fetches the Zig
version that the pinned Ghostty needs, plus the Ghostty source, into
`.local/`. The first build takes a few minutes.

On Linux, the build needs fontconfig's headers (`libfontconfig-dev` on
Debian and Ubuntu, `fontconfig-devel` on Fedora) and `pkg-config`: tai asks
fontconfig for fonts it doesn't bundle, such as CJK.

tai draws with OpenGL 3.2 or newer on every platform. On an older
OpenGL it stops with an error that says what's missing.

To run `tai-gui`:

```sh
scripts/cargo.sh run --release --features gui --bin tai-gui
```

It needs Rust 1.95 or newer and ppxai 1.19.5 or newer, whose
`ppxai-server --owned` it starts itself. It looks for `ppxai-server` on
`PATH`, then in `~/.ppxai/bin`; `TAI_PPXAI_SERVER` sets the path.

## Shortcuts

| | macOS | Linux and Windows |
|---|---|---|
| Copy selection | Cmd+C | Ctrl+Shift+C |
| Paste | Cmd+V | Ctrl+Shift+V |
| Bigger / smaller / reset font | Cmd+= / Cmd+- / Cmd+0 | Ctrl+= / Ctrl+- / Ctrl+0 |

Plain Ctrl+C and Ctrl+V go to the shell. Drag to select. Hold Option on
macOS, or Ctrl+Alt on Linux and Windows, for a rectangular selection.
While text is selected, a badge in the bottom-right corner counts its
lines and characters.

## Fonts

Text is drawn with the bundled JetBrains Mono. Nerd Font icons, general
symbols and emoji fall back to bundled fonts (`fonts/README.md`). Anything
else, such as Chinese, Japanese, Korean or Thai, comes from the font your
OS picks for that character in your locale: DirectWrite on Windows,
CoreText on macOS, fontconfig on Linux
([ADR 0012](docs/decisions/0012-cjk-font-fallback.md)). So Han characters
take the forms your language expects.

## Images

tai supports the **Kitty graphics protocol** (PNG). Use a tool that
speaks it, such as `kitten icat` from kitty, or `chafa` if it detects
the protocol.

iTerm2's `imgcat` doesn't work: it uses iTerm2's own escape sequence
(OSC 1337), which libghostty-vt doesn't implement.

## Known gaps

- CJK text renders and can be pasted, but typing it through an input
  method (IME) doesn't work yet, in either binary. It's under
  investigation.
- Emoji are monochrome; colour emoji aren't drawn yet.
- tai doesn't shape text, so scripts that need shaping, such as Arabic or
  Devanagari, draw poorly. A grapheme cluster, such as an emoji ZWJ
  sequence, takes the font of its first character.
- macroquad is vendored with a clipboard fix (`vendor/macroquad/TAI-PATCHES.md`).
- Windows: tai bundles Microsoft's ConPTY (`conpty.dll` and
  `OpenConsole.exe`, next to `tai.exe`; ADR 0007), because Windows' own
  swallows image sequences. Without those files tai still works, without
  images, and logs a warning. If lines ever show out of order after a
  resize, press Ctrl+L or run `clear` to redraw. Details:
  `docs/lessons/windows-pitfalls.md`.
- Windows: piping `wsl.exe` output (for example `wsl --help | more`)
  shows one character per line, in any terminal, because `wsl.exe`
  writes UTF-16 into pipes. Set `$env:WSL_UTF8 = '1'` first. Details:
  `docs/lessons/windows-pitfalls.md`.
- ppxai's TUI (ppxaide) draws its image panel in tai from ppxai 1.19.5,
  which knows `TERM_PROGRAM=tai` as a Kitty terminal. An older ppxai says
  "Terminal does not support image display"; set
  `PPXAI_IMAGE_PROTOCOL=kitty` before starting it (PowerShell:
  `$env:PPXAI_IMAGE_PROTOCOL = 'kitty'`).

## Licence notes

`src/main.rs` started from libghostty-rs's `ghostling_rs` example (MIT OR
Apache-2.0). The bundled JetBrains Mono font is under the SIL Open Font
License (`fonts/OFL.txt`). The bundled Symbols Nerd Font Mono, used as the
fallback for icons, is MIT, and its icon sets carry their own licences
(`fonts/README.md`). The bundled Iosevka Fixed, the fallback for general
symbols, is under the SIL Open Font License (`fonts/Iosevka-LICENSE.md`).
The bundled Noto Emoji, the fallback for emoji, is under the SIL Open Font
License (`fonts/NotoEmoji-LICENSE`). Fonts found on your system are read
in place and not copied or bundled.
On Windows, tai bundles Microsoft's ConPTY (`conpty.dll`, `OpenConsole.exe`)
from the `Microsoft.Windows.Console.ConPTY` package, under the MIT licence
(`docs/third-party/microsoft-terminal-LICENSE`).
