# ADR 0005: Kitty images, from detection to drawing

- **Status:** Proposed 2026-10-25.
- **Deciders:** owner. Written by the Claude session on the macOS dev host.
  The Linux and Windows sessions reviewed it, and their findings widened
  decision 1.
- **Related:** [ADR 0001](0001-stack-rust-libghostty-vt-ppxai-server.md)
  (libghostty-vt has no renderer, so tai draws the screen);
  [ADR 0006](0006-sixel-to-kitty.md) (Sixel translated into Kitty images);
  [ADR 0007](0007-bundle-conpty.md) (the bundled ConPTY that lets image
  sequences reach tai on Windows).

## Context

The owner found that ppxai's Rich TUI shows no images in tai, while it does
in Windows Terminal. Investigated on macOS on 2026-10-25, at tai `main`
b5e0347 and Ghostty a887df42 (the pin):

- **libghostty-vt answers correctly.** A probe run as tai's shell got these
  replies, all inside textual-image's 0.1 s timeout:
  - DA1: `ESC[?62;1;6;22c` in 8 ms
  - Kitty graphics query (`a=q`): `ESC_Gi=31;OK ESC\` in 15 ms
  - `CSI 16 t`: `ESC[6;20;6t` in 18 ms
- **Problem 1: tai passes on the parent terminal's identity.** The shell
  inherited `TERM_PROGRAM=iTerm.app` from the terminal tai was started from.
  ppxai (`ppxai/tui/widgets/image_handlers.py`,
  `get_image_widget_class`, and `ppxai/tui/terminal.py`, at f75d751f "docs:
  staleness sweep across the Markdown docs") trusts `TERM_PROGRAM` first. It
  therefore picked iTerm2's OSC 1337 inline images, which libghostty-vt
  doesn't implement.
  - tai sets only `TERM` and `COLORTERM` (`src/main.rs`, the child `exec`;
    `src/pty_windows.rs`). Everything else passes through, because
    `portable-pty`'s `CommandBuilder` on Windows starts from the whole
    environment too.
  - Ghostty sets `TERM_PROGRAM=ghostty` and `TERM_PROGRAM_VERSION`, and
    removes `VTE_VERSION` (`src/termio/Exec.zig` at a887df42).
  - The leak is wider than `TERM_PROGRAM`. On Linux, a shell in tai
    started from a simulated parent terminal saw the parent's
    `TERM_PROGRAM` and a stale `TERM_PROGRAM_VERSION`. It also saw
    `VTE_VERSION`, `GNOME_TERMINAL_*`, `KITTY_WINDOW_ID`, `KITTY_PID`,
    `TMUX`, `TMUX_PANE`, `WEZTERM_PANE`, `KONSOLE_VERSION` and
    `LC_TERMINAL`.
  - On macOS, a shell in tai started from iTerm2 also saw
    `ITERM_PROFILE`, `ITERM_INJECT_SHELL_INTEGRATION`, `TERM_FEATURES`
    (iTerm2's list of the features it supports) and a `TERMINFO_DIRS`
    pointing into the iTerm2 app.
  - These variables steer detection:
    - `KITTY_WINDOW_ID` and `WT_SESSION` are read by ppxai.
      `WT_SESSION`, set by Windows Terminal, makes it pick Sixel.
    - `TMUX` and `STY` make programs wrap their escapes for tmux or
      screen, which tai then receives raw.
    - `TERM_FEATURES` tells programs which iTerm2 features they can use.
- **Problem 2: tai doesn't draw virtual placements.** With `TERM_PROGRAM`
  out of the way, textual-image 0.8.5's detection inside tai picks Kitty
  graphics (`textual_image.renderable.tgp.Image`). It sends the image with
  `U=1` and fills cells with U+10EEEE placeholders whose diacritics and
  colours pick the image tile.
  - textual-image checks Sixel first (DA1 feature 4) and tries Kitty
    graphics only when that fails (`textual_image/renderable/__init__.py`).
    It picks Kitty in tai only because tai doesn't advertise Sixel. If
    ADR 0006 lands, ppxai's Rich TUI would send Sixel instead, and
    decision 2 would serve other placeholder clients, such as
    `kitty icat --unicode-placeholder`.
  - libghostty-vt reports virtual placements as not visible and gives them
    no position (`include/ghostty/vt/kitty_graphics.h`, render info
    `viewport_visible`; `src/terminal/c/kitty_graphics.zig`).
  - `render_kitty_images` (`src/main.rs`) skips placements that aren't
    visible, so tai would show placeholder glyphs instead of the image.
    That conclusion comes from reading the code; it hasn't been seen on
    screen.
  - The C API exposes only the inputs: `is_virtual` on a placement, and the
    row flag `GHOSTTY_ROW_DATA_KITTY_VIRTUAL_PLACEHOLDER`
    (`include/ghostty/vt/screen.h`). Resolving placeholder cells into image
    tiles is renderer work. Ghostty's app does it in
    `src/terminal/kitty/graphics_unicode.zig` (`placementIterator`,
    `Placement`, and the diacritic table) and in `src/renderer/image.zig`
    (`kittyUpdate`, `prepKittyVirtualPlacement`).
- **Windows Terminal doesn't use Kitty at all.** There, ppxai falls through
  to textual-image's detection, and Windows Terminal advertises Sixel in
  DA1. Ghostty, and so libghostty-vt, has no Sixel.
- **On Windows, no image reaches tai through Windows' own ConPTY.** It
  swallows Kitty graphics sequences and answers DA1 itself. Both decisions
  below depend on the bundled ConPTY of ADR 0007 there.

## Decision

1. **Identify tai to its child.** On every OS, in the Unix `exec` in
   `src/main.rs` and in `src/pty_windows.rs`:
   - Always set `TERM_PROGRAM=tai` and `TERM_PROGRAM_VERSION=<crate
     version>`, overwriting any inherited value. A stale version must never
     sit next to `TERM_PROGRAM=tai`.
   - Remove the identity variables of other terminals and multiplexers:
     - `VTE_VERSION`
     - `TMUX`, `TMUX_PANE`, `STY`
     - `KITTY_WINDOW_ID`, `KITTY_PID`
     - `WT_SESSION`, `WT_PROFILE_ID`
     - `TERM_SESSION_ID`, `TERM_FEATURES`
     - `LC_TERMINAL`, `LC_TERMINAL_VERSION`
     - `TERMINAL_EMULATOR`
     - every variable starting with `ITERM_`, `WEZTERM_`, `KONSOLE_`,
       `GNOME_TERMINAL_` or `ALACRITTY_`

   `TERMINFO_DIRS` stays, even when it points into another terminal's app:
   it only says where to look up `TERM`'s description, and
   `xterm-256color` is in the system terminfo anyway.

   This goes further than Ghostty, which removes only `VTE_VERSION`. tai
   is a new terminal, not a pane of whatever started it, so nothing should
   tell the shell otherwise. Inside tai, `tmux` then starts a normal
   session instead of believing it's nested.

   On Windows, a WSL shell gets variables through `WSLENV`, and Windows
   Terminal adds `WT_SESSION` to it. Once tai removes `WT_SESSION` itself,
   `WSLENV` has nothing to forward. That's expected but not yet verified.
2. **Draw Kitty virtual placements.** Port Ghostty's placeholder resolution
   into tai's renderer:
   - Scan visible rows that carry the virtual-placeholder flag.
   - Decode each run of U+10EEEE cells: the image id from the foreground
     colour and the third diacritic, the placement id from the underline
     colour, and the row and column from the diacritics.
   - Draw the matching tile of the image. Size the image to the
     placement's rows × cols grid at the current cell size, as
     `prepKittyVirtualPlacement` does. The cell size changes with font
     steps, so the visual check includes Cmd/Ctrl+= and +0.

   Record the port's source (path, commit) in the code, as for other
   Ghostty ports. Prefer porting `graphics_unicode.zig`'s logic to Rust
   over patching libghostty-vt's C API, unless upstream adds a C iterator
   for virtual placements first.
3. Order: (1) first, as a small change; (2) on its own branch with a
   visual check on all three OSes.

## Why this and not the alternatives

- **Teach ppxai about tai** (e.g. `TERM_PROGRAM=tai` → Kitty, or always
  probe first): it helps only ppxai. Every other program that trusts
  `TERM_PROGRAM`, including neovim and textual-image users, would still be
  misled by the inherited value.
- **Set `TERM_PROGRAM=ghostty`**: programs would assume features of the full
  Ghostty app that tai lacks, such as its keybinds, shell integration and
  `xterm-ghostty` terminfo.
- **Leave virtual placements undrawn and rely on direct placements**: only
  programs that use ordinary placements would work, such as ppxai's own
  viewer (`ppxai/tui/images.py`, `display_image_kitty`, `a=T`).
  textual-image, and so ppxai's Rich TUI, always uses `U=1`.
- **Add Sixel** to match Windows Terminal: libghostty-vt has no Sixel
  parser or storage, so tai would own the whole protocol. Kitty is already
  parsed and stored by libghostty-vt.
- **Patch libghostty-vt to expose a virtual-placement iterator**: this would
  reuse Ghostty's code directly, but it would be a large, growing patch on
  the pin (see `patches/ghostty/`). A Rust port keeps the pin's patch set
  small.

## Accepted risks

- The Rust port can drift from Ghostty's behaviour as the spec or upstream
  changes. Its tests should reuse Ghostty's cases from
  `graphics_unicode.zig`.
- The removal list can go stale. A terminal that appears later, or one we
  missed, can still leak its identity variables. Add to the list when that
  happens.
- Programs that keyed on an inherited `TERM_PROGRAM` will see `tai`, which
  they don't recognise, and fall back to their generic path. That's the
  correct result.

## Triggers to revisit

- libghostty-vt's C API gains a virtual-placement (unicode placeholder)
  iterator. Then use it instead of the port.
- Sixel lands in Ghostty or libghostty-vt.
- ppxai or textual-image change how they detect image support.
- A program in tai still misdetects the terminal because of an inherited
  variable that isn't on the removal list.
