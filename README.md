# tai

A terminal app for the ppxai engine, built on
[libghostty-vt](https://github.com/ghostty-org/ghostty) (Ghostty's
embeddable terminal library). It runs on Linux and macOS.

**Status:** milestone (a), a terminal running your `$SHELL`. The ppxai chat
UI comes next. [ADR 0001](docs/decisions/0001-stack-rust-libghostty-vt-ppxai-server.md)
explains the stack.

## Build and run

```sh
scripts/cargo.sh run --release
```

Always go through `scripts/cargo.sh`. On first use it fetches the Zig
version that the pinned Ghostty needs, plus the Ghostty source, into
`.local/`. The first build takes a few minutes.

On macOS, `TAI_APPLE_GFX=metal` switches from OpenGL to Metal. It's
experimental: it leaked GPU memory in testing.

## Shortcuts

| | macOS | Linux |
|---|---|---|
| Copy selection | Cmd+C | Ctrl+Shift+C |
| Paste | Cmd+V | Ctrl+Shift+V |
| Bigger / smaller / reset font | Cmd+= / Cmd+- / Cmd+0 | Ctrl+= / Ctrl+- / Ctrl+0 |

Plain Ctrl+C and Ctrl+V go to the shell. Drag to select. Hold Option on
macOS, or Ctrl+Alt on Linux, for a rectangular selection.

## Images

tai supports the **Kitty graphics protocol** (PNG). Use a tool that
speaks it, such as `kitten icat` from kitty, or `chafa` if it detects
the protocol.

iTerm2's `imgcat` doesn't work: it uses iTerm2's own escape sequence
(OSC 1337), which libghostty-vt doesn't implement.

## Known gaps

- Font fallback covers Nerd Font icons only. Emoji and CJK show as boxes.
- macroquad is vendored with a clipboard fix (`vendor/macroquad/TAI-PATCHES.md`).

## Licence notes

`src/main.rs` started from libghostty-rs's `ghostling_rs` example (MIT OR
Apache-2.0). The bundled JetBrains Mono font is under the SIL Open Font
License (`fonts/OFL.txt`). The bundled Symbols Nerd Font Mono, used as the
fallback for icons, is MIT, and its icon sets carry their own licences
(`fonts/README.md`).
