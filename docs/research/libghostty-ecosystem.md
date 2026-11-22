# The libghostty ecosystem: what tai and ppxai can use

A survey of [awesome-libghostty](https://github.com/Uzaaft/awesome-libghostty),
read 2026-11-22 at commit
[`e37b5fa8`](https://github.com/Uzaaft/awesome-libghostty/tree/e37b5fa85d965dcf9cd20929329eb7d5e8e2967f)
(2026-11-13). The list then held about 147 projects in five sections. This
page sorts the ones that matter for tai and ppxai by what they could give
us. For lessons already taken from earlier reads, see
[`../lessons/from-libghostty-projects.md`](../lessons/from-libghostty-projects.md),
which surveyed the list at `b09654cf` on 2026-11-01.

**How far this was read.** Only the READMEs of ghostty-web and Restty were
read (section 1). Everything else is sorted by its name and the list's
one-line description, so read the project before relying on it. The list
gives no licence per project: check each one before porting code, and
record the repo, path and commit as for Ghostty ports (`CLAUDE.md`).

## 1. A browser terminal on libghostty-vt

ppxai's web app draws its terminal with xterm.js
(`ppxai/web/components/views/terminal-view.js`) over the websocket
`/ws/terminal` (`ppxai/server/routes/terminal.py`). The protocol is JSON:
`{"type":"input"|"resize"}` from the browser, `output`, `exit` and
`error` from the server. It doesn't depend on the emulator, so either
library below could connect to it unchanged. The endpoint is Unix only:
on Windows it answers with an error.

### ghostty-web

- https://github.com/coder/ghostty-web, MIT, read at
  [`835eb274`](https://github.com/coder/ghostty-web/tree/835eb2749f640a139693bb64fd67de3c1b50a598)
  (2026-11-18).
- Ghostty's VT parser compiled to WebAssembly, with xterm.js's API. Coder
  wrote it for their Mux agent app. npm `ghostty-web`, about 400 KB, no
  runtime dependencies, the WebAssembly bundled in the package.
- The README says it "aims to be API-compatible with the xterm.js API";
  migrating is changing the import from `@xterm/xterm` to `ghostty-web`. Its
  example uses `init()`, `new Terminal({...})`, `open`, `onData`, `write`.
  It doesn't mention the fit addon or resize, both of which ppxai's
  terminal view uses: check those first.
- It builds its own Ghostty: a submodule at `5714ed07` plus
  `patches/ghostty-wasm-api.patch`, which exposes more of the API to the
  browser. That isn't tai's Ghostty pin. The README says it will move to a
  native Ghostty WebAssembly distribution once there is one.
- It claims grapheme handling for complex scripts that xterm.js lacks.

### Restty

- https://github.com/wiedymi/restty, MIT, read at
  [`3db45b0e`](https://github.com/wiedymi/restty/tree/3db45b0e10dbe53c52a77131d366f519546e4f04)
  (2026-09-05).
- libghostty-vt in WebAssembly, drawn with WebGPU and a WebGL2 fallback on a
  canvas, with a hidden input for IME. npm `restty`, with `restty/xterm` (an
  xterm.js-style wrapper), `restty/headless` (no DOM) and `restty/esm`.
- The README calls it "early-release software"; APIs may change. Its xterm
  compatibility is documented at https://restty.pages.dev/docs/xterm-compat,
  not read.

**For ppxai:** ghostty-web is the closer drop-in for xterm.js, so it's the
one to try first in the web app.

## 2. Using libghostty-vt from a browser build of tai-gui

tai's Ghostty pin already builds libghostty-vt for the browser:
`src/build/GhosttyLibVt.zig` (`initWasm`) emits `ghostty-vt.wasm` with
`zig build -Demit-lib-vt -Dtarget=wasm32-freestanding`. It's a standalone
module, not a library Rust can link, and libghostty-rs links a native
library. Whether libghostty-rs can use the browser build, or tai would need
a JS bridge between its own WebAssembly and Ghostty's, is unanswered. The
pinned Ghostty's `example/wasm-vt` and `example/wasm-key-encode` show the
browser API.

Other browser embedders to compare, not read:

- ghostty-web's `patches/ghostty-wasm-api.patch` (above): the API one
  embedder had to add.
- browstty, https://github.com/Snoupix/browstty: Zig, WebAssembly.
- vscode-bootty, https://github.com/0xBigBoss/vscode-bootty: VS Code,
  WebAssembly.
- obsidian-ghostty-terminal,
  https://github.com/lavs9/obsidian-ghostty-terminal: Obsidian, WebAssembly.
- hauntty, https://github.com/seruman/hauntty: Go, VT parser in
  WebAssembly.
- jupyterlab-ghostty-terminal,
  https://github.com/mvfti/jupyterlab-ghostty-terminal.
- electron-libghostty,
  https://github.com/philipp-spiess/electron-libghostty.
- libghostty-vt-node, https://github.com/coder/libghostty-vt-node: Node-API
  bindings.

## 3. Rust apps on libghostty

tai's own stack. Worth reading for IME, font fallback, grapheme clusters
and Windows, the problems the lessons page tracks. Not read:

- shellwing, https://github.com/hivepaas/shellwing: GPUI; macOS, Windows,
  Linux.
- SSHL, https://github.com/qfdk/sshl: GPUI, macOS.
- moai-studio, https://github.com/modu-ai/moai-studio: GPUI, agent
  orchestration.
- witty, https://github.com/milesxu/witty: Python, Qt6 and Rust.
- taskers, https://github.com/OneNoted/taskers: a Rust shell.
- evp, https://github.com/HalFrgrd/evp: Rust.
- Godotty, https://github.com/ingur/godotty: Rust, Godot. Already a source
  of the lessons page (font fallback).

## 4. Agent-orchestration terminals

The list's largest growth area, with about 40 projects. These are the
references for showing sub-agents and parallel runs beside terminals,
where tai-gui's `/task` and run pane lead. Not read:

- cmux, https://github.com/manaflow-ai/cmux (macOS)
- Mux, https://github.com/coder/mux (ghostty-web was written for it)
- con-terminal, https://github.com/nowledge-co/con-terminal (already a
  source of the lessons page)
- agterm, https://github.com/umputun/agterm (macOS)
- agtmux-term, https://github.com/g960059/agtmux-term (SwiftUI)
- Factory Floor, https://github.com/alltuner/factoryfloor (macOS)
- tuidoscope, https://github.com/shuv1337/tuidoscope (OpenTUI, SolidJS)

## 5. Headless terminals and testing

For lesson 6 of the lessons page (save cell data next to screenshots) and
for letting an agent read a terminal pane's text. Not read:

- headless-terminal, https://github.com/montanaflynn/headless-terminal: Go
  CLI.
- termscope, https://github.com/mwunsch/termscope: CLI.
- vterm-mcp, https://github.com/mjpowersjr/vterm-mcp: an MCP server around
  a terminal.
- term2html, https://github.com/aarol/term2html.

## 6. Other bindings

Not tai's stack, listed for reference: libghostty-cpp, go-libghostty,
ghosttpy-vt (Python), libghostty-dart, ghostty_ex (Elixir),
libghostty-vt-dotnet, libghostty-mbt (MoonBit), libghostty-odin, ghosdin,
GhosttyKit and libghostty-spm (Swift), ts-libghostty (TypeScript),
flutter_ghostty, expo-libghostty (React Native), libghostty-ohos
(HarmonyOS). Ghostling (C, raylib) is Ghostty's own minimal embedder;
libghostty-rs's `ghostling_rs`, where tai's `src/main.rs` started, is its
Rust counterpart.

## References

- libghostty API reference: https://libghostty.tip.ghostty.org/index.html
- "libghostty is coming": https://mitchellh.com/writing/libghostty-is-coming
