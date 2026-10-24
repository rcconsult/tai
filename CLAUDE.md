# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**tai** is a terminal app that runs the **ppxai engine harness** behind a UI built on **libghostty** (Ghostty's embeddable terminal library).

**Status:** milestone (a) is scaffolded. `src/main.rs` is `ghostling_rs` from libghostty-rs v0.2.2 with small tai changes, and its header records the provenance. There are no tests yet.

## Build

Always build through `scripts/cargo.sh`, never plain `cargo`. libghostty-vt's build script runs whichever `zig` is on `PATH`. The pinned Ghostty needs an exact Zig major.minor, 0.15.x, and that's not the system Zig. On first use, the wrapper fetches Zig into `.local/zig/` (`scripts/fetch-zig.sh`, checked against ziglang.org's sha256) and Ghostty into `.local/ghostty/` (`scripts/fetch-ghostty.sh`). `.cargo/config.toml` points `GHOSTTY_SOURCE_DIR` at that checkout.

- Build: `scripts/cargo.sh build` (the first build compiles libghostty-vt with Zig, about a minute)
- Run: `scripts/cargo.sh run` (opens a window running `$SHELL`). On macOS, `TAI_APPLE_GFX=metal` selects Metal instead of OpenGL.
- Lint: `scripts/cargo.sh fmt --check` and `scripts/cargo.sh clippy`
- Test: `scripts/cargo.sh test`, or `scripts/cargo.sh test <name>` for a single test

Three pins move together: the `libghostty-vt` version in `Cargo.toml`, `GHOSTTY_COMMIT` in `scripts/fetch-ghostty.sh` (it must equal the crate's `crates/libghostty-vt-sys/build.rs` pin), and `ZIG_VERSION` in `scripts/fetch-zig.sh` (it must match that Ghostty commit's `build.zig.zon` `minimum_zig_version`).

## Stack

[ADR 0001](docs/decisions/0001-stack-rust-libghostty-vt-ppxai-server.md) records the decision. In short:

- **Rust**, using **libghostty-vt** through the crates.io `libghostty-vt` crate. Pin the Ghostty commit with `GHOSTTY_SOURCE_DIR` (see Build). Full libghostty (`ghostty.h`) supports Apple platforms only, and Ghostty says it's not for outside embedders.
- libghostty-vt has no renderer and no PTY. tai draws the screen and spawns the child process itself.
- **Milestone (a):** a PTY running `$SHELL` or ppxai's TUI, rendered with macroquad (from `ghostling_rs`, with audio off). **Milestone (b):** a native chat UI built on winit + wgpu.
- **Engine:** tai launches `ppxai-server --uds --announce` as its own child process. Don't run the engine in-process, which would force a Python host. Don't use stdio JSON-RPC either: it has no interrupt or consent handling.
- All of ppxai's wire format lives in **one Rust client module**, covering one-shot (`/v1/oneshot`, stable, no streaming), chat (`/chat` SSE + `/interrupt` + `/consent`, internal) and tasks (`/v1/agent/*` SSE, not yet sealed). Pin the ppxai version and keep a tai-side contract test for the unstable routes.
- Ghostty's macOS build stamps a minimum of macOS 13.0. Check how herdr gets around that before pinning Ghostty.

Record new architecture decisions in `docs/decisions/`, following its README.

## Sister Projects

tai is built from pieces of two sibling checkouts, taken as needed:

- **`../ppxai`** is the source of the engine. Read its `CLAUDE.md` before using the engine. Its key facts change from release to release, so check them there rather than copying them here.
  - The in-process entry point is `from ppxai.engine import EngineClient`. Since ADR 0016, `from ppxai import EngineClient` no longer works.
  - ppxai pins the import surface that outside consumers rely on in `tests/test_consumer_import_surface.py`. If tai imports new engine modules, that test may need updating in ppxai.
  - The engine also has an HTTP/SSE server mode (`ppxai-server`), which is the one tai uses (ADR 0001). ppxai's `docs/api-gateway.md` is the authority on which routes are stable. The import-surface test above doesn't cover the HTTP routes. tai's own contract test does.
- **`../ppxai-sre`** is the source of the agentic code. Look in `agents/`, `libs/` and `mcp-servers/`. It already embeds the engine in-process, so use it as the reference for doing that. Read its `AGENTS.md`, which `CLAUDE.md` links to, before reusing code from it.

When you bring code over from either project, record where it came from: the repo, the path, and the commit **subject** as well as the sha. ppxai rebases its long-lived branches, so its shas don't stay valid.
- **ppxai runtime data:** `~/.ppxai/` holds sessions, logs, runs, checkpoints and similar data, shared with the other ppxai clients.

## Coding Discipline

These rules come from `../ppxai/CLAUDE.md` and the owner's global `~/.claude/CLAUDE.md`. The global rules on model tiering for subagents and on `rtk` still apply here.

- **Verify, don't assume.** Before you call an anomaly "pre-existing", "unrelated" or "expected", run the check that proves it: `git stash` and rerun, `git blame`, parse just the section you edited, or run `<binary> --version` after a build. If you can't verify, say so: "I'm assuming X because Y, but haven't confirmed." The same standard of evidence applies both to "this is broken" and to "no, it's fine".
- **Make sure an empty result really ran.** A check that never ran prints the same nothing as a check that found nothing. Don't add `2>/dev/null` to a verification command. Print the exit status when "found nothing" is the finding. `grep` exits 1 for no match and 2 for an error.
- **Don't expand scope.** Do what was asked. If you spot something else worth doing, mention it and wait for a yes.
- **Read the source, not summaries.** Commit subjects, ADR titles and subagent reports point you somewhere. They don't prove anything. Open the files that matter, especially when a change in ppxai might affect tai.
- **No release actions without explicit permission each time.** That covers tagging, creating releases, force-pushing, rewriting pushed history and merging to the default branch.
- **Python code taken from ppxai keeps ppxai's rules:**
  - Python 3.10+ with type hints, dataclasses, `async` for I/O, and pytest.
  - No function-level internal imports and no `if TYPE_CHECKING:`. Break import cycles with a `Protocol` in a leaf module instead (see ppxai's `docs/patterns/protocol-dependency-inversion.md`).
- **Write down hazards in the repo.** When you find a hazard that's true on any machine, can be confirmed with grep, and changes what someone does, propose recording it in the repo, for example under `docs/lessons/`. Don't commit it without asking. Per-host AI memory doesn't sync between machines.

## Commit Guidelines

- Don't add Claude credits, `Co-Authored-By` lines, `Claude-Session:` lines or "Generated with Claude Code" lines to commits or PRs. This is ppxai's rule, applied here too.
- Never commit secrets, hostnames, usernames, file paths containing usernames or environment-specific details. In docs and PRs, use placeholders such as `/path/to/project`.

## Repo Conventions

- `.local/` is gitignored scratch space for local-only files.
