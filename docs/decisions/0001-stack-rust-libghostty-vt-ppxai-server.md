# ADR 0001: Rust + libghostty-vt for the UI, ppxai-server as a child process for the engine

- **Status:** Accepted 2026-10-24 (owner). Nothing is implemented yet.
- **Deciders:** owner. Two Claude sessions, one on each dev host (Linux x86_64, macOS 12 Intel), wrote the options and reviewed each other's work.
- **Related:** ppxai `docs/api-gateway.md` (API stability tiers), ppxai ADR 0003 (agent platform, `/v1/agent/*`), ppxai ADR 0016 (cheap engine import).

## Context

tai is a terminal app. It runs the ppxai engine harness behind a UI built on
Ghostty's embeddable terminal library. It must run on Linux and macOS. The
repo had no code when this was decided, so this record picks the stack.

Facts it rests on, as read from the sources on 2026-10-24:

**Ghostty** (`ghostty-org/ghostty`, branch `main`, fetched 2026-10-24, sha
not recorded):

- `include/ghostty.h` is the full surface and renderer API. Its header says
  it is "libghostty-internal". It also says "the only consumer of this API is the
  macOS app" and "external embedders should instead use `libghostty-vt`". Its
  platform enum is `MACOS` and `IOS` only. So the full API is not an option
  on Linux.
- `include/ghostty/vt.h` is **libghostty-vt**. It provides terminal state,
  VT parsing, scrollback, a render-state API for custom renderers, key, mouse
  and focus encoding, search, and Kitty graphics. It does **not** include a
  renderer or a PTY, so the embedder draws the screen and spawns the child.
- The API is not stable yet and there is no tagged release. It builds from
  source with Zig, using `zig build -Demit-lib-vt`. The Zig version you need
  depends on the Ghostty commit; both dev hosts have Zig 0.17.0 (but see
  the 2026-10-24 notes: the pinned build uses a repo-local Zig 0.15.2).
- On macOS, Ghostty's build (`Config.zig`) forces a minimum target of 13.0.
  A smoke-test build of libghostty-vt still ran on macOS 12, but it's
  stamped as needing 13.0. That build was from `ghostty-org/ghostty`
  `22f100c6367045bb5fc86172f837047dee2f7670`, "docs: replace stale function
  name. `ghostty_vt_osc_free()` should be `ghostty_osc_free` (#14818)".
  herdr 0.9.5 vendors libghostty-vt (at Ghostty `44f2a44df`) and ships a
  macOS binary stamped 10.12, so there is a known way around this. herdr
  also carries local patches on its vendored copy, but none of them
  concerns the macOS minimum, so how herdr gets 10.12 is still unexplained
  (explained since: see the 2026-10-24 notes).

**Rust bindings:** the `libghostty-vt` crate on crates.io comes from
`github.com/uzaaft/libghostty-rs`. It's community-maintained and licensed
"MIT OR Apache-2.0". It builds Ghostty with the `zig` on `PATH`, and setting
`GHOSTTY_SOURCE_DIR` makes it use a local checkout instead. Its Ghostty pin
is `GHOSTTY_COMMIT` in `crates/libghostty-vt-sys/build.rs:7`. Release
v0.2.2 (2026-09-28, the latest) pins `a887df42`; the default branch has
since moved to `521cf2c5`. Its repo has a `ghostling_rs` example, a minimal terminal
rendered with macroquad.

**ppxai** (`../ppxai`, branch `feature/v1.19.5` at `f75d751f`, "docs:
staleness sweep across the Markdown docs"):

- The engine can be reached three ways:
  - **In-process:** `from ppxai.engine import EngineClient`. Its async
    `chat()` yields events (`ppxai/engine/client.py:1028`).
  - **HTTP/SSE server:** `ppxai-server` (`ppxai/server/http.py`). It
    listens on 127.0.0.1:54320 by default. `--uds --announce` gives a 0600
    unix socket instead, plus a per-launch bearer token written to a
    registry file.
  - **stdio JSON-RPC:** `python -m ppxai.server` (`ppxai/server/jsonrpc.py`).
- The engine offers three interaction modes. **One-shot** is request and
  reply, with no streaming by design. **Chat** streams. **Tasks**, including
  sub-agents through `spawn_subagent`, stream.
- Stability tiers (`docs/api-gateway.md`):

  | Mode | Route | Streaming | Stability |
  |---|---|---|---|
  | One-shot | `POST /v1/oneshot` | no (by design) | **stable**: v1 semver guarantees (`api-gateway.md:18-29`) |
  | Chat | `POST /chat` (`routes/chat.py:278`), `/interrupt`, `/consent`, `/shell-consent` (`routes/consent.py`) | SSE | **internal**: "may break with any release" (`api-gateway.md:19`) |
  | Tasks / sub-agents | `/v1/agent/run`, `/v1/agent/task`, `/v1/agent/runs/<id>/{events,cancel,respond,ack,resume}` (`routes/agent_v1.py`) | SSE (`events?live=1`) | **not yet sealed**: excluded from the v1 guarantees "despite living under `/v1/`" (`api-gateway.md:44-62`) |

- The stdio JSON-RPC server has no interrupt or consent methods. It runs
  each streaming request to completion with `asyncio.run(...)`
  (`jsonrpc.py:322-323`), and it forwards only `STREAM_CHUNK` events
  (`jsonrpc.py:112-115`). So no request can come in while a reply streams,
  and tool and consent events never reach the client.
- The engine's shell tool runs commands as plain subprocesses with piped
  output (`ppxai/engine/tools/builtin/shell.py`), not in a PTY. So the
  engine never needs tai's terminal.

## Decision

1. **Host language: Rust.** tai uses libghostty-vt through the crates.io
   `libghostty-vt` crate. We pin the Ghostty commit with `GHOSTTY_SOURCE_DIR`
   instead of relying on the crate's default. Before we pin, we find out how
   herdr reaches a macOS minimum lower than 13.0 and apply that approach.
2. **Milestone (a): a working terminal.** tai runs a PTY with `$SHELL`, or
   one of ppxai's own TUIs (`ppxai`, `ppxaide`), inside a libghostty-vt
   terminal. This needs no engine integration. It is rendered with
   macroquad, starting from `ghostling_rs`, with macroquad's audio turned
   off. The renderer is tested early on the weaker dev host, an older Intel
   Mac whose GPU supports only the oldest Metal family.
3. **Milestone (b): a native UI.** tai draws chat from engine events and
   uses libghostty-vt panes for shells. At this point rendering moves to
   winit + wgpu.
4. **Engine link: tai launches `ppxai-server --uds --announce` as its own
   child process.** tai owns its lifetime, and it reads the socket path and
   token from the registry file. The same client can attach to a shared,
   long-running server instead.
5. **One Rust client module covers all three modes**: one-shot on
   `/v1/oneshot`, chat on `/chat` + `/interrupt` + `/consent`, and tasks on
   `/v1/agent/*`. It is the only code in tai that knows ppxai's wire format.
6. **Pin the ppxai version and keep a tai-side contract test** for the
   internal and unsealed routes tai uses. When `/v1/agent/*` is sealed, it is
   the target to move to.

## Why this and not the alternatives

- **Full libghostty (`ghostty.h`):** it supports Apple platforms only, and
  Ghostty says it is not meant for outside embedders.
- **Zig host:** it would use the first-party API with no binding layer. But
  we'd write the GPU renderer and the UI toolkit ourselves, and Zig has the
  thinnest UI ecosystem of the options. Rust has winit and wgpu, and other
  apps already embed libghostty-vt from Rust.
- **Python host with the engine in-process:** this is the only way to embed
  `EngineClient` directly. But terminal rendering in Python at interactive
  frame rates is a real risk. The engine is also heavy to import and its
  dependencies are heavy (`import ppxai.server.http` is still about 1.8 s
  after ppxai ADR 0016). A separate process keeps that cost away from the
  frame loop.
- **Swift / GhosttyKit:** it runs on macOS only.
- **stdio JSON-RPC child:** it has no port, token or registry file, which is
  the simplest option. But it has no interrupt or consent handling and can't
  take a request while streaming, so it can't support (b). We'd revisit it
  only if ppxai adds those features there.
- **GPUI:** it targets newer macOS and Apple Silicon first, and one of our
  dev hosts is macOS 12 on Intel.

## Accepted risks

- **Unstable upstream APIs on both sides.** libghostty-vt has no stable API,
  ppxai's `/chat` is internal, and `/v1/agent/*` is unsealed. The pins, the
  single client module and the contract test (decisions 1, 5 and 6) keep
  the damage contained.
- **The macOS minimum version.** macOS 12 is fine for development. Shipping
  for macOS 12 depends on herdr's approach working, or on patching Ghostty.
- **The Rust binding is community-maintained and pre-1.0**, and its
  Ghostty pin moves between releases.

## Open questions

- **For ppxai's owner:** which surface should an outside consumer like tai
  build chat on: the internal `/chat` routes, or `/v1/agent/*` once it is
  sealed? Will `/chat` ever get a stable tier?
- **For the owner:** is macOS 12 something tai promises to support, or only a
  development host?
- **Later:** macOS code signing and notarization for releases.

## Triggers to revisit

- ppxai seals `/v1/agent/*`, or adds a stable chat route. Then the client
  moves to it.
- ppxai adds interrupt and consent to the stdio JSON-RPC server.
  Then reconsider decision 4.
- Ghostty tags a libghostty-vt release, or ships the GPU renderer library it
  has announced. Then reconsider decisions 1 and 3.
- macroquad or wgpu can't hold an interactive frame rate on the weaker dev
  host.

## Notes

Fact-fix notes under the ADR README's exception. They correct the record
to match what was built and observed, and don't change the decision.

- **2026-10-24, Zig version.** libghostty-vt 0.2.2 pins Ghostty `a887df42`,
  whose `build.zig.zon` declares `minimum_zig_version` 0.15.2. Ghostty
  requires an exact Zig major.minor (`src/build/zig.zig`, `requireZig`), so
  Zig 0.17 refuses to build it. The owner chose to keep the crates.io
  release: `scripts/fetch-zig.sh` fetches Zig 0.15.2 into `.local/`, and
  `scripts/cargo.sh` builds with it first on `PATH`. The system Zig 0.17
  is unused.
- **2026-10-24, macOS minimum.** The milestone (a) build on the macOS dev
  host links libghostty-vt statically. All 11 objects in `libghostty-vt.a`
  carry `LC_BUILD_VERSION` minos 13.0, yet the `tai` binary carries
  `LC_VERSION_MIN_MACOSX` 10.12 (rustc's x86_64 default), links with no
  warnings, and runs on macOS 12. That is very likely how herdr reaches
  10.12, with no Ghostty patch involved. What remains open is whether
  libghostty-vt calls any API newer than macOS 12 at runtime.
