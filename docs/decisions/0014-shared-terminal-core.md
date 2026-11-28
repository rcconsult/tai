# ADR 0014: One terminal core for tai and tai-gui, under `src/term/`

- **Status:** Accepted 2026-11-27 (owner). Steps 1 to 4 are merged
  (main `20f3ec6`, `fbc60ea`, `268ac44`). Step 5 is on branch
  `term-core-keys`.
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
    (CSI ? 996 n).
  - tai sends its own mode 2048 size report when a program sets the
    mode. libghostty-vt already sends one itself at this pin (Ghostty
    `src/terminal/stream_terminal.zig`, `reportMode2048`, at 108cacb1),
    so programs get it twice in tai and once in tai-gui (found by the
    macOS and Linux sessions while testing step 1).
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
        colour scheme.

      tai-gui gains the replies it lacked. tai drops its own mode 2048
      report, leaving libghostty-vt's, so both send it once.
   2. **Images** (`term/images.rs`):
      - Kitty graphics setup and the PNG decoder;
      - the Sixel translator;
      - placements in drawing order, and Unicode placeholder runs;
      - a draw list of image id, generation, source and destination
        rectangles, and layer.

      The texture cache's logic is shared too (`TextureCache<T>`, over
      each front end's texture type); each front end keeps only its
      textured-quad draw. tai-gui gains images. One `image` crate decodes
      PNG for both, so the vendored macroquad moves to the version eframe
      uses (`vendor/macroquad/TAI-PATCHES.md`, patch 5).
   3. **Mouse, selection and wheel** (`term/pointer.rs`): one state
      machine fed with neutral button, position, modifier and wheel
      events.

      The two front ends had drifted in eight places; the core takes one
      rule for each: a release goes to the program only after its press
      did; presses off the pane aren't the program's; the wheel to a
      tracking program clears a Shift-selection and reports the pointer,
      one press per row even within a cell; alternate scroll clears the
      selection; no motion reaches the program while Shift is held; the
      pane's last pixel row autoscrolls down as its first does up; and the
      pointer shows a crosshair for a rectangle and an arrow off the grid.
      A drag released in the frame it last moved now ends where it was
      released.
   4. **Row to cells:** one walker producing backgrounds, glyph runs,
      fallback icons and placeholder cells. Each renderer turns them into
      its own instances.

      `term/rows.rs` reads a row into each cell's background, its glyphs
      with their faces, the placeholder runs and the padding's colours;
      `push_glyphs` and `padding_rects` are shared too. Where the two
      renderers differed, both now do what tai-gui and Ghostty did: the
      cursor is drawn into its cell, the character in the background
      colour on the cursor's (tai covered it with a block), and the
      padding takes the edge cells' colours (window-padding-color =
      extend), which tai didn't do. `extend` is Ghostty's opt-in mode, not
      its default (`background`); the owner chose to keep it (2026-11-28).
   5. **Keys:** one encoder path fed with a neutral key event.

      `term/keys.rs` takes a `KeyIn` (the libghostty key, its unshifted
      character, the modifiers and the text the platform typed for it)
      and writes what the program gets. tai's rules are the core's:
      AltGr's character on Windows, a Ctrl chord's own character dropped,
      Shift consumed for a printable key, the text when the encoder has
      no sequence, and the C0 byte for Ctrl+[ and its kind. tai-gui now
      pairs each key with the text egui sends just after it, as tai pairs
      macroquad's key and character events, so typed text goes through
      the encoder too: the Kitty keyboard protocol sees printable keys,
      their repeats and releases in both apps, and Alt or Option with a
      key sends what tai sends. A release goes out only after its press
      did, as a button's does, and tai-gui applies tai's shortcut table,
      so an app shortcut's key (Cmd+C, Cmd+=) never reaches the program.

      **Open item, the numeric keypad (2026-11-28, owner).** Both apps
      send the keypad as the main keys: its digits, `.`, `/`, `-`, `=`
      and Enter as Digit0-9, Period, Slash, Minus, Equal and Enter, and
      `+` and `*` as text. That is what tai-gui can do: egui has no
      keypad keys (egui-winit 0.36.2 `key_from_key_code`), and tai
      follows it. Ghostty sends the keypad's own keys (Numpad0...,
      NumpadEnter), which the Kitty keyboard protocol reports apart from
      the main keys. Do it Ghostty's way, in both apps, once all
      platforms are better tested; tai-gui will need the physical key
      from winit for it. Not a missing feature while the keypad and the
      main keys behave the same and both work. It becomes a functional
      defect if a program needs the keypad keys themselves, such as the
      keypad without Num Lock used to steer a UI.
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
