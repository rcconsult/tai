# ADR 0014: One terminal core for tai and tai-gui, under `src/term/`

- **Status:** Accepted 2026-11-27 (owner). Step 1 is under way on branch
  `term-core-setup`.
- **Deciders:** owner. Written by the Claude session on the Windows dev
  host, from the owner's question that day: "what else can we refactor
  to minimize divergence and code duplication?"
- **Related:** [ADR 0008](0008-gpu-cell-renderer.md) and
  [ADR 0012](0012-cjk-font-fallback.md) (tai draws with OpenGL through
  macroquad), [ADR 0011](0011-native-gui-egui.md) (tai-gui draws with wgpu
  through egui), [ADR 0005](0005-kitty-images.md),
  [ADR 0006](0006-sixel-to-kitty.md) and
  [ADR 0009](0009-sixel-not-advertised.md) (images).

## Context

tai has two front ends over the same libghostty-vt terminal: `tai`
(`src/main.rs`, macroquad on OpenGL) and `tai-gui`
(`src/bin/tai-gui/terminal.rs`, egui on wgpu). tai-gui shares some of
tai's modules through `#[path]`: `cells.rs`, `fonts.rs`, `shortcuts.rs`,
`selection.rs`, `identity.rs`, `locale.rs`, `grapheme.rs`,
`shell_integration.rs` and the pty modules. Everything between the
terminal and the screen was written twice, once per front end.

On 2026-11-27, main at `98edab0`, the two copies were:

- **Diverged.**
  - Only tai answers device attributes (DA1/DA2/DA3, with Sixel listed
    only under `TAI_SIXEL=1`, ADR 0009) and the colour-scheme query
    (CSI ? 996 n), and sends the mode 2048 in-band size report when a
    program sets the mode.
  - Only tai-gui reads prompt marks (`on_semantic_prompt`, ADR 0013).
  - Only tai has Kitty images and Sixel.
  - So a program sees two slightly different terminals.
- **Duplicated, the same today but kept twice.**
  - Mouse reports to programs, with the Shift rules and clearing the
    selection on a report.
  - Selection: press, drag, release, autoscroll ticks, Shift-click
    extend, rectangle modifiers.
  - The wheel: rows per notch, fraction carry, alternate scroll.
  - Constants: `PADDING`, `CLICK_INTERVAL`, `AUTOSCROLL_EVERY`,
    `WHEEL_ROWS`.
  - Turning a row into drawable cells (`walk_row` in tai, `build` in
    tai-gui).
  - Key encoding and `mods_from`.

Every fix in this area so far (the wheel, autoscroll, Shift-click,
clearing the selection) was made in both files and tested in both apps on
three platforms.

## Decision

1. **A shared terminal core in `src/term/`.** It holds everything that
   decides what the terminal does and what to draw. Both binaries build
   it: tai as `mod term`, tai-gui through `#[path = "../../term/mod.rs"]`.
2. **The boundary.** Each front end keeps only what its toolkit forces:
   - the window and event loop;
   - translating its toolkit's input events into neutral ones;
   - the GPU calls (OpenGL in tai, wgpu in tai-gui).

   The core takes neutral input and returns bytes for the pty, selection
   changes, and backend-neutral draw lists.
3. **In steps, one branch each.** Each step leaves both apps behaving as
   before, except where it makes the two converge, which its branch says.
   Each is tested on all three platforms and merged with the owner's go.
   1. **Setup and replies** (`term/setup.rs`):
      - creating a terminal with its options;
      - the replies: pty write, size, device attributes, XTVERSION,
        colour scheme;
      - the mode 2048 in-band size report.

      tai-gui gains the replies it lacked.
   2. **Images** (`term/images.rs`):
      - Kitty graphics setup and the PNG decoder;
      - the Sixel translator;
      - placements in drawing order, and Unicode placeholder runs;
      - a draw list of image id, generation, source and destination
        rectangles, and layer.

      Each front end keeps a texture cache and one textured-quad draw.
      tai-gui gains images.
   3. **Mouse, selection and wheel** (`term/pointer.rs`): one state
      machine fed with neutral button, position, modifier and wheel
      events.
   4. **Row to cells:** one walker producing backgrounds, glyph runs,
      fallback icons and placeholder cells. Each renderer turns them into
      its own instances.
   5. **Keys:** one encoder path fed with a neutral key event.
4. **Not in this decision:** drawing `tai` through tai-gui's wgpu
   renderer, so there is one renderer. That would change ADR 0008 and
   0012's choice of OpenGL for tai and needs its own record.

## Why this and not the alternatives

- **Keep two copies.** Every fix lands twice, and the copies have
  already drifted in what programs see. That's the cost this record
  stops.
- **One renderer first.** It would remove the most code, but it moves
  tai off macroquad, a bigger change with its own risks (window, input
  and clipboard quirks recorded in `docs/lessons/`). The core is needed
  either way, so it comes first; a single renderer can follow on top of
  it.
- **A library crate instead of `src/term/`.** It's cleaner on paper, but
  tai already shares through `#[path]`, and a workspace split can follow
  once the core has settled.

## Triggers to revisit

- A step that can't keep both apps' behaviour without app-specific
  branches inside the core: the boundary is wrong for that area.
- tai is retired, or moved onto wgpu: the core stays, but the per-app
  layer shrinks to one.
- libghostty-rs grows its own render or input helpers that cover a step.
