# Lessons from other libghostty projects

Other apps built on libghostty, from
[awesome-libghostty](https://github.com/Uzaaft/awesome-libghostty), have
already hit problems that tai has open. This page records the ones tai
can use, with where they come from and what state tai is in. Surveyed
2026-11-01. Each source was read at the commit given; the postmortems are
the projects' own write-ups, so check their code before porting.

Licences: con-terminal, wispterm and godotty are MIT, so code may be ported with
attribution, recorded as for Ghostty ports (repo, path, commit). mightty
has no licence: ideas only.

## 1. ConPTY can't take scrollback back on resize

**Source:** con-terminal (`nowledge-co/con-terminal` at `ce47be2061`),
`postmortem/2026-09-20-conpty-resize-scrollback.md`; Ghostty PR #14296
("terminal: add option to disable scrollback pull on resize", merged
2026-09-18 as `b32f20f3`).

ConPTY keeps its own active screen and can't pull the terminal's
scrollback back into it. Ghostty's default resize does pull scrollback in
when rows grow or columns reflow, so after such a resize the two active
screens disagree. con-terminal turns the option off for Windows only. They
found it by audit, not from a reproduced failure.

**In tai:** tai's pin (a887df42) predates the option, and libghostty-vt
0.2.2 doesn't expose it; libghostty-rs master has
`Terminal::set_resize_pull_scrollback`. tai's own resize garbling went
away with the bundled ConPTY (`windows-pitfalls.md`, "tai may run another
app's ConPTY"), so this is a remaining risk, not a known bug. Taking it
means moving the three pins together (CLAUDE.md, Build). When you do,
turn the option off on Windows only, and test row growth and column
reflow separately.

## 2. IME: composed text and raw keys take separate paths

**Source:** con-terminal `postmortem/2026-05-01-windows-linux-cjk-ime.md`
and `postmortem/2026-05-13-ime-commit-no-repaint.md`; wispterm
(`xuzhougeng/wispterm`, MIT, at `196ac8e574`) `src/ime_caret.zig`.

Rules for when tai-gui wires winit's IME events (ADR 0012, decision 7, and
its trigger):

- Raw key events only drive the key encoder. Printable text and IME
  commits come through the text-input path, and are sent to the pty as
  text.
- Don't filter ASCII out of commits: a Chinese IME in English mode commits
  real ASCII.
- While a composition is open, keys go to the IME, not the pty.
- Keep the marked-text range, draw the preedit at the cursor, and report
  the cursor cell's rectangle so the candidate window opens there.
- Request a repaint after every commit and preedit change. Missing that
  made committed text invisible until the next unrelated repaint.
- Send to the pty only while the terminal pane has focus.
- Move the IME caret only once the terminal cursor has stayed on one cell
  for two frames (wispterm). TUIs that redraw a status line, such as Claude
  Code or Codex, jump the cursor around every frame.

**In tai:** no IME handling yet; typing CJK through an IME doesn't work in
either binary.

## 3. Grapheme clusters: key glyphs by the whole cluster and its width

**Source:** con-terminal
`postmortem/2026-09-22-portable-grapheme-rendering.md`.

Their bug was tai's gap: only the first scalar of each cell was drawn.
What fixed it:

- Read the cell's full grapheme from the render iterator; keep a no-alloc
  path for single-scalar cells.
- Key the glyph atlas by the cluster's text and its cell width, not by
  one `char`.
- Shape each non-ASCII cell on its own, and never let shaping change how
  many columns the terminal gave it. Shaping a whole row merged separate
  emoji cells when mode 2027 was off.
- Grow or repack the atlas before the frame's instances are built, not
  partway through.
- Tests: accents, ZWJ sequences, variation selectors, mode 2027 on and
  off, and a tiny atlas filled with many distinct clusters.

**In tai:** `Coverage::cluster_face` picks one font for the whole
cluster, as Ghostty's `RunIterator.indexForCell` does, and both binaries'
row walks use it. tai doesn't shape, so it draws the first codepoint and
the zero-advance marks after it, and stops at anything that would
advance (the rest of a ZWJ sequence, a skin tone, a flag's second
letter). A cluster can't draw past its cells, so `GlyphAtlas` keeps
per-glyph keys, `(Face, char, px)`: the cluster's cell width is already
in `px`, through `Coverage::place`. Mode 2027 stays off by default
(`libghostty-vt-pitfalls.md`).

## 4. Font fallback: cache per font, ask loaded fonts first

**Source:** wispterm `src/font/fallback_cache.zig`, after Ghostty's
`Collection.zig` and `CodepointResolver.zig`. Opening a face per new
codepoint kept 800 copies of PingFang open, about 1.2 GB.
con-terminal `postmortem/2026-07-22-windows-cjk-font-fallback.md`: pass
`GetUserDefaultLocaleName` to DirectWrite, not a fixed `en-US`, or Han gets
the wrong regional forms; and DirectWrite's sizing calls that return
`E_NOT_SUFFICIENT_BUFFER` are working as designed.

**In tai:** done in ADR 0012's implementation (`src/fonts.rs`,
`Coverage::discover`; `src/discovery.rs`): faces already found are asked
first, each font file is loaded once, every answer is cached, and the
user's locale reaches DirectWrite, CoreText and fontconfig. Keep it that
way when changing either file.

## 5. Colour emoji: choose by the cell's width

**Source:** godotty (`ingur/godotty` at `bad2d0a971`), `src/font.rs`,
`compute_fallback`.

A wide cell tries the colour emoji font first, then monochrome; a narrow
cell tries monochrome, then a symbol font, then colour. That follows
Ghostty, and the width comes from libghostty-vt. godotty also draws box
drawing, braille and powerline glyphs itself (`src/sprite.rs`), as
Ghostty's sprite font does.

**In tai:** monochrome Noto Emoji only (`fonts/README.md`). Colour emoji
needs a bitmap path (sbix/CBDT) or COLR, which zeno can't fill.

## 6. Capture cell data alongside screenshots

**Source:** mightty (`frixaco/mightty` at `bcfccfac70`; no licence),
`docs/windows-text-rendering-notes.md`.

Each capture writes the rows' text, cells and colours as JSON next to the
PNG, so a wrong screenshot can be split into a VT bug (the cells are
wrong) or a rendering bug (the cells are right). tai's pixel comparisons
(ADR 0008's checks, ADR 0012's step 1) compare PNGs only.

## References

Thanks to the authors of these projects for writing up what they found.
Links are pinned to the commits the lessons were read at.

- awesome-libghostty, the list surveyed:
  https://github.com/Uzaaft/awesome-libghostty/tree/b09654cfc5435f554e1f4213767b67801bf397ce
- Ghostty PR #14296, "terminal: add option to disable scrollback pull on
  resize": https://github.com/ghostty-org/ghostty/pull/14296
- libghostty-rs, `Terminal::set_resize_pull_scrollback`:
  https://github.com/Uzaaft/libghostty-rs/blob/575ed6b8ceb79ffaf247ecf7e34333876b0d5be4/crates/libghostty-vt/src/terminal.rs
- con-terminal (MIT), https://github.com/nowledge-co/con-terminal, postmortems at
  `ce47be206176332ab28b726b48a94bfc376f2caf`:
  - https://github.com/nowledge-co/con-terminal/blob/ce47be206176332ab28b726b48a94bfc376f2caf/postmortem/2026-09-20-conpty-resize-scrollback.md
  - https://github.com/nowledge-co/con-terminal/blob/ce47be206176332ab28b726b48a94bfc376f2caf/postmortem/2026-05-01-windows-linux-cjk-ime.md
  - https://github.com/nowledge-co/con-terminal/blob/ce47be206176332ab28b726b48a94bfc376f2caf/postmortem/2026-05-13-ime-commit-no-repaint.md
  - https://github.com/nowledge-co/con-terminal/blob/ce47be206176332ab28b726b48a94bfc376f2caf/postmortem/2026-09-22-portable-grapheme-rendering.md
  - https://github.com/nowledge-co/con-terminal/blob/ce47be206176332ab28b726b48a94bfc376f2caf/postmortem/2026-07-22-windows-cjk-font-fallback.md
- wispterm (MIT), https://github.com/xuzhougeng/wispterm, at
  `196ac8e5749e68999bc07b33c57fd1aa2ac7f40b`:
  - https://github.com/xuzhougeng/wispterm/blob/196ac8e5749e68999bc07b33c57fd1aa2ac7f40b/src/ime_caret.zig
  - https://github.com/xuzhougeng/wispterm/blob/196ac8e5749e68999bc07b33c57fd1aa2ac7f40b/src/font/fallback_cache.zig
- godotty (MIT), https://github.com/ingur/godotty:
  https://github.com/ingur/godotty/blob/bad2d0a9712ba12eb13f6f4b34a64497fdd1498f/src/font.rs
- mightty (no licence; ideas only), https://github.com/frixaco/mightty:
  https://github.com/frixaco/mightty/blob/bcfccfac708c0ca83092a85d8d9599d9e47a89bf/docs/windows-text-rendering-notes.md
