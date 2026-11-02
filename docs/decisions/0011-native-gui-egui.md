# ADR 0011: A Rust-native GUI on egui, with terminals as panes

- **Status:** Accepted 2026-11-01 (owner), after the prototype (branch
  `gui-proto`; see "Prototype results"). On Windows, wgpu uses its GL
  backend by default until the Vulkan memory cost or DX12 is resolved.
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

## Prototype results (2026-11-01)

`tai-gui` on branch `gui-proto` (behind the `gui` feature, so the
macroquad build is untouched): eframe 0.36 on wgpu 30, egui_dock 0.21,
egui_commonmark 0.25 with syntect.

**What it does.** Two tab stacks, chat beside a terminal and preview
stack. The terminal pane runs the shell on tai's own pty modules and
draws it with `cells.rs`'s glyph atlas through a WGSL port of the text
pass in an egui paint callback. The chat pane starts `ppxai-server
--owned` per ADR 0010, sends a prompt to `/v1/oneshot` and renders the
answer as Markdown; the owner got an answer from Gemini through it on
Windows. Not in it: images, selection, mouse reporting, streaming chat,
the preview, and ADR 0010's plain-mode fallback.

**Measured** against today's macroquad `tai`, release builds. Windows:
AMD integrated GPU, Vulkan. Linux: GTX 980 under Xvfb (no vsync), with
`tai` on llvmpipe GL since Xvfb has no hardware GL. macOS: Intel Iris
Pro, Metal, `WGPU_VALIDATION_INDIRECT_CALL=0`.

| | Windows | Linux | macOS |
|---|---|---|---|
| Idle CPU, shell idle, `tai` → `tai-gui` | 4.4-5.3% → 0% | 479% → 0% | 4.7-4.9% → 0% |
| Memory, `tai` → `tai-gui` | 227 → 531 MB (224 MB with `WGPU_BACKEND=gl`) | 207 → 291 MB | 160 → 162 MB |
| Keypress to pixels, median | felt the same (owner) | 3.5 → 4.4 ms | not measured |
| Output flood, `tai` → `tai-gui` | 7.42 → 7.50 s (100,000 lines, shell-bound) | 2.46 → 3.0 s (`seq 1 3000000`) | 12.6 → 11.4 s (`seq 1 200000`) |

The engine is a separate process: about 90-100 MB.

**Re-measured on 2026-11-02** at main `d9f0cf1`, after the Unix pty
began gathering reads into one chunk (`3c0a85a`) and `tai-gui` began
rebuilding only changed rows (`9d1c285`). Same hosts and methods as
above; memory wasn't re-taken.

| | Windows | Linux | macOS |
|---|---|---|---|
| Idle CPU, shell idle, `tai` → `tai-gui` | not re-taken | 480% → 0.1% | 2.7-3.9% → 0% |
| Keypress to pixels, median | not re-taken | 3.5 → 4.2 ms | not measured |
| Output flood, `tai` → `tai-gui` | 6.63-6.69 → 6.88 s (100,000 lines, shell-bound) | 1.26 → 1.11 s (`seq 1 3000000`) | 7.8-8.3 → 6.5-7.0 s (`seq 1 200000`) |
| CPU during the flood, `tai` → `tai-gui` | not measured | not measured | 9.5-10.8% → 17.6-23.0% |

- The flood gap is gone: `tai-gui` is now faster than `tai` on Linux
  and macOS, and both halved on Linux. Windows is bound by PowerShell
  generating the lines, and the gather change is Unix-only.
- macOS flood time is mostly frame-bound: a first set of runs with the
  display off gave 1.2 s (`tai`) and 0.36 s (`tai-gui`), so it isn't in
  the table.
- Linux typing is unchanged. A first re-take gave 4.7 → 4.85 ms because
  it watched a larger screen region (640×200 for `tai`) than the original
  probe (400×40); each grab then took about 0.9 ms instead of 0.25 ms,
  which the probe counts as latency. With the original regions, `tai`
  gives 3.4-3.5 ms and `tai-gui` 4.2-4.3 ms.
- Linux idle CPU is `tai`'s own, but it comes from the test host: under
  Xvfb `tai` draws with llvmpipe and nothing caps its frame rate.

**Findings** of the first measurement.
- egui's reactive repaint gives an idle `tai-gui` no frames at all,
  once the pty's reader wakes egui only when output arrives, at most
  once per frame. A blinking text cursor alone kept egui at about 30
  frames a second, so text cursors don't blink.
- Typing took two frames until the pane waited up to 4 ms for the echo
  after writing input, as `tai`'s `ECHO_WAIT` does; that brought the
  Linux median from 7.5 to 4.4 ms.
- The flood gap on Linux (+22%) is per-frame work: the pane rebuilds
  and uploads every visible row on any change, where `tai` rebuilds
  only dirty rows (ADR 0008). That's the first thing to tune. (Done in
  `9d1c285`; see the re-measure above.) On macOS
  `tai-gui` was faster but used about twice the CPU (20-26% against
  9-10%), and its ui time grew from about 0.7 to 1.1 ms over a flood.
- Windows memory: wgpu's Vulkan backend on the AMD driver costs about
  300 MB; the GL backend matches `tai`. DX12 isn't available on that
  host (`DxcCreateInstance` fails), so the backend choice there needs a
  decision.
- The process tree ends with tai on all three: a kill-on-close job
  object on Windows (the server starts suspended and is resumed inside
  the job), the process group and stdin EOF on Unix, checked with
  `kill -9` of `tai-gui` and with a one-file `--owned` build on macOS.
- Hazards found: egui-winit turns Ctrl+C/X/V into Copy/Cut/Paste events
  with no key event, so the terminal pane sends Ctrl+C and Ctrl+X on
  itself; libghostty-vt 0.2.2's `encode_to_vec` can drop a key when its
  retry reserves too little (now worked around in both binaries,
  `9e81ca4`; the unfiled upstream issue is
  `docs/upstream/libghostty-rs-encode-to-vec.md`); clicked links in
  model output open only for http and https.

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

## Reference: runs spawned from a chat session

Not a decision yet. It records how ppxai's web app handles oneshot and
task runs started from a chat session, as tai's starting point. The
details are to be settled with the ppxai session in an RFC, because the
routes involved are ppxai's unsealed `/v1/agent/*` and
`/sessions/merge-run-result`.

Read in ppxai `feature/v1.19.5` at `b3433545`
(`ppxai/web/shared/agent-run-controller.js`,
`components/views/agent-run-view.js`, `task-run-view.js`,
`ppxai/server/routes/sessions.py`):

- **Where progress shows:** a run started from chat (`/agentrun` for
  the tool-free `POST /v1/agent/run`, `/task` for `POST /v1/agent/task`)
  renders in its own right-panel view, one per run id, so the chat prompt
  stays usable while it runs.
- **What the views show:**
  - **Oneshot run:** a status chip and the result.
  - **Task run:** a meta bar (provider and model, tool and egress
    grants, budget), a live event log (`tool_call`, `tool_denied`,
    `network_*`, `spawn_*`), Cancel while it runs, and a consent card
    while it's parked in `waiting`, answered through
    `POST .../respond`.
- **How a result reaches the session:** it follows the server's
  `execution.collect` setting.
  - **`yes` (the default):** a Collect button.
  - **`auto`:** it merges on completion.
  - **`no`:** collecting is disabled.

  Merging posts `POST /sessions/merge-run-result`, and the result text
  enters the conversation as-is, for the model's next turn, at most
  once per run.
- **Sub-agents:** a task can start child runs (`spawn_subagent`,
  `ppxai/engine/tools/agent_spawn.py`).
- **Records:** run data stays in `~/.ppxai/runs/<id>/`, and a resume
  route exists.

tai's intended shape, the same flow natively:
- **In the chat:** a compact card per run, with status, consent buttons
  and "Open".
- **In the session's tab stack (decision 6):** a tab per run, with
  child runs nested under their parent.
- **Collect:** follows `execution.collect`.
- **Attention:** a run waiting for consent marks its card, its tab and
  the workspace tab.

What ppxai has today, from the ppxai session (`b3433545`, 2026-11-01),
and what the RFC has to settle:

- **Sealing `/v1/agent/*`:** no date and no planned changes
  (`docs/api-gateway.md`, "NOT yet sealed"). Additive drift already
  exists: `RunMetaResponse` gained `kind`, `waiting`, `acked_at` and
  `workdir`, plus `GET /runs?kind=` and `POST /runs/{id}/ack`
  (`ppxai/server/routes/agent_v1.py`).
- **A session's runs:** not supported. Run records carry no session id,
  and `GET /v1/agent/runs` filters only by owner and kind. A restarted
  client can only list all runs and match them itself. **RFC item.**
- **Runs started mid-chat:** chat doesn't start runs. `spawn_subagent`
  is a task-tier tool only (`engine/task_runner.py`), and its
  `subagent_spawned` and `subagent_finished` events go on the parent
  run's events stream, not on `/chat`. Runs start explicitly: by
  `POST /v1/agent/run` or `/task`, or by the slash commands. The chat
  event `agent_run_start` (`engine/chat.py`) is the chat tool loop and
  has no run id.
- **A run waiting for consent with nobody watching:**
  - **Timeout:** after `execution.task.consent.consent_ttl_s` (default
    300 s) it's denied, fail-closed (`{"approved": false, "via":
    "timeout"}`), with no notification.
  - **A later client** finds it through `GET /runs` or `/runs/{id}`
    (status `waiting`, with the token) and answers it with
    `POST /runs/{id}/respond`. `agent_waiting` is replayed on the events
    stream.
  - **After a server restart** the timer is gone; `POST /runs/{id}/resume`
    restarts the run.
  - **How the TUIs do it:** they poll every 2 s (`tui/run_consent.py`).
  - **Docs:** `docs/agent-task-lifecycle.html`, ppxai ADR 0003 §8,
    `docs/consent-contract.md`.
  - **For tai:** a session's waiting run must be visible even when its
    tab isn't open, so tai polls or listens for every run it started.
- **Collect:** documented and implemented (`docs/task-agent-guide.md`,
  ppxai ADR 0011; `ppxai/server/routes/sessions.py`: 403 under `no` or
  for a non-owner, 404 for an unknown run, 409 with no result; results
  kept `result_retention_s`, default 3600 s).
  - **What a merge does:** it appends a user (task) plus assistant
    (result) pair to the caller's **active** session.
  - **What it doesn't check:** the run id is its only input. It isn't
    tied to the session that started the run, and nothing on the server
    stops a second merge.
  - **Under `auto`:** the client merges, not the server. So tai must
    target the right session and remember merged runs itself, or the
    RFC moves both to the server. **RFC item.**
