# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**tai** is a terminal app that runs the **ppxai engine harness** behind a UI built on **libghostty** (Ghostty's embeddable terminal library).

**Status:** the project is just starting. The repo has no source code, build system or tests yet. Once the stack and toolchain are chosen, add the build, lint and test commands here, including how to run a single test. Don't guess them.

## Related Code

- **ppxai engine:** `../ppxai` (sibling checkout). Read its `CLAUDE.md` before using the engine. Its key facts change from release to release, so check them there rather than copying them here.
  - The in-process entry point is `from ppxai.engine import EngineClient`. Since ADR 0016, `from ppxai import EngineClient` no longer works.
  - `ppxai-sre` (`../ppxai-sre`) is an existing out-of-repo consumer that uses the engine in-process. Use it as a reference for embedding. ppxai pins the import surface its consumers rely on in `tests/test_consumer_import_surface.py`. If tai adds new engine imports, that test may need updating in ppxai.
  - The engine also has an HTTP/SSE server mode (`ppxai-server`). Choose deliberately between in-process and server.
- **ppxai runtime data:** `~/.ppxai/` holds sessions, logs, runs, checkpoints and similar data, shared with the other ppxai clients.

## Repo Conventions

- `.local/` is gitignored scratch space for local-only files.
