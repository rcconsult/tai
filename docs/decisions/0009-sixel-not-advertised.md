# ADR 0009: Translate Sixel, but don't advertise it by default

- **Status:** Accepted 2026-10-26 (owner).
- **Deciders:** owner. Written by the Claude session on the Windows dev host.
- **Supersedes:** decision 1 of [ADR 0006](0006-sixel-to-kitty.md)
  ("Advertise Sixel"). ADR 0006's other decisions stand.
- **Related:** [ADR 0005](0005-kitty-images.md) (Kitty virtual
  placements), [ADR 0007](0007-bundle-conpty.md) (bundled ConPTY).

## Context

ADR 0006 decision 1 adds feature 4 (Sixel) to tai's DA1 reply. Since
ADR 0005, programs that ask tai what it supports get Kitty Unicode
placeholders, and ppxai's TUI (ppxaide) draws its image panel with them on
Windows, Linux and macOS.

textual-image 0.8.5, which ppxaide uses, checks Sixel before Kitty
(`textual_image/renderable/__init__.py`): if DA1 lists feature 4, it picks
Sixel. Advertising Sixel would therefore move ppxaide off the placeholders
that work today and onto ADR 0006's translator, which turns each Sixel image
into a fixed Kitty placement. Placeholders are cells: Textual moves, clips
and redraws them like text. A fixed placement is not, and textual-image's
Sixel widget compensates with its own redraw logic, which hasn't been tried
against the translator.

Programs that send Sixel without asking still need the translator: ppxai's
Rich renderer sends Sixel when it sees `WT_SESSION`, and some tools only
speak Sixel.

## Decision

1. tai translates every Sixel image it receives (ADR 0006, decisions 2 to
   5), whether or not it advertised Sixel.
2. tai's DA1 reply lists feature 4 only when `TAI_SIXEL=1` is set in its
   environment. By default, programs that ask see no Sixel and choose Kitty.

A fact correction to ADR 0006 decision 2, which mentions 8-bit DCS
(`0x90`): libghostty-vt decodes the stream as UTF-8 and prints any
codepoint at or above 0x80 (`src/terminal/stream.zig`, `handleCodepoint`,
a887df42), so it never starts a DCS from a raw `0x90`. The translator does
the same and only recognises `ESC P`. Inside a string, it still treats
`0x9C` as a terminator, as libghostty-vt's parser does.

## Why this and not the alternatives

- **Advertise Sixel, as ADR 0006 said:** ppxaide's images would change path
  on every platform at once, from one that works to one that hasn't been
  tested.
- **Don't translate Sixel at all until it's advertised:** programs that send
  Sixel unasked would keep showing nothing.

## Triggers to revisit

- textual-image's Sixel path is tested under the translator and works as
  well as placeholders: advertising can become the default.
- textual-image starts preferring Kitty over Sixel: the question goes away.
- A program needs DA1 feature 4 before it sends Sixel and users want it
  without setting `TAI_SIXEL`.
