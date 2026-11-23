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

## Questions for ppxai

1. Is `name` the field name you'd choose, or something else (`alias`,
   `terminal`)?
2. Does `--` collide with anything in ppxai's command parsing?
3. Does the VS Code behaviour change need its own note in ppxai's
   changelog or an ADR?
