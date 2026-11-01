# ADR 0011: A Rust-native GUI on egui, with terminals as panes

- **Status:** Proposed 2026-11-01. The owner agreed to try the direction;
  a prototype decides.
- **Deciders:** owner. Written by the Claude session on the Windows dev
  host.
- **Amends:** decision 3 of [ADR 0001](0001-stack-rust-libghostty-vt-ppxai-server.md)
  (milestone (b): "winit + wgpu"), by naming the UI layer on top of them.
- **Related:** [ADR 0008](0008-gpu-cell-renderer.md) (the GPU cell
  renderer), [ADR 0010](0010-engine-link-loopback-tcp.md) (the engine
  link).

## Context

The owner's aim, stated 2026-11-01: tai is to replace ppxai's own clients,
the Rich TUI and ppxaide, at some point, and catch up with the feature set
of ppxai's desktop web app.
- tai is a small, self-contained, Rust-native desktop terminal platform,
  with the ppxai harness embedded as a bundled `ppxai-server` child.
- No TypeScript, JavaScript or Node UI. A Python harness married to a web
  front end is the many-language system the owner wants to avoid.
- The engine is reached only through the server API, the contract ppxai's
  web app and VS Code extension already use.

The Python TUIs are limited by the terminal cell grid itself, not only by
the terminal's speed: one monospace font size, images through the Kitty
and Sixel protocols (ADRs 0005, 0006, 0009), emoji widths that the app,
the shell and the terminal each count differently (the mode 2027 lesson in
`docs/lessons/libghostty-vt-pitfalls.md`), and mouse and resize
negotiation through ConPTY on Windows. A faster terminal can't lift that
ceiling. So tai's own UI is drawn natively, and terminals are panes in it.

**The web app's feature set** (ppxai `feature/v1.19.5` at `b3433545`,
"docs(changelog): Python 3.15 support", `ppxai/web/`, about 14,000 lines of
application JavaScript), read from each component's header and from
keyword counts in `app.js`, and mapped to Rust:

| Web app feature (what it uses) | In Rust | Verdict |
|---|---|---|
| Chat stream with Markdown and code highlighting (`marked`, `highlight.js`) | `egui_commonmark`, `syntect` | yes |
| Consent dialogs, tooltips | egui windows | yes |
| Slash-command autocomplete, command roster | text box and popup | yes |
| Attachments: drag and drop, upload, inline images | dropped files from the windowing layer, the `image` crate | yes |
| Sessions, model and provider pickers, usage, checkpoints, themes | standard widgets | yes |
| File tree | a tree widget | yes |
| Split panes, tabs, the right-hand panel | `egui_dock` or `egui_tiles` | yes |
| CSV and XLSX tables; JSON, YAML, TOML and HCL trees (SheetJS, js-yaml) | `calamine`, `serde_json`, `serde_yaml`, `toml`, `hcl-rs`, table widgets | yes |
| Markdown and image file views | as for chat | yes |
| Agent and task run views (event streams) | lists and logs over SSE | yes |
| Terminal view (xterm.js on a server-side PTY over a WebSocket) | tai's own libghostty-vt panes, local, and SSH in a pane | yes, natively |
| PowerPoint (the server renders each slide to PNG with LibreOffice) | the PNGs | yes |
| Code editor (CodeMirror 6, 30 languages, edit and save) | weak in Rust GUI crates beyond viewing and light edits | see decision 3 |
| PDF (the browser's `<embed>`); Word (the server converts it to PDF) | needs a PDF library: `pdfium-render` bundles Google's C++ library, and MuPDF is AGPL | see decision 4 |

The server already serves `GET /files/preview?path=…&slide=N` (PNG per
PowerPoint slide) and converts Word to PDF (`ppxai/server/routes/files.py`,
`file_serve.py`). It doesn't render PDF pages to images.

## Decision

1. **The UI layer is egui on wgpu**, through eframe or egui-winit with
   egui-wgpu. It replaces macroquad, and with it the vendored clipboard
   patch and the Metal leak workaround.
2. **Terminal panes stay libghostty-vt.** The ADR 0008 cell renderer moves
   into an egui paint callback on the same wgpu device, with its shaders
   ported to WGSL.
3. **Code editing goes to a real editor in a terminal pane.** The editor
   comes from a config setting or an environment variable (for example
   `$VISUAL`/`$EDITOR`, or Helix or Neovim). tai's own view is read-only
   with syntax highlighting.
4. **Documents preview as scrollable PNG pages, rendered by the server.**
   - **LibreOffice found:** high-quality page images. The detection is
     ppxai's, in the engine, not in the web app's JavaScript
     (`ppxai/common/libreoffice.py`, `find_libreoffice`: the
     `PPXAI_LIBREOFFICE` override, then `libreoffice` or `soffice` on
     `PATH`, then the usual install folders for each OS). The server
     reports `libreoffice_available` in its preview answers, so tai
     needs no detection of its own, and a user who wants high-quality
     output installs LibreOffice or sets `PPXAI_LIBREOFFICE`.
   - **No LibreOffice:** the server's `text_fallback` (extracted text),
     shown with a note on how to get page images.
   - PowerPoint works this way today. PDF and Word need the server to
     render pages to PNG as well, a request to ppxai. Until then they
     open in the OS viewer.
5. **Prototype first.** One window with a chat pane (Markdown, a
   streamed `/v1/oneshot` answer, through the ADR 0010 client) beside a
   libghostty-vt terminal pane, on Windows, Linux and macOS. It is
   measured against today's macroquad build for frame time, input
   latency and memory. If it holds up, this record becomes Accepted.

## Why this and not the alternatives

- **A webview reusing ppxai's web app (Tauri):** the fastest way to
  parity, but it brings back the JavaScript front end the owner ruled out.
- **iced:** retained-mode and Rust-native, with custom shader widgets.
  Fewer ready widgets for this feature set, and its API still changes
  between releases.
- **Plain winit + wgpu with our own widgets:** full control, but every
  widget above would be ours to write.
- **GTK4 (as Ghostty's Linux app):** native on Linux, weak on Windows and
  macOS.
- **Keep macroquad and draw widgets ourselves:** it has no widget toolkit
  for this, and its Metal path leaks on macOS.

## Accepted risks

- The port is large: the web app is about 14,000 lines of JavaScript.
- egui is immediate mode. Very long chat histories need virtualised
  scrolling, and rich text styling takes work.
- Kitty and Sixel images inside terminal panes keep working through the
  cell renderer, but the native UI never uses them.

## Triggers to revisit

- The prototype misses macroquad's frame time or input latency, or egui's
  text rendering isn't good enough for long Markdown answers.
- ppxai can't render PDF pages to PNG.
- A Rust code-editor widget matures enough to make an in-window editor
  worthwhile.
