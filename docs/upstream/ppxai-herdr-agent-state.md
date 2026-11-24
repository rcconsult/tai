# ppxai change request: report agent state to herdr

For the ppxai session. From tai's research
[`../research/herdr-integration.md`](../research/herdr-integration.md),
option C. Nothing is implemented in either repo, and the owner hasn't
decided to send it. Facts about ppxai are from `b3c21798`, "fix(edit):
Cancel in /edit's create prompt ends the command". Facts about herdr are
from v0.9.5's `add-herdr-support.mdx` and its bundled API schema.

## Why

herdr shows each agent's state in its sidebar and notifies the user when
one finishes or needs a decision. `herdr agent wait` and herdr's other
automation wait on that state. herdr already knows Claude Code, Codex and
about sixteen other agents. ppxai is not one of them, so in a herdr pane a
ppxai TUI is an ordinary shell. herdr documents a way for any agent to
report itself without a change to herdr.

## What herdr asks of an agent

Each step happens only when `HERDR_ENV=1` and `HERDR_PANE_ID`,
`HERDR_BIN_PATH` and `HERDR_SOCKET_PATH` are set. Outside herdr, nothing
happens.

1. **State**, whenever it changes:

   ```bash
   "$HERDR_BIN_PATH" pane report-agent "$HERDR_PANE_ID" \
     --source ppxai --agent ppxai --state working|idle|blocked --seq <n>
   ```

   - **`working`:** a turn starts.
   - **`idle`:** ppxai is ready for input again.
   - **`blocked`:** ppxai waits for the user to decide, as with a consent
     request (`EventType.CONSENT_REQUEST`) or a command's `prompt_*`
     question. `--message` says what it's waiting for.
   - **`--seq`:** it must grow across sessions and restarts. A timestamp
     in milliseconds works. herdr ignores reports that aren't newer than
     the last.
   - **`--source`:** must not start with `herdr:`.
2. **Release** on exit: `pane release-agent "$HERDR_PANE_ID" --source
   ppxai --agent ppxai --seq <n>`.
3. **Optional, a resume command:** after `--`, the command that reopens
   the current session, such as `-- ppxaide --resume <session-id>`.
   herdr reruns it in the same pane after a herdr server restart. This
   needs herdr 0.9.2 or later. Neither `ppxai` nor `ppxaide` takes a
   session argument at `b3c21798`; `ppxai` asks at start whether to
   restore one. So this part needs a new flag and can come later.

herdr's rules for doing this:

- Send the reports in the background, with a short timeout, and ignore
  failures.
- When several changes queue up, send only the latest.
- The same calls exist over the socket as newline-delimited JSON:
  `pane.report_agent`, `pane.release_agent` and
  `pane.report_agent_session`. herdr's docs recommend the CLI through
  `HERDR_BIN_PATH` because it also works on Windows, where the socket is a
  named pipe.

## Where it would go in ppxai

The state comes from the engine's events, which both TUIs already handle:

- **The Rich TUI:** `ppxai/rich/event_handler.py`.
- **The Textual TUI:** `ppxai/tui/stream_handler.py`.

So the natural place is one small reporter module shared by both, fed
from those handlers. It could live beside the engine's event types, as a
leaf module with no imports from the TUIs (ppxai's import rules). The web
app and `ppxai-server` don't run in a herdr pane as an interactive agent,
so they don't report.

## Hazard on tai's side

A ppxai TUI started in a tai-gui terminal, when tai-gui itself was
started from a herdr pane, inherits that herdr pane's `HERDR_PANE_ID`
(tai ADR 0013 keeps the `HERDR_*` variables). It would then report as
that pane. tai would fix this on its side by not passing the pane-specific
`HERDR_*` variables to its shells. The research's step 3 records it as
the owner's decision. It doesn't change what ppxai should do.

## How to check it

From herdr's guide: start ppxai in a herdr pane of a **separate** named
session (`herdr --session ppxai-test`), never the user's own. Run a turn
and a consent request, then look at `herdr pane get "$HERDR_PANE_ID"` and
`herdr agent list`.
