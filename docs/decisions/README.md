# Architecture Decision Records (ADRs)

## Index

Status is summarised; the record itself is authoritative.

| # | Decision | Status |
|---|---|---|
| [0001](0001-stack-rust-libghostty-vt-ppxai-server.md) | Rust + libghostty-vt for the UI, ppxai-server as a child process for the engine | ✅ Accepted 2026-10-24, not yet implemented |
| [0002](0002-windows-11-support.md) | Windows 11 support: MSVC, ConPTY via portable-pty, loopback TCP to the engine | ✅ Accepted 2026-10-24; engine link waits for milestone (b) |
| [0003](0003-utf-8-everywhere.md) | UTF-8 everywhere, with the locale sensed per OS | ✅ Accepted 2026-10-24 |
| [0004](0004-windows-system-conpty.md) | Use Windows' own ConPTY, not whatever `conpty.dll` the DLL search finds | ✅ Accepted 2026-10-25 |

Numbering is sequential. The next record is `0005`.

## About these records

These records follow ppxai's convention (`../ppxai/docs/decisions/README.md`).
Each one is short and dated, and covers a decision where the right choice
isn't obvious or where we deliberately put off the proper solution. Each
names:

- **Context:** the problem and its constraints, with the sources read
  (repo, path, commit subject and sha)
- **Decision:** what we chose
- **Why this and not the alternatives:** the trade-off we accepted
- **Triggers to revisit:** concrete signals that should prompt a re-evaluation

File names follow `NNNN-short-slug.md`, lowercase kebab-case. Never
renumber or reuse a number.

Once a record is Accepted, it doesn't change. To change a decision, write a
new record that supersedes the old one, and add a "Superseded by:" line to
the old one. The one exception is fixing facts to match what actually
shipped without changing the decision, such as a path, a version label or
a ticked checklist. Mark that kind of edit with a dated note.
