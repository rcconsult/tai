# ADR 0008: Draw the grid from GPU cell buffers, as Ghostty does

- **Status:** Accepted 2026-10-25 (owner). The prototype and its macOS
  measurements follow on their own branch (decision 3).
- **Deciders:** owner. Written by the Claude session on the macOS dev host.
- **Related:** [ADR 0001](0001-stack-rust-libghostty-vt-ppxai-server.md)
  (milestone (a) renders with macroquad; milestone (b) moves to winit +
  wgpu; one trigger is macroquad not holding an interactive frame rate on
  the weaker dev host).

## Context

tai draws the terminal grid with macroquad's immediate-mode calls. Each
frame it walks the rows of libghostty-vt's render state and calls
`draw_rectangle` for backgrounds and `draw_text_ex` for glyphs, onto a kept
frame texture (`src/main.rs`, the renderer). Three changes made on
2026-10-25 borrowed ideas from Ghostty without changing that model:

- **Two passes** (6b62237): all backgrounds first, with same-colour
  neighbours on a row merged, then all glyphs. Before this, alternating a
  background rectangle and a glyph texture per cell gave macroquad about
  10k draw calls per frame on a full-colour screen. The CPU went into the
  Intel GL driver's per-call work while the GPU sat 97% idle.
- **Dirty rows** (d6a3717): only rows libghostty-vt marks dirty, plus the
  old and new cursor rows, are redrawn onto the kept texture.
- **Glyph pre-caching** (c0cc9cc): a frame's text is measured before any
  of it is drawn, because macroquad's font atlas can grow mid-frame and
  replace the texture that already-queued glyphs point at.

Ghostty's renderer works differently (`src/renderer/generic.zig` at
a887df42, `rebuildCells`, `rebuildRow`, `drawFrame`):

- It keeps the grid on the CPU as **cell buffers**: one background colour
  per cell, and one text instance per glyph (atlas position and size,
  bearings, grid position, colour, flags). Only dirty rows are rebuilt.
- Each frame uploads the buffers and draws the grid in **two GPU draws**:
  one full-screen pass whose fragment shader reads each pixel's cell colour
  (`shaders/glsl/cell_bg.f.glsl`), and one instanced draw of quads, one per
  glyph (`cell_text.v.glsl`, `cell_text.f.glsl`).
- It owns its **glyph atlas** (`src/font/Atlas.zig`), so it controls when
  the atlas grows and what happens to glyphs already placed.

Measured on the macOS dev host (Intel HD 5000, OpenGL, dev builds), with
current main:

- Full-screen colour fill at 12 fps: 18–21% CPU.
- 22 MB `cat` flood: about 0.8–1.0 s.
- ppxaide: 40–55 fps, 2–8% CPU.
- Typing feels delayed, also with builds from before these changes. The
  cause hasn't been measured; drawing is one suspect.

On Linux, popos-tai measured that a full redraw of 131×29 coloured cells
in a debug build on llvmpipe (software GL) takes about as long as the
14 ms parse window, so a flood gets about half the wall time. That reading
isn't proven.

Constraints for a port into the current stack:

- **OpenGL version.** Ghostty's GL renderer needs OpenGL 4.3
  (`src/renderer/OpenGL.zig`, `MIN_VERSION_MAJOR`/`MINOR`) and its shaders
  are `#version 430 core` with SSBOs (`layout(binding = 1, std430)
  readonly buffer bg_cells`). macOS stops at OpenGL 4.1, and miniquad
  0.4.11 asks for a 3.2 core profile (`src/native/macos.rs`). The shaders
  can't be copied; they have to be rewritten without SSBOs.
- **miniquad can do it.** Its `draw` takes `num_instances`, and vertex
  buffers can step `PerInstance` (`src/graphics.rs`). macroquad hands out
  the miniquad context through `get_internal_gl()`, whose `flush()` draws
  macroquad's own batch first. Instancing needs GL 3.3 or
  ARB_instanced_arrays. The macOS 4.1 core context and Mesa have it; the
  Windows drivers tai runs on haven't been checked.
- **Metal** is opt-in (`TAI_APPLE_GFX=metal`) and would need its own MSL
  shaders.
- **libghostty-vt types are `!Send`**, so building the cell buffers stays
  on the main thread, as the current renderer does.

## Decision

1. **Port Ghostty's cell-buffer renderer into milestone (a) now**, on
   miniquad under macroquad, instead of waiting for winit + wgpu in
   milestone (b).
   - **CPU side:** per-row cell buffers rebuilt from dirty rows, as in
     `rebuildRow`. A background colour per cell, and a text instance per
     glyph.
   - **Glyph atlas:** tai's own, with glyphs rasterised by fontdue (already
     a dependency), modelled on `src/font/Atlas.zig`. When it grows it
     copies the old contents instead of repacking, so placed glyphs keep
     their position and the pre-caching of c0cc9cc becomes unnecessary.
   - **Backgrounds:** a `cols × rows` RGBA texture, sampled with nearest
     filtering by a full-screen pass. This replaces the SSBO of
     `cell_bg.f.glsl`.
   - **Text:** one instanced draw, with the per-glyph data as
     per-instance vertex attributes instead of an SSBO. The shader math
     ports from `cell_text.v.glsl`.
   - Cursor, selection, Kitty images and icons keep their current
     macroquad drawing on top, until they need to move too.
   - Record the Ghostty paths and the a887df42 pin in the code, as for
     other ports.
2. **OpenGL first.** Write GLSL that runs on a 3.2 core context (macOS),
   Mesa and Windows. Under `TAI_APPLE_GFX=metal`, tai keeps the current
   renderer until MSL shaders exist.
3. **Gate on the weakest host.** Build it on its own branch and test it on
   the macOS dev host first. It goes to the Linux and Windows sessions only
   when it passes there:
   - It renders the same as main: the glyph stress test, ppxaide, the
     long coloured prompt, font steps (Cmd+= and Cmd+0), and both the 1x
     and Retina displays.
   - It's no slower than main on the colour fill (CPU), the flood (wall
     time) and ppxaide (fps and CPU), and we measure keypress-to-frame
     latency on both.
   - If it fails the performance gate, it stays unmerged and this record
     says why.
4. **Milestone (b) reuses it.** The CPU side (cell buffers, glyph atlas)
   doesn't depend on macroquad. Moving to wgpu then means new GPU code
   only.

## Why this and not the alternatives

- **Wait for wgpu in milestone (b).** Nothing would be thrown away, but
  tai would ride on immediate-mode drawing, with its per-call driver cost
  and macroquad's atlas behaviour, for the whole of milestone (a). The
  CPU side of the port is needed in (b) anyway; only the shaders and the
  draw calls are milestone-(a) code.
- **Keep tuning the immediate-mode renderer.** The two-pass and dirty-row
  changes got most of the easy wins. What's left (per-glyph quads built
  on the CPU each frame, an atlas tai doesn't control) is what Ghostty's
  design removes.
- **Copy Ghostty's GLSL as is.** It needs OpenGL 4.3 and SSBOs, which
  macOS doesn't have.
- **Move to wgpu now, ahead of milestone (b).** That swaps the windowing
  and input layer (winit) at the same time, which is a much larger change
  than the renderer alone, and wgpu on this Intel Mac hasn't been tried.

## Accepted risks

- Two GPU backends to keep working for a while (GLSL now, wgpu later, and
  MSL if Metal stays). The shaders are small.
- A port can drift from Ghostty's behaviour, for example in glyph
  positioning and bearings. The visual gate above compares against main,
  not against Ghostty.
- Owning the atlas means owning its edge cases, such as glyphs larger than
  a cell and font-size changes invalidating it.

## Triggers to revisit

- The prototype fails the macOS performance gate. Then keep the current
  renderer and supersede this record.
- Milestone (b) starts. Then port the GPU side to wgpu and drop the GLSL
  path.
- Ghostty ships the GPU renderer library ADR 0001 mentions. Then consider
  using it instead.
- tai needs colour glyphs (emoji). Ghostty keeps a separate colour atlas
  for those.
