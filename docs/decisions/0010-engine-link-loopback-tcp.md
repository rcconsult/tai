# ADR 0010: Reach ppxai-server over loopback TCP on every platform

- **Status:** Accepted 2026-11-01 (owner): loopback TCP on every
  platform now, with ppxai's strict mode (decision 5) scheduled to close
  the accepted risks.
- **Deciders:** owner. Written by the Claude session on the Windows dev
  host, with input from the macOS and Linux sessions and from the ppxai
  session on the Windows host.
- **Supersedes:** decision 4 of [ADR 0001](0001-stack-rust-libghostty-vt-ppxai-server.md)
  (`--uds --announce`), and the condition in decision 7 of
  [ADR 0002](0002-windows-11-support.md) that its two open gaps close
  before Windows gets the engine link.
- **Related:** ppxai `docs/api-gateway.md`, ppxai ADR 0013 (the
  announce contract).

## Context

ADR 0001 has tai launch `ppxai-server --uds --announce`: a 0600 unix
socket, the env token forced to the front of the secrets chain, and the
address and token reported to the parent. ADR 0002 found that this can't
work on Windows, and chose loopback TCP there, pending two gaps.

Read in ppxai `feature/v1.19.5` at `b3433545` ("docs(changelog): Python
3.15 support"), and confirmed by the ppxai session:

- `--uds`, `--announce` and `--detach` are refused on win32
  (`ppxai/server/http.py`, `_run_announced`). Python on Windows has no
  `socket.AF_UNIX` and no `asyncio.start_unix_server`, checked on 3.13.15
  and 3.15.2 on the Windows host, so this is not a flag away.
- In plain `--host/--port` mode, a token-less request from a loopback
  address skips auth on every route except `/v1/agent/*` and `/v1/tokens`
  (`ppxai/server/auth.py`, `_is_loopback_ui_request`). That includes
  `/chat`, which can run tools, and `/v1/oneshot`. It's by design, for the
  local web UI.
- Only `_run_announced` forces the env token first. Otherwise the chain
  is exactly `server.secrets.providers` from the user's config, and the
  env provider on `PPXAI_API_TOKEN` is added only when that list is empty
  (`ppxai/server/secrets/__init__.py`, `build_chain_from_config`). So a
  config that lists providers without an `env` entry on
  `PPXAI_API_TOKEN` (a `file` store alone, say) ignores the token tai
  puts in the child's environment. A `file` store with such an `env`
  entry beside it still accepts it: the Linux host's own config is
  `[file, env PPXAI_API_TOKEN]`, and its probe below ran with it.
- Nothing reports a port-0 bind back to the parent, so a parent-chosen
  port can be taken first.
- ppxai's own desktop app (`ppxai-desktop.py`) already runs the server
  this way on Windows: bare, on port 54320, with no token.

Probes of `ppxai-server` 1.19.5, started with
`--host 127.0.0.1 --port <free port>` and `PPXAI_API_TOKEN` in its
environment. Windows and macOS ran the installed one-file build; Linux
ran b3433545 from source under `uvx`, where the two processes are `uvx`
and python, split the same way as a one-file build's bootloader and
child:

| | Windows 11 | macOS 12 | Pop!_OS (b3433545 via uvx) |
|---|---|---|---|
| `GET /health` with the bearer answers 200 after | 7.0 s | 4.2 s | 1.5 s |
| `GET /health` without a token | 200 (loopback exemption) | 200 | 200 (bad token: 401) |
| `GET /v1/agent/runs`: no token / wrong token / token | 401 / 401 / 200 | not run | 401 without a token |
| Other routes without a token | not run | not run | `/v1/tokens` 401; `/sessions`, `/models` 200 |
| Processes (launcher and server) | 3 | 2 | 2 |
| Stopped with none left by | `taskkill /T /F` | `killpg(SIGTERM)` on its own session | `killpg(SIGTERM)`; a SIGKILL of the launcher alone left the server running and listening |

## Decision

1. **One engine link everywhere: loopback TCP.** tai picks a free port
   (bind port 0, read it, close), and starts
   `ppxai-server --host 127.0.0.1 --port <n>` with a fresh random
   `PPXAI_API_TOKEN` in the child's environment. It sends that token as
   `Authorization: Bearer` on every request.
2. **Readiness and token check at startup.** tai polls `GET /health`
   with the bearer until it answers.
   - **200:** the server is up and accepts tai's token.
   - **401:** the server doesn't accept tai's token, because the user's
     `server.secrets.providers` has no `env` entry on `PPXAI_API_TOKEN`.
     A request that carries a bearer never gets the loopback exemption,
     so this answer comes at once. tai stops the server and says why,
     instead of waiting for a timeout.

   tai then checks that `GET /v1/agent/runs` without the token answers
   401. That guards against auth being off entirely; it can't happen
   while tai sets the token and the chain includes `env` on it, but it
   costs one request. A bind failure (the port was taken) means a new
   port and a new start.
3. **tai owns the whole process tree.** On Unix the server starts in its
   own session (`setsid`), and tai stops it with `killpg`. On Windows it
   starts in a job object with kill-on-close, so the bootloader's children
   go too, even if tai crashes. Unix has no equivalent when tai itself is
   SIGKILLed: `PR_SET_PDEATHSIG` reaches only the direct child, which in a
   one-file build is the bootloader, and the bootloader can't forward a
   SIGKILL. So tai records the server's process group in a pidfile and
   stops a leftover group at its next start.
4. **Accepted risks, the same as ppxai-desktop's today:** any local
   process, including another user's, can use `/chat` (which runs tools
   with the owner's rights and provider keys), `/consent` and
   `/v1/oneshot` without a token; on Linux and macOS the 0600 socket's per-user
   protection is given up; the port can still race, mitigated by the
   retry.
5. **ppxai gets a strict, opt-in TCP mode, and tai moves to it.** As the
   ppxai session proposed (its options B and C, plus D on Unix): the env
   token forced to the front of the chain and required on every route,
   with no loopback exemptions; a port-0 bind reported as one JSON line
   on stdout once listening; and, on Unix, the server exiting when its
   parent goes away. That closes all three risks in decision 4. ppxai
   records the mode in its own ADR. When it ships, tai passes the flag,
   reads the port from stdout instead of picking one, and drops the
   decision 2 probe of `/v1/agent/runs` and the pidfile sweep.

ADR 0001's decisions 5 and 6 (one client module, pinned contract test)
stand. `/v1/oneshot` is the stable route and comes first, pinned to ppxai
v1.19.4 or later. `/chat`, `/interrupt` and `/consent` are internal, so
their contract test pins a commit and expects churn.

## Why this and not the alternatives

- **Keep `--uds --announce` on Linux and macOS, TCP on Windows:** keeps
  the socket's protection there, at the cost of two transports to build
  and test. The owner chose one path.
- **Wait for the strict ppxai mode first:** closes the risks from day
  one, but blocks tai on a ppxai contract and release. tai's client
  doesn't change shape when the mode lands, so it can start now.
- **TCP `--announce` on win32, or named pipes:** the first contradicts
  ppxai ADR 0013 ("socket-only, never TCP"); the second needs a transport
  uvicorn doesn't have.

## Triggers to revisit

- ppxai ships the strict mode (decision 5). Then tai switches to it,
  and this record gets a dated note.
- tai is to run on shared multi-user machines before the strict mode
  ships.
- The 401 in decision 2 turns up for real users (configs without an
  `env` entry are common), or `/v1/agent/runs` stops being protected
  from loopback.
- Startup time (4–7 s) gets in the way. Then start the server earlier or
  share one between tai windows.

## Notes

- **2026-11-01, the strict mode exists.** ppxai `feature/v1.19.5` at
  `0ca511a7` adds `ppxai-server --owned` (ppxai ADR 0017,
  `docs/decisions/0017-parent-owned-server-mode.md`). It's unreleased and
  ships with 1.19.5. Its contract, from the ppxai session:
  - `PPXAI_API_TOKEN` must be set in the child's environment, or the
    server exits 2 before binding. It's honoured even under a file-only
    secrets chain.
  - The bearer is required on every route except CORS `OPTIONS`,
    including `/health` and `/status`. A bearer probe of `/health`
    answers 200 when ready, and 401 without the bearer.
  - It binds 127.0.0.1 itself (port 0 unless `--port`; a non-loopback
    `--host` is refused). The first stdout line is exactly one JSON
    object, `{"host": "127.0.0.1", "port": N, "pid": P, "version":
    "1.19.5"}`, meaning "bound"; readiness is still the `/health` probe.
    Later stdout carries the normal startup lines, and logs go to stderr.
  - `pid` is the real server's, not the PyInstaller bootloader's.
  - On POSIX, EOF on a piped stdin stops the server gracefully. On
    Windows, tai's job object covers it.
  - Idle shutdown and the SSH hub are off, and no registry entry is
    written. Combining it with `--uds`, `--announce`, `--detach` or
    `--reload` exits 2.
  - On the Windows host the first line arrived about 17 s after launch.
    That isn't `--owned`: debug logging in the user's config shells out
    to `git` for a version banner, which takes about 9 s there. The
    first-line timeout must be generous.

  So tai starts with `--owned`. A server older than 1.19.5 rejects the
  flag and exits 2 with a usage error, and then tai falls back to
  decisions 1 and 2. Decision 5 is done on ppxai's side; this record's
  accepted risks close for servers that have the flag.
