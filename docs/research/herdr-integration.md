# herdr support in tai and tai-gui

Research, 2026-11-24. Not a decision: it sorts the ways tai and tai-gui could
work with herdr and recommends an order. A decision would be its own ADR.

herdr is "a terminal workspace manager for AI coding agents": a multiplexer
with workspaces, tabs and panes, a persistent headless server, and agent
tracking. ADR 0011 (decision 8) names it the reference for a detachable
mode, and ADR 0013 ("With herdr") records herdr running inside a tai-gui
terminal on Windows.

## Sources and how far they were checked

- herdr **0.9.5**, the installed binary: `herdr --help`, the `--help` of
  `terminal`, `terminal attach`, `terminal session observe|control`,
  `agent attach` and `api`; `herdr --skill`; `herdr --default-config`;
  and `herdr api schema --json` (protocol 22, schema_version 1, about 100
  methods). Nothing was run against a server.
- The repo [herdrdev/herdr](https://github.com/herdrdev/herdr) at tag
  `v0.9.5`, read raw: `Cargo.toml`, `LICENSE`,
  `vendor/libghostty-vt.vendor.json`, and the docs pages
  `persistence-remote.mdx`, `socket-api.mdx`, `add-herdr-support.mdx` and
  `plugins.mdx` under `docs/next/website/src/content/docs/`.
- Not tried live. Every "works" below is herdr's own docs, unless it says
  it was tried. The spike in [step 2](#recommended-order) is where they
  get tried.

## What herdr offers an outside program

### Licence and code

- **Apache-2.0** at v0.9.5 (`Cargo.toml` `license = "Apache-2.0"`, and the
  root `LICENSE`). A web search showed docs.rs listing herdr 0.1.0 under
  AGPL-3.0-or-later from an earlier personal repo. That's unconfirmed, so
  anything ported must come from v0.9.5 or later, with the repo, path and
  tag recorded (`CLAUDE.md`).
- Rust, with a ratatui UI. It vendors libghostty-vt at Ghostty
  `44f2a44df7e8c4a0c6df3f7d872ef3d7ead88e51` (`vendor/libghostty-vt.vendor.json`,
  "libghostty-vt-1.3.2-HEAD"), built with Zig 0.16. That confirms ADR
  0001's note. tai's pin needs Zig 0.17, so the two pins differ.
- There's no library crate: its `crates/ghostty-vt` is `publish = false`.
  Integration means talking to the `herdr` binary or its socket.

### The server and its clients

- A background server owns the panes, shells and agents. Clients attach
  and detach, and the panes keep running. Named sessions are separate
  servers with their own sockets. `herdr --remote <ssh-target>` attaches
  over SSH, with the local client drawing the UI.
- The server parses each pane with libghostty-vt. Clients get "terminal
  content", not the raw PTY bytes: "Direct attach streams the current
  rendered terminal state, then live ANSI frames"
  (`persistence-remote.mdx`).
- The full UI talks to the server over a "stable endpoint generation"
  that negotiates codecs and capabilities, so client and server builds
  needn't match. It isn't documented for outside clients.

### The JSON socket API

- **Transport:** newline-delimited JSON over a Unix domain socket, or a
  named pipe on Windows. The socket is `~/.config/herdr/herdr.sock`, or
  `~/.config/herdr/sessions/<name>/herdr.sock` for a named session.
  `HERDR_SOCKET_PATH` overrides it.
- **Envelope:** `{"id","method","params"}` in, then `{"id","result"}` or
  `{"id","error":{"code","message"}}`. `ping` returns `protocol`,
  `version` and `capabilities`.
- **Stability:** "JSON API clients should ignore unknown fields and handle
  unsupported methods as normal errors." There's no semver promise. herdr
  ships stable releases about every one to three weeks (0.9.0 on
  2026-09-07, 0.9.5 on 2026-10-14).
- **Reading a pane:** `pane.read {pane_id, source:
  visible|recent|recent_unwrapped|detection, lines, strip_ansi}` returns a
  snapshot with `format: text|ansi`, a `revision` and `truncated`. There's
  also `pane.wait_for_output` (substring or regex, with a timeout).
- **Writing to a pane:** `pane.send_text`, `pane.send_keys` (key names such
  as `ctrl+c`), `pane.send_input`, and the CLI's `pane run` (text and
  Enter together).
- **Structure:** `session.snapshot`, `workspace|tab|pane.list/get`,
  `pane.split` (no command field), `tab.create`, and `layout.apply`,
  whose `LayoutNode` can carry a `command`, `cwd` and `env`.
- **Events:** `events.subscribe` keeps the connection open and pushes
  `{event, data}`. Events cover workspaces, tabs, panes, `layout.updated`,
  `pane_agent_detected`, `pane_agent_status_changed` and
  `pane_output_changed`. `pane_output_changed` is a notification with a
  revision; it carries no bytes. A slow reader gets `events_lost` and
  must resubscribe and re-read `session.snapshot`.
- **No raw output stream:** the JSON API has no stream of a pane's bytes.

### Terminal sessions for third-party bridges

This is the piece that matters most for tai-gui. herdr documents it "for
third-party bridges that only need rendered terminal bytes"
(`persistence-remote.mdx`, "Direct terminal attach"):

```bash
herdr terminal session observe w1:p1 --cols 120 --rows 40            # read-only
herdr terminal session control w1:p1 --takeover --cols 120 --rows 40 # read-write
```

- **Output:** both print newline-delimited JSON `terminal.frame` records
  with **base64 ANSI bytes**, then `terminal.closed` when the stream ends.
  Any number of observers can watch a terminal at once.
- **Input:** control mode also reads newline-delimited JSON on stdin:
  - `terminal.input` (text or base64 bytes)
  - `terminal.resize`
  - `terminal.scroll`
  - `terminal.mouse` (an event at a cell, for applications that turned
    mouse reporting on)
  - `terminal.release`
- **Ownership:** one controller owns input and resize at a time, and
  `--takeover` replaces it.
- **Platforms:** Linux and macOS only. "Direct terminal attach is
  available on Linux and macOS, not native Windows."
- **Builds:** this rides on "the numbered binary protocol … for
  same-install and internal operations". So the `herdr` binary tai runs
  must be the one that owns the server (`HERDR_BIN_PATH` inside a pane).
  Check `ping` first.
- **Not documented:** the exact fields of `terminal.frame` and of the
  stdin commands, and whether a frame is a full repaint or a diff. Since
  the bytes are ANSI, libghostty-vt takes either.

### Agents

- herdr tracks agents in panes. It shows their state, `idle`, `working`,
  `blocked`, `done` or `unknown`, in its sidebar, and it notifies when
  one finishes or is blocked. It finds them in two ways: screen detection
  from manifests that herdr.dev updates, and integrations that report
  their own state.
- Any agent can report its own state without changes to herdr
  (`add-herdr-support.mdx`):
  - **State:** when `HERDR_ENV=1`, the agent runs `"$HERDR_BIN_PATH" pane
    report-agent "$HERDR_PANE_ID" --source <s> --agent <name> --state
    working|idle|blocked --seq <increasing>`.
  - **Resume:** a resume command after `--` (herdr 0.9.2 or later) lets
    herdr reopen the session after a server restart.
  - **Exit:** `pane release-agent` on exit.
  - **Over the socket:** `pane.report_agent`, `pane.report_agent_session`
    and `pane.release_agent`.
  - **Display:** `pane.report_metadata` adds a title, labels and token
    counts.
- **Environment:** herdr injects `HERDR_ENV=1`, `HERDR_PANE_ID`,
  `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`, `HERDR_SOCKET_PATH` and
  `HERDR_BIN_PATH` into its panes, and its values win over a caller's
  `env`.

### Plugins

A plugin is a `herdr-plugin.toml` manifest plus argv commands, in any
language. Plugins can't draw native UI: their panes are terminal panes,
placed as `overlay`, `popup`, `split`, `tab` or `zoomed`. Plugin v1 has
"no native non-terminal plugin UI". So a plugin could run a TUI inside
herdr, but it can't host tai-gui's chat or previews.

## Ways tai and tai-gui could support herdr

### A. tai as herdr's outer terminal

This works today. ADR 0013 records it on Windows, and nothing is built
for it. Worth checking on Linux and macOS, because herdr leans on its
outer terminal for:

- **Images:** `[terminal] kitty_graphics = true` renders pane images to
  "Kitty graphics-compatible outer terminals". tai draws Kitty images
  (ADR 0005), but virtual placements through a multiplexer are untried.
- **Keys and mouse:** the `ctrl+b` prefix, mouse capture, right-click
  routing, and copy-on-select (OSC 52, if herdr sends it).
- **Titles:** the window title (`herdr terminal title`).

Cost: a test pass, plus lessons in `docs/lessons/`. Risk: none.

### B. herdr panes as tai-gui tabs

tai-gui would get a new tab kind, a herdr terminal. Its pane runs `herdr
terminal session control <pane_id> --cols C --rows R` as a child process
instead of a PTY:

- **Output:** each `terminal.frame` is decoded from base64 and written to
  that tab's libghostty-vt `Terminal`, the way PTY bytes are now. The cell
  renderer, selection, scrollback view and fonts stay as they are.
- **Input:** tai's key encoder already produces bytes, which are sent as
  `terminal.input` (base64).
- **Resize:** a resize becomes `terminal.resize`.
- **Mouse:** a mouse report becomes `terminal.mouse`. The pane's own
  mouse modes live in herdr's terminal, so tai should pass events through
  rather than encode them itself; check this in the spike.
- **Lifetime:** closing the tab sends `terminal.release`. The herdr pane
  keeps running, unlike a tai terminal.
- **Choosing panes:** a picker from `session.snapshot` or `pane.list`
  over the socket, kept current by `events.subscribe`. It shows each
  pane's agent and status, so a tab can be titled after its agent and
  badged when it's `blocked`.

What it gives: herdr's sessions in tai's window, next to the chat and the
previews. The shells and agents outlive tai-gui, which is ADR 0011's
detachable mode without building `tai --server`. It works with a remote
herdr too, once `--machine` or `--remote` is worked out (not researched).

Limits:

- **No native Windows:** direct attach doesn't run there. On Windows this
  option would need the JSON API's `pane.read` snapshots plus polling on
  `pane_output_changed`. That redraws the whole screen and has no
  scrollback stream.
- **Same install:** the bridge must use the `herdr` binary of the
  running server, and the frame format isn't specified. tai would pin a
  herdr version range and keep a contract test, as it does for ppxai's
  unstable routes (ADR 0001).
- **Double parsing:** each byte is parsed twice, by herdr's libghostty-vt
  and then by tai's. That should be cheap, but it's an extra hop for
  latency; measure it in the spike.
- **Unknown size handling:** two clients with different sizes on one
  pane (herdr's own UI and tai) is unresolved. `--cols/--rows` sets the
  controller's viewport.

Cost: one new pane backend behind the existing terminal pane, an NDJSON
child-process reader, a socket client for the picker, and tests. The size
is like ADR 0013's terminal work.

### C. ppxai reports its state to herdr

A ppxai TUI or a ppxai agent run in a herdr pane would report `working`,
`idle` and `blocked` the way `add-herdr-support.mdx` describes, with a
resume command (`ppxai --resume <session>` or whatever ppxai's flag is).
herdr's sidebar, notifications and `herdr agent wait` would then cover
ppxai like the agents herdr supports itself.

This is ppxai's code, not tai's.
[`../upstream/ppxai-herdr-agent-state.md`](../upstream/ppxai-herdr-agent-state.md)
is the change request.

There's one hazard to fix on tai's side first. ADR 0013 keeps the
`HERDR_*` variables in tai-gui's shells when tai-gui is started from a
herdr pane. A ppxai started in one of those shells would then report as
the herdr pane that started tai-gui, and it would mark that pane
`working` or `idle`. tai-gui (and tai) should then remove
`HERDR_PANE_ID`, `HERDR_TAB_ID` and `HERDR_WORKSPACE_ID` and keep the
rest, or remove all of them. This reverses part of ADR 0013's choice, so
it's the owner's call.

### D. ppxai's terminal commands reach herdr panes

ADR 0013's step 3 is a model running commands in a terminal and reading
the output back. herdr already has this for its panes: `pane run`,
`pane.read` and `pane.wait_for_output`, with agent names as aliases. If
tai-gui has herdr tabs (option B), `/terminal <alias> -- cmd` could
target one through the socket instead of typing into a PTY. That avoids
ADR 0013's alternate-screen rule, since herdr routes to the right pane.

This depends on B, and on step 3's own ADR.

### E. herdr as tai-gui's only terminal backend

Every tai-gui terminal would be a herdr pane, so herdr becomes tai's
detachable server (ADR 0011 decision 8). That's rejected for now:

- herdr would become a required 26 MB third-party daemon with its own
  update channel.
- Direct attach doesn't work on native Windows.
- ADR 0011 already chose herdr as a reference, not a dependency.

B gives the same benefit to people who use herdr, without making it a
dependency for those who don't.

### F. herdr agent status in tai-gui without herdr tabs

A small subset of B: subscribe to `pane_agent_status_changed` and show
herdr's agents in tai-gui, with notifications when one is `blocked` or
`done`, and a click that opens the pane (B) or focuses it in herdr's UI
(`agent.focus`). This needs only the socket client.

## Recommended order

1. **A, a test pass.** Run herdr inside `tai` and `tai-gui` on macOS and
   Linux, covering images, the prefix key, mouse, OSC 52 and titles.
   Write up what breaks in `docs/lessons/`.
2. **Spike B on Unix.** Run `herdr terminal session control` in a
   separate named session (`herdr --session tai-spike`), never the
   user's. Answer the open questions:
   - what `terminal.frame` holds: full repaint or diff, its fields, and
     whether the first frame includes scrollback;
   - the stdin command fields;
   - latency against a local PTY;
   - what a second client of another size does;
   - mouse pass-through.
   If it holds up, write an ADR for herdr tabs.
3. **The `HERDR_*` question from C.** The owner decides it, and it's a
   small change in `src/identity.rs`, which both binaries share.
4. **Send C to ppxai** when the owner agrees. It doesn't depend on tai.
5. **F, then D,** once B's socket client exists.

## Open questions

- **Frames:** the format of `terminal.frame` and the control commands,
  beyond their names. Nothing describes them except
  `persistence-remote.mdx`.
- **Windows:** whether native direct attach is coming. It's needed before
  B can be a cross-platform feature.
- **Licence history:** whether herdr was AGPL before Apache-2.0, and from
  which version.
- **Agent names:** whether `pane.report_agent` accepts any `agent` name
  without a detection manifest. The guide implies yes ("Use your own
  agent's name").
- **Pane commands:** `pane.split` takes no command. A herdr tab started
  from tai-gui with a given command would go through `layout.apply` or
  `pane run`.
