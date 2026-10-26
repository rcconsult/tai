# ADR 0006: Accept Sixel by translating it into Kitty images

- **Status:** Accepted 2026-10-25 (owner). Not implemented yet; the checks
  listed under decision 3 and Accepted risks are still open.
- **Superseded in part by:** [ADR 0009](0009-sixel-not-advertised.md)
  replaces decision 1 (Sixel is advertised only with `TAI_SIXEL=1`) and
  decision 3 (the cursor follows xterm, not WezTerm).
- **Deciders:** owner. Written by the Claude session on the Windows dev host.
- **Related:**
  - [ADR 0005](0005-kitty-images.md) (Kitty images: identity variables
    and virtual placements). This record adds a second way for images to
    reach the same Kitty path; it doesn't replace either of 0005's
    decisions.
  - [ADR 0007](0007-bundle-conpty.md) (bundled ConPTY). On Windows this
    record depends on it: Windows' own ConPTY swallows Sixel and answers
    DA1 itself.
  - [ADR 0001](0001-stack-rust-libghostty-vt-ppxai-server.md):
    libghostty-vt has no renderer, so tai draws images itself.
  - Future: an image and document preview feature (see the last section).

## Context

**libghostty-vt has no Sixel** (Ghostty a887df42 plus tai's patch):

- Sixel arrives as a DCS sequence, `ESC P <params> q <data> ESC \`.
  Ghostty's DCS handler (`src/terminal/dcs.zig`, `hook`) recognises only
  `+q` (XTGETTCAP) and `$q` (DECRQSS). Any other DCS is dropped with an
  "unknown DCS hook" log line, and there is no callback for it.
- The DA1 reply is the embedder's. tai sets it in `on_device_attributes`
  (`src/main.rs`) and doesn't list feature 4 (Sixel), which the header
  defines as `GHOSTTY_DA_FEATURE_SIXEL`.
- Kitty graphics is fully parsed and stored. tai draws each placement
  scaled into its grid box (`render_kitty_images`: grid columns × rows at
  the current cell size, plus the pixel offsets), so the protocol's
  sizing keys (`c`, `r`, `X`, `Y`, the source rectangle) already decide
  what's on screen.
- libghostty-vt honours Kitty's `C=1` (don't move the cursor) and `q=`
  (quiet), and gives an image sent without `i=` an id of its own, counting
  up from 2147483647, and sends no reply for it
  (`src/terminal/kitty/graphics_exec.zig`, `graphics_storage.zig`).

**On Windows, Sixel reaches tai only through the bundled ConPTY**
([ADR 0007](0007-bundle-conpty.md)). Windows' own ConPTY (Windows 11 build
26300) swallows Sixel DCS and Kitty APC sequences and answers DA1 itself
without feature 4, so neither this record nor tai's DA1 reply would have
any effect there. The bundled ConPTY passes all three through to tai.

**Programs that tai's users run choose Sixel first:**

- textual-image 0.8.5, which ppxai's Textual TUI uses
  (`textual_image/renderable/__init__.py`), checks Sixel before Kitty:
  "only terminal with TGP unicode diacritic support I know of is Kitty".
  Its check is DA1 feature 4 (`renderable/sixel.py`,
  `query_terminal_support`).
- ppxai's Rich renderer sends Sixel when `WT_SESSION` is set
  (`ppxai/rendering/rich_renderer.py`).
- Windows Terminal, xterm, foot, mlterm, WezTerm and Konsole accept Sixel,
  so it's what programs expect on Windows.

**WezTerm does this cheaply** (researched 2026-10-25 at WezTerm main
e2a98e1b, "escape-parser: Fix no_std build and add CI coverage"):

- Sixel, iTerm2 and Kitty end in one image model
  (`term/src/terminalstate/image.rs`).
- A Sixel image is decoded once, when its DCS ends, into an RGBA buffer
  (`term/src/terminalstate/sixel.rs`). Drawing reuses a cached texture, so
  nothing is decoded per frame.
- It bounds input: a raster size over 100,000,000 pixels is refused by the
  parser, and an image needing over 100 MB of RGBA is refused. CVE-2022-24130
  was a crash from an unbounded repeat count (wezterm#1610).
- Its Sixel speed was tuned in 2022 to "the same ballpark as kitty"
  (wezterm#217). Its code isn't published as a crate; the
  pixel conversion is about 100 lines.

**Decoder crates** (checked 2026-10-25):

- `icy_sixel` 0.7.0 (2026-09-06, MIT OR Apache-2.0, about 1.5M downloads)
  is the fit:
  - `sixel_decode` returns RGBA pixels with width and height.
  - A streaming `SixelDcsStreamDecoder` is fed as bytes arrive.
  - It caps images at 64 Mi pixels and has fuzz targets.
  - It uses `unsafe` for speed and depends on an encoder crate
    (`quantette`) that tai doesn't need.
- Zellij's `sixel-image` 0.2.1 (MIT) has no `unsafe`, but it yields
  per-row vectors of palette pixels, not RGBA, and is little maintained.

## Decision

1. **Advertise Sixel.** Add feature 4 to tai's DA1 reply.
2. **Translate Sixel into a Kitty image.** tai scans the PTY output before
   `vt_write` for Sixel DCS sequences (`ESC P … q`, or 8-bit `0x90 … q`)
   that aren't inside another string. The scan state survives read
   boundaries, and CAN or SUB aborts a sequence as it does in libghostty-vt's
   parser. For each one:
   - Decode it with `icy_sixel` into an RGBA buffer, refusing images over
     tai's size limit (see 4). Keep the alpha channel: with `P2=1`, pixels
     the image doesn't paint are transparent, not background colour.
   - Pass on, in its place, a Kitty transmit-and-display command: `a=T`,
     `f=32`, `q=2`, `s`/`v` set to the decoded size, and base64 payload in
     chunks of 4096 bytes or less with `m=1` on all but the last.
   - Send no `i=`. libghostty-vt then picks its own id, from a range
     programs don't use, so the image can't replace one of the program's
     own Kitty images. Without `i=` and with `q=2`, nothing is written back
     to the program, which never asked for a Kitty reply.
   - Everything else in the stream passes through unchanged. Kitty
     sequences go straight to libghostty-vt as today.
3. **Keep Sixel's cursor semantics, as WezTerm does.** WezTerm leaves the
   cursor on the image's last cell row, in its starting column. Between rows
   it issues a line feed that keeps the column, so text scrolls with the
   image, and it moves past the image only with "Sixel scrolls right" set
   (`term/src/terminalstate/image.rs`, `assign_image_to_cells`, WezTerm
   main e2a98e1b). A Kitty image instead moves the cursor past the
   bottom-right corner.
   - The Kitty command carries `C=1` (don't move the cursor). tai then
     passes on one line feed per cell row but the last, keeping the column.
   - Cell rows are `ceil(pixel height / cell height)` at the current cell
     size, as in WezTerm.
   - To check: xterm and the VT340 may leave the cursor on the line below
     the image instead. That hasn't been checked against xterm's source.
     If tai's users' programs assume the xterm behaviour, follow xterm.
   - To check: placement and scrolling for an image that starts near the
     bottom of the screen.
   - DECSDM (Sixel display mode) and "Sixel scrolls right" are out of scope
     until a program needs them.
4. **Bound the cost.**
   - Refuse a Sixel image whose decoded RGBA would exceed 64 MB. That's the
     same as tai's whole Kitty storage limit (`set_kitty_image_storage_limit`,
     `src/main.rs`), which libghostty-vt enforces by evicting old images.
   - Per image, memory peaks at about twice its RGBA size (tai's buffer
     plus libghostty-vt's copy) and settles at one copy. A 1920×1080
     image is about 8 MB.
   - CPU is two passes over the pixels per image (decode, then
     libghostty-vt's base64 decode), and none per frame.
5. **Size images natively.** The Kitty command carries no `c`/`r`, so the
   image keeps its pixel size, as a Sixel image does in other terminals.
   Fitting an image to an area is the sender's choice and is already
   possible: textual-image and ppxai size their Sixel output from the cell
   size (`CSI 16 t`), which libghostty-vt answers.

## The chain of image decisions

| Path in | Handled by | Ends as |
|---|---|---|
| Kitty, direct placement | libghostty-vt (today) | a Kitty placement tai draws |
| Kitty, virtual placement (Unicode placeholders) | ADR 0005, decision 2 | the same, resolved by tai's renderer |
| Sixel | this record | a direct Kitty placement |
| iTerm2 OSC 1337 inline images | not handled; a later record could translate them the same way (Kitty accepts PNG directly, `f=100`) | — |

ADR 0005's decision 1, setting `TERM_PROGRAM=tai` and removing inherited
identity variables, still matters for every row. It's what makes programs
query tai instead of assuming another terminal's protocol.

## Why this and not the alternatives

- **Draw Sixel in tai's renderer, beside libghostty-vt:** tai would have to
  track where the image sits as text scrolls, reflows and clears, which
  libghostty-vt already does for Kitty placements.
- **Patch Ghostty to parse Sixel:** a large patch on the pin (see
  `patches/ghostty/`), and Ghostty upstream doesn't have Sixel to backport.
- **Patch Ghostty to hand unknown DCS sequences to the embedder:** smaller
  than a Sixel parser, and it would reuse libghostty-vt's string tracking
  (OSC, APC and DCS, 8-bit C1, CAN and SUB) instead of a second scanner.
  `dcs.zig`'s `hook` already sees every unknown DCS before dropping it. But
  it needs a new callback in libghostty-vt's C API, then in the
  `libghostty-vt-sys` and `libghostty-vt` crates, which tai takes from
  crates.io. That's three patched layers to carry until upstream accepts
  such a hook. Revisit if the scanner proves fragile, or upstream adds
  the hook.
- **Port WezTerm's decoder instead of using `icy_sixel`:** about 100 lines,
  without `unsafe`, but the tokenizer would have to be ported too, along
  with the bounds that CVE-2022-24130 showed are needed. Revisit if
  `icy_sixel` causes trouble.
- **Leave Sixel out and rely on ADR 0005 alone:** textual-image would keep
  choosing Kitty placeholders only when Sixel is absent, but programs that
  pick Sixel by environment (`WT_SESSION`) or support only Sixel would show
  nothing.

## Accepted risks

- The stream scanner is new parsing code in front of libghostty-vt. A bug
  could swallow or corrupt output. It needs unit tests with:
  - sequences split across read boundaries;
  - `ESC P` inside OSC or APC strings;
  - 8-bit C1 DCS (`0x90`) and ST (`0x9C`);
  - sequences aborted by CAN or SUB.
- `icy_sixel` brings `unsafe` and an unused encoder dependency.
- Translated images count against the same 64 MB Kitty storage, so heavy
  Sixel output (video players) evicts older images sooner. One image at
  the per-image limit fills the whole store and evicts every other image.
- Advertising Sixel invites XTSMGRAPHICS queries (`CSI ? 1 ; 1 ; 0 S`
  for colour registers, `CSI ? 2 ; 1 ; 0 S` for geometry), which
  libghostty-vt doesn't answer. textual-image and Rich don't send them
  (checked in their installed sources). Other programs, such as chafa,
  haven't been checked; a program that waits for the answer without a
  timeout would hang.
- On Windows, ConPTY keeps its own copy of the screen. If it doesn't move
  its cursor for a Sixel image the way tai does, the two disagree about
  where the cursor is. That's the same kind of drift as the resize garbling
  in ADR 0004. To check: print text after an image, then resize the window
  so ConPTY repaints.

## Triggers to revisit

- Ghostty or libghostty-vt gains Sixel, or a DCS callback for the embedder.
- A program needs DECSDM, "Sixel scrolls right", or XTSMGRAPHICS answers.
- A program misplaces text after a Sixel image in tai: check the cursor
  rule in decision 3 against xterm.
- tai adds iTerm2 inline images: extend the translator rather than adding a
  second path.

## Future: preview and fit-to-pane

Showing where a user is (an image, a PDF page, a slide, a diagram) needs
more than a terminal image: fitting and centring in an area, re-fitting on
resize or font change, zoom. At the terminal level the Kitty protocol
already carries the inputs: a box in cells (`c`/`r`), offsets (`X`/`Y`)
and a source rectangle. tai honours them, so a sender that knows the cell
and pane size can fit and centre exactly. Turning documents into page
images is the harness's job. A tai-native preview pane that keeps an image
fitted as the window changes belongs to milestone (b)'s native UI or a tai
side panel, and needs its own record, written from the owner's use cases.
