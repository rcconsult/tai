# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**tai** is a terminal app that runs the **ppxai engine harness** behind a UI built on **libghostty** (Ghostty's embeddable terminal library).

**Status:** the project is just starting. The repo has no source code, build system or tests yet. Once the stack and toolchain are chosen, add the build, lint and test commands here, including how to run a single test. Don't guess them.

## Sister Projects

tai is built from pieces of two sibling checkouts, taken as needed:

- **`../ppxai`** is the source of the engine. Read its `CLAUDE.md` before using the engine. Its key facts change from release to release, so check them there rather than copying them here.
  - The in-process entry point is `from ppxai.engine import EngineClient`. Since ADR 0016, `from ppxai import EngineClient` no longer works.
  - ppxai pins the import surface that outside consumers rely on in `tests/test_consumer_import_surface.py`. If tai imports new engine modules, that test may need updating in ppxai.
  - The engine also has an HTTP/SSE server mode (`ppxai-server`). Choose deliberately between in-process and server.
- **`../ppxai-sre`** is the source of the agentic code. Look in `agents/`, `libs/` and `mcp-servers/`. It already embeds the engine in-process, so use it as the reference for doing that. Read its `AGENTS.md`, which `CLAUDE.md` links to, before reusing code from it.

When you bring code over from either project, record where it came from: the repo, the path, and the commit **subject** as well as the sha. ppxai rebases its long-lived branches, so its shas don't stay valid.
- **ppxai runtime data:** `~/.ppxai/` holds sessions, logs, runs, checkpoints and similar data, shared with the other ppxai clients.

## Repo Conventions

- `.local/` is gitignored scratch space for local-only files.
