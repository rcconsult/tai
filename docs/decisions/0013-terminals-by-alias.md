# ADR 0013: Reach a terminal by its number or name, and type a command into it

- **Status:** Proposed 2026-11-23. The owner settled the open points on
  2026-11-23 (ask before closing a busy terminal; type only at a prompt).
  ppxai agreed to its half ([change request](../upstream/ppxai-terminals-by-alias.md)),
  which ppxai records as its ADR 0020. Accepted when the code lands.
- **Deciders:** owner. Written by the Claude session on the Windows dev
  host, from the owner's answers on 2026-11-23 and the macOS session's
  notes on steps 1–3.
- **Related:** [ADR 0011](0011-native-gui-egui.md) (terminals as panes),
  [ADR 0001](0001-stack-rust-libghostty-vt-ppxai-server.md) (the engine
  doesn't drive tai's terminals; step 3 below would change that).

## Context

The owner, on 2026-11-22: "since we have terminal open we could check if
the terminal window exists we either do nothing or create a new terminal
window, optionally we might augment terminal command to accept terminal ID
to send a command to the terminal by ID or create a new terminal with ID if
does not exist already open".

The macOS session split this into three steps:

1. **Several terminals in tai-gui.** Done on main (`804721f`, "tai-gui:
   several terminals, View > New terminal"). Tabs are titled "Terminal",
   "Terminal 2", …. ppxai's `open_terminal` (`/terminal`) and `run_shell`
   go to the terminal that last had the focus; a running one is focused,
   an exited one restarts in the chat's folder. Terminals have no name or
   ID a user can give.
2. **Terminals by alias.** This record.
3. **The model working in a terminal.** Recorded below, not decided.

What ppxai sends today (ppxai `e3ae990b`, "feat(web): web images in an
answer wait for the user's yes"):

- `open_terminal {cwd}` from `/terminal` (`ppxai/commands/system.py`,
  `handle_terminal`), which ignores its arguments. In ppxai's TUI it also
  prints image-protocol help; tai-gui shows that help too.
- `run_shell {command, cwd}` (`ppxai/commands/results.py`). No built-in
  command sends it; user commands can.
- ppxai's web app and VS Code extension destructure only the keys they
  know (`ppxai/web/shared/side-effects.js`,
  `vscode-extension/src/sideEffectsHandler.ts`), so a new key is safe for
  them. VS Code's `run_shell` runs the command (`sendText` adds Enter);
  tai-gui types it and leaves Enter to the user; the web app shows it as
  a line in the chat.

## Decision

1. **Every terminal has an internal ID** that tai gives it when it starts
   and never reuses. Users never see or type it. Everything else refers to
   a terminal through an alias.
2. **A terminal has a number alias, and may have a name alias.**
   - The number is the lowest positive integer no live terminal holds,
     taken when the terminal is created. The tab shows the name when
     there is one, else "Terminal N", and number 1 is titled "Terminal".
     Since any terminal can be closed, number 1 can be freed and taken
     again.
   - A name follows Kubernetes' DNS label rule (RFC 1123): 1–63 characters,
     lowercase `a-z`, `0-9` and `-`, starting and ending with a letter or
     digit. Unlike Kubernetes, a name must contain a letter, so it can
     never be read as a number.
   - Both aliases are unique among live terminals. They're freed when the
     terminal is closed, and can then be taken again.
   - **Closing a terminal's tab ends its shell** and closes the terminal.
     This replaces step 1's rule that a closed tab hides a running shell
     (and that the first terminal is never removed).
     - **Ask before closing** when the shell isn't at its prompt (a
       build, an editor, ssh), as Ghostty's `confirm-close-surface` does
       (default true, skipped when shell integration says nothing is
       running; Ghostty `src/config/Config.zig` at `108cacb1`). This uses
       decision 7's prompt marks; without them, as with PowerShell and
       cmd, always ask. An exited shell closes without asking.
     - Ending the shell doesn't block the window. On Unix, dropping a
       child sends SIGHUP and waits up to 1 s before SIGKILL
       (`src/pty_unix.rs`), so a closed terminal is dropped on a thread of
       its own. On Windows the kill doesn't wait; closing the pseudo
       console is checked when this is built.
   - **A terminal can be named or renamed from its tab** (right-click), as
     well as from `/terminal`. The same rules apply; a name taken by
     another terminal is refused. Typing a number there renumbers the
     terminal, if no other terminal holds that number (found in the
     owner's first test: closing Terminal 2 and giving Terminal 3 the
     free number).
3. **`/terminal [alias] [-- command]`**, in ppxai's command, is how a user
   reaches one:
   - `/terminal`: as now, the terminal that last had the focus, or the
     first.
   - `/terminal 2`: the terminal with number 2; if none, a new terminal
     that takes number 2, which can leave gaps (1, 2, 7).
   - `/terminal build`: the terminal named `build`; if none, a new one
     with that name and the next free number.
   - In each case a running terminal is shown and focused; an exited one
     restarts in place; a new one starts in the chat's folder.
   - `/terminal build -- npm test`: the same, then the command is typed
     into it. **It isn't run:** the terminal pane takes the focus and the
     user presses Enter. One line, control characters dropped. Where it's
     typed is decision 7. `/terminal -- npm test` types into the terminal
     that last had the focus.
   - An alias that breaks the rules is refused with a message saying why.
4. **ppxai carries the alias and stores nothing.** `open_terminal` and
   `run_shell` gain an optional `name` (a string: a number alias is
   digits). tai keeps the alias map. ppxai stays stateless about a client's
   terminals, and every ppxai client applies the same rules (the change
   request covers the web app, VS Code and the TUI).
5. **Aliases don't outlive tai-gui.** tai-gui doesn't restore terminals,
   so there's nothing to restore them on.
6. **Recorded for later, not decided: the model addressing terminals
   (step 3).** The model could name a terminal by alias when it asks for a
   command, with the user's consent per command, like the shell consent
   card. Open: whether it reads the output back (scrollback, prompt marks
   with OSC 133), how consent looks, the alternate screen, and how this
   sits beside ppxai's own `run_shell` tool. It needs its own ADR, since it
   changes what ADR 0001 assumes.
7. **Type only at a shell prompt.** Today
   `run_shell` refuses only while a full-screen program holds the
   alternate screen (`type_command`, `src/bin/tai-gui/terminal.rs`); a
   program on the main screen (python, ssh, `cat`, a `read` prompt) still
   gets the text. tai's shell integration for zsh and bash would send
   OSC 133 prompt marks, and tai would type only right after a prompt
   mark, refusing otherwise with a line in the chat. Shells without the
   integration (PowerShell, cmd) keep today's alternate-screen rule. The
   same marks serve decision 2's close question and step 3.

## With herdr

herdr 0.9.5 is a terminal multiplexer with its own workspaces, tabs and
panes, and a persistent server. Checked on Windows on 2026-11-23, with tai-gui
at `89e88b0`; Linux and macOS not yet.

- **herdr inside a tai terminal.** It runs and draws correctly. Its
  server is a child of the herdr client in tai's shell, with its own
  console. When tai-gui quit, the client ended and the server kept
  running, with the session listed as running; `herdr --session <name>`
  from any terminal attaches again.
  - Aliases name **tai's** terminals. `/terminal build` brings forward
    the tai tab that runs herdr; it can't reach a herdr pane.
  - `/terminal build -- cmd` into that terminal is **not typed**: herdr
    holds the alternate screen, and tai doesn't type into a full-screen
    program (the rule `run_shell` follows now). Otherwise the keys would
    go to whichever herdr pane has the focus. herdr's own CLI
    (`herdr pane run`) reaches its panes.
  - Closing the tab (decision 2) ends the shell and the herdr client, not
    herdr's sessions.
  - The names don't clash: tai's aliases and herdr's names (agent names
    `[a-z][a-z0-9_-]{0,31}`) are separate. A name valid in both is
    lowercase letters, digits and `-`, starting with a letter, at most 32
    characters.
- **tai-gui started from a herdr pane.** tai-gui's shells inherit
  `HERDR_ENV`, `HERDR_PANE_ID` and the rest; the owner chose to keep them,
  to respect herdr's nested launch. So herdr's CLI, or an agent following
  herdr's skill, in a tai terminal believes it is in the herdr pane that
  started tai-gui: `herdr pane split --current` acts on that pane. This
  record doesn't change that.
- **For step 3:** herdr already gives agents a CLI to run commands in a
  pane and read the output back (`pane run`, `pane read`,
  `pane wait-output`). Step 3's ADR should study it, as ADR 0011 does for
  herdr's server.

## What changes in tai-gui

Step 1's code that this replaces, from the macOS session's review:

- `Tab::Terminal(usize)` is a slot index titled from `i + 1`; it becomes
  `Tab::Terminal(id)`, with a map from ID to pane and the two aliases.
  `App.last_terminal` becomes an ID.
- `on_close` in `main.rs` hides a running terminal and keeps the first;
  closing now ends and drops any terminal, off the UI thread.
- `focus_tab` puts a new terminal tab beside the last terminal. With every
  terminal closable it needs a fallback when none is left: the stack the
  first terminal started in.
- View > Show no longer lists hidden terminals; `initial_layout` starts
  with the first terminal's ID.
- Rename uses egui_dock 0.21.1's `TabViewer::context_menu`.

## Why this and not the alternatives

- **Internal ID plus aliases, not the alias as the ID:** an alias can be
  freed and taken again, so it can't be what tai holds on to. A number
  that's reused also matches how the tabs already read.
- **Numbers and names both, told apart by a letter:** the owner asked for
  both. Requiring a letter in names removes the only ambiguity.
- **Type, don't run:** a command from ppxai, or later from the model,
  never runs without the user seeing it at the prompt first.
- **ppxai's `/terminal`, not a tai-only `/term`:** a tai command would
  clash with ppxai's command list, and the other clients get the feature
  too.
- **Names live in tai, not ppxai:** the terminals are tai's; ppxai would
  only hold a copy that can go stale.

## Triggers to revisit

- ppxai declines the payload change, or chooses another field name.
- Step 3's ADR needs the model to read terminals back by alias.
- Users want terminals, and so aliases, restored across restarts.
