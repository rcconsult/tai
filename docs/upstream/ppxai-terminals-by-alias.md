# ppxai change request: `/terminal [alias] [-- command]`

For the ppxai session. From tai's [ADR 0013](../decisions/0013-terminals-by-alias.md)
(Proposed). Nothing is implemented in either repo. Facts below are from
ppxai `e3ae990b`, "feat(web): web images in an answer wait for the user's
yes".

## What tai asks for

One optional field on two existing side effects, and `/terminal` taking
arguments. ppxai stores nothing about a client's terminals.

| Effect | Today | Asked for |
|---|---|---|
| `open_terminal` | `{cwd}` | `{cwd, name?}` |
| `run_shell` | `{command, cwd}` | `{command, cwd, name?}` |

`name` is a string, the terminal's alias: either a number (`"2"`) or a
name following the Kubernetes DNS label rule (RFC 1123: 1–63 characters,
lowercase `a-z`, `0-9`, `-`, starting and ending with a letter or digit)
that also contains a letter. Absent means "the terminal that last had the
focus", as now.

## Changes in ppxai

1. **`/terminal [alias] [-- command]`** (`ppxai/commands/system.py`,
   `handle_terminal`, around line 788). Today it ignores `args`.
   - No arguments: as now, `open_terminal {cwd}`.
   - `alias`: `open_terminal {cwd, name}`.
   - `alias -- command`: `run_shell {command, cwd, name}`.
   - `-- command` without an alias: `run_shell {command, cwd}`.
   - An alias that breaks the rule: an error result saying why, and no
     side effect.
   - Update the usage string (line ~887) and the stale `spawn_terminal`
     comment in the function.
2. **Help text only in the TUI.** In tai-gui, `/terminal` shows the TUI's
   image-protocol help as the answer. Outside the TUI, reply with a short
   notification instead ("Opening terminal build"). In the TUI, `/terminal`
   with an alias has no terminals to address: say so, and keep the help.
3. **Document `name`** on both kinds in the `SideEffect` docstring
   (`ppxai/commands/results.py`, around line 308).
4. **Web app** (`ppxai/web/shared/side-effects.js`): `open_terminal` with a
   `name` opens or focuses the terminal panel with that alias; `run_shell`
   types the command into it without Enter, as far as `TerminalView`
   allows (today it only shows "Run in terminal: …").
5. **VS Code** (`vscode-extension/src/sideEffectsHandler.ts`):
   - `open_terminal` with `name`: find a terminal with that name
     (`vscode.window.terminals`) and show it, or `createTerminal({name,
     cwd})`.
   - `run_shell`: the same, then `sendText(command, false)`. **This is a
     behaviour change:** today it runs the command. The owner's rule for
     all clients is to type it and let the user press Enter.
6. **Tests:** the payload shapes, the cross-client kind sentinel
   (`tests/test_command_envelope.py`), the VS Code helper test that checks
   `sendText` (`test_vscode_step5a_helpers`), and the alias rule.

## What tai does with it

tai keeps the alias map: an internal ID per terminal, a number alias (the
lowest free), and an optional name. It routes by alias, creates a missing
terminal, restarts an exited one, types `run_shell` commands without
running them, and focuses the terminal pane. tai also checks the alias,
so a client that skips ppxai's check still can't create a bad one.

## ppxai's review (win32-ppxai, 2026-11-23, at `5bb39ad4`)

Agreed, with these answers and corrections. Not started; waits for the
owner.

- **Field name:** `name`. VS Code's `createTerminal({name})` already uses
  it, and `alias` means command aliases in ppxai's `CommandSpec`.
- **`--`:** no collision. `handler.py` splits the command once
  (`split(maxsplit=1)`) and the server route passes `args` raw
  (`routes/commands.py`). The handler splits on the first ` -- ` (or a
  leading `-- `) and keeps the rest verbatim, not shlex'd, so quotes and
  pipes reach the shell untouched. An empty command after `--` is an
  error.
- **VS Code:** a CHANGELOG line is enough. No Python code emits
  `run_shell` today, so `sendText(command, false)` changes no current
  command. The code-block "Run" button (`chatPanel.ts`,
  `runCommandInTerminal`) is a separate path, left alone unless the owner
  wants the rule there too. A ppxai ADR (next free number 0020; ppxai's
  own 0013 is the SSH hub) only if the owner wants the type-don't-run rule
  recorded once for all clients; otherwise it goes in the `SideEffect`
  docstring.
- **Help only in the TUI** (item 2 above): the `ImportError` fallback
  never fires, since `ppxai.tui.terminal` ships in every install; that's
  why tai-gui sees the help. The branch goes on the context:
  `ServerCommandContext` (any HTTP client) gets the notification, Rich and
  Textual the help. `_help_client(context)` in `system.py` already splits
  them.
- **Client id:** `KNOWN_CLIENTS` is `{rich, textual, web, vscode}` and
  the route answers 400 to others. tai-gui already sends `web`
  (`COMMAND_CLIENT`, `src/bin/tai-gui/ppxai.rs`); adding `tai` there is a
  separate decision.
- **TUIs:** `alias -- command` errors like `alias`, since they have no
  terminals. No new `client_action`, so the parity fence needs nothing.
- **Validation:** one helper with one regex, imported by the tests:
  `^(?:[0-9]+|(?=[a-z0-9-]*[a-z])[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)$`.
- **Order:** Python and VS Code first. The web app needs a new
  `TerminalView.typeText` that writes to the PTY websocket without a
  newline, the biggest piece, done last.
