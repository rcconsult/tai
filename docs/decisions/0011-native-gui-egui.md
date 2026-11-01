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
| Chat stream with Markdown and code highlighting (`marked`, `highlight.js`, `DOMPurify`) | `egui_commonmark`, `syntect`. Raw HTML in Markdown isn't rendered, which replaces the sanitiser. | yes |
| Consent dialogs, tooltips | egui windows | yes |
| Slash-command autocomplete, command roster | text box and popup | yes |
| Attachments: drag and drop, upload, inline images | dropped files from the windowing layer, the `image` crate | yes |
| Sessions, model and provider pickers, usage, checkpoints, themes | standard widgets | yes |
| File tree | a tree widget | yes |
| Split panes, tabs, the right-hand panel | `egui_dock` or `egui_tiles` | yes |
| CSV and XLSX tables; JSON, YAML, TOML and HCL trees (SheetJS, js-yaml) | `calamine`, `serde_json`, `serde_yaml`, `toml`, `hcl-rs`, table widgets | yes |
| Markdown and image file views | as for chat | yes |
| Agent and task run views (event streams; SSE read from `fetch()` bodies of POSTs, not `EventSource`) | lists and logs; the client POSTs and parses SSE itself | yes |
| Terminal view (xterm.js on a server-side PTY over a WebSocket) | tai's own libghostty-vt panes, local, and SSH in a pane | yes, natively |
| Remote ppxai servers (`ssh-launcher-view.js`, ppxai ADR 0013's hub: launch and attach to `ppxai-server` on remote hosts over SSH) | an `ssh` child process, and the ADR 0010 client on the forwarded endpoint | yes, a feature of its own |
| PowerPoint (the server renders each slide to PNG with LibreOffice; without it, per-slide text) | the PNGs, or the text | yes |
| Code editor (CodeMirror 6, 30 languages, edit and save) | weak in Rust GUI crates beyond viewing and light edits | see decision 4 |
| PDF (the browser's `<embed>`); Word (the server converts it to PDF) | needs a PDF library: `pdfium-render` bundles Google's C++ library, and MuPDF is AGPL | see decision 5 |

Neither Mermaid nor KaTeX is used, so there's nothing more to port for
diagrams or maths. The Linux session checked this table against the files
at `b3433545`.

The server already serves `GET /files/preview?path=…&slide=N` (PNG per
PowerPoint slide) and converts Word to PDF (`ppxai/server/routes/files.py`,
`file_serve.py`). It doesn't render PDF pages to images.

## Decision

1. **The UI layer is egui on wgpu**, through eframe or egui-winit with
   egui-wgpu. It replaces macroquad, and with it the vendored clipboard
   patch and the Metal leak workaround.
2. **Terminal panes stay libghostty-vt.** The ADR 0008 cell renderer moves
   into an egui paint callback on the same wgpu device, with its shaders
   ported to WGSL. The macOS session's estimate for `src/cells.rs` (455
   lines):
   - **Ports as is:** the glyph atlas (shelf packing, fontdue
     rasterising, dirty rows, the 36-byte `Instance` and its size caps),
     except a ~25-line upload that becomes `queue.write_texture`.
   - **Rewritten for wgpu:** `TextPass` (shader-version choice, pipeline,
     texture-size query and draw). The two ~20-line shaders translate
     directly to WGSL, and the texture-size cap comes from the adapter's
     limits.
   - **Unchanged:** the cell walk in `main.rs`.
   - **Removed:** the immediate renderer and macroquad's side of
     `fonts.rs`.
3. **wgpu's indirect-call validation stays on by default, and an
   environment variable turns it off.**
   - **The problem:** on macOS 12.7 with Intel Iris Pro (eframe 0.37,
     wgpu 30.0.1), the validation builds a compute pipeline at device
     creation that the Metal driver rejects ("Compiler encountered an
     internal error"). Device creation then fails, after about 30 s per
     attempt. wgpu has no GL backend on macOS, so Metal is the only path
     there.
   - **With it off,** a window repainting 40 changing text lines every
     frame ran 105 s at a steady 60 fps, with memory flat at 56–63 MB:
     none of miniquad's Metal leak.
   - **Why not off for everyone (the owner's call):** this is likely one
     driver's bug, and later macOS versions and other GPUs probably don't
     have it. So tai builds its instance flags from the environment
     (`InstanceFlags::with_env`, as eframe does; a bare
     `wgpu::Instance::default()` ignores it). Then
     `WGPU_VALIDATION_INDIRECT_CALL=0` turns the validation off where
     it's needed.
   - **When device creation fails,** tai's error names that variable,
     so an affected user isn't left with only the 30 s hang.
4. **Code editing goes to a real editor in a terminal pane.** The editor
   comes from a config setting or an environment variable (for example
   `$VISUAL`/`$EDITOR`, or Helix or Neovim). tai's own view is read-only
   with syntax highlighting.
5. **Documents preview as scrollable PNG pages, rendered by the server.**
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
6. **Layout: panes are tab stacks, and the window has workspace tabs.**
   - **Pane tabs** (`egui_dock`): every pane holds a stack of views,
     rotated, dragged between panes, split off or maximised. The views
     are terminals (shells, SSH), document previews (PNG pages,
     Markdown, images, tables, trees), editors (decision 4) and
     sub-agent runs (task and oneshot).
   - **Following the web app's right panel**
     (`components/right-panel-frame.js`): one view per file, each view
     keeps its scroll, page and cursor, back and forward history, and an
     LRU limit (default 10 per stack). Running views and views with
     unsaved changes are never dropped. A hidden view isn't drawn, but a
     hidden terminal keeps reading its output.
   - **Workspace tabs:** each holds a whole layout with one ppxai
     session, its working folder and file tree. They show unread output
     and running tasks. One `ppxai-server` serves all of them.
7. **Keys follow the mainstream AI harnesses** (Claude Code, Codex CLI,
   pi agent), not ppxaide.
   - **In chat:** Enter sends; Shift+Enter or Ctrl+J adds a new line; Esc
     interrupts; Esc Esc rewinds; Up and Down recall history. `/`
     commands, `@` files and `!` shell lines; Shift+Tab cycles modes;
     Ctrl+G edits in `$EDITOR`.
   - **Window chords** use Cmd on macOS and Ctrl+Shift elsewhere, as
     `src/shortcuts.rs` does. Ctrl+Tab rotates a stack, and Ctrl+\`
     moves between chat and the terminal.
   - **Inside a terminal pane,** every other key goes to the shell.
   - Each binding is checked against those harnesses' own docs before it
     ships.
8. **Lifecycle: everything ends with tai's process, as in the web app.**
   - **What stops:** the PTYs, `ppxai-server` and its running tasks
     (ADR 0010, decision 3).
   - **What survives:** chat sessions, which ppxai keeps on disk
     (`~/.ppxai`) and restores at the next start.
   - **The seam:** everything that would outlive the window sits behind
     one internal interface: the PTYs, the server process and the
     running tasks. That keeps a detachable mode possible.
   - **herdr is the reference for that mode, not a dependency.** herdr
     0.9.5 ("terminal workspace manager for AI coding agents") runs a
     headless server that keeps named sessions alive for clients that
     attach and detach, locally or over SSH, and it bundles ConPTY on
     Windows. Building on it would add a 26 MB third-party daemon with
     its own UI, keys and update channel. Its licence, and whether its
     API streams pane output, are unchecked.
9. **Prototype first.**
   - **Scope:** one window with two tab stacks, on Windows, Linux and
     macOS. One holds a chat pane: a `/v1/oneshot` answer through the
     ADR 0010 client, rendered as Markdown. The other holds a
     libghostty-vt terminal plus one preview view, to prove rotation.
   - **Measured against today's macroquad build:** frame time, input
     latency, memory, and idle CPU with egui's reactive repaint (tai
     idles at about 4% CPU today).
   - If it holds up, this record becomes Accepted.

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
- Linux is new ground in two ways: Wayland-native windows (miniquad runs
  X11 or XWayland only), and IME through winit (XIM on X11, text-input-v3
  on Wayland), which egui must have wired. The prototype tests dead keys
  and an IBus CJK engine, and clipboard copy-then-exit, on X11 and
  Wayland. On Wayland, `arboard` needs the data-control protocol, which
  GNOME's Mutter lacks, so it falls back through XWayland.
- wgpu picks Vulkan where it can. Older GPUs may stop getting new
  drivers (the Linux host's Maxwell card is on NVIDIA's 580 branch), so
  wgpu's GL backend has to keep working as a fallback. Headless runs
  under Xvfb get only CPU Vulkan (lavapipe) or llvmpipe GL, enough for
  pixel-diff tests at CPU speed.

## Triggers to revisit

- The prototype misses macroquad's frame time or input latency, or egui's
  text rendering isn't good enough for long Markdown answers.
- ppxai can't render PDF pages to PNG.
- Users need shells or agent work to outlive tai's window: build the
  detachable `tai --server` behind decision 8's seam, studying herdr.
- A wgpu upgrade or a newer macOS: recheck whether decision 3's variable is still
  needed on the macOS host.
- A Rust code-editor widget matures enough to make an in-window editor
  worthwhile.
