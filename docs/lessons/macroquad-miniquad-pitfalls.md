# macroquad / miniquad pitfalls

Hazards found while building milestone (a). Each one holds on any machine
and can be checked with grep against the named file.

## The built-in UI empties the clipboard on every Ctrl+C (Linux)

macroquad runs its UI's `process_input` on every frame, even when the app
draws no UI (`lib.rs`, `begin_frame`). While Ctrl and C (or X) are down,
`Ui::key_down` (`ui.rs`) writes the UI's selection to the system
clipboard, and with no UI in use that selection is empty. The result: an
app's own copy gets overwritten on the next frame, and every Ctrl+C in the
shell empties the clipboard. Upstream issue #546 and PR #1073 are both
open.

**What tai does:** `vendor/macroquad` carries a one-line fix that skips
the write when the selection is empty (`TAI-PATCHES.md`). Before
dropping the vendored copy, check that the macroquad release you move to
includes the fix: `grep -n 'clipboard_selection.is_empty' src/ui.rs`.

## The macOS clipboard is a stub

On macOS, miniquad's `window::clipboard_get` returns `None` and
`clipboard_set` does nothing (`MacosClipboard`, `src/native/macos.rs`).
Working NSPasteboard code sits in the same file, but those functions
never reach it. Reported upstream as not-fl3/miniquad#696.

**What tai does:** `src/clipboard.rs` uses `arboard` on macOS and miniquad
everywhere else. A side effect: the clipboard wipe above never happened on
macOS, because it went through the same stub.

## The Metal backend leaks every frame

miniquad's Metal backend has no autorelease pool anywhere: no
`autorelease` or `NSAutoreleasePool` in `src/native/macos.rs` or
`src/graphics/metal.rs`. Its event loop reimplements `[NSApp run]`.
Each frame's autoreleased Metal objects (command buffer, render pass
descriptor, encoder) are therefore never freed.

Measured on the macOS dev host (macOS 12.7.6, Intel Iris Pro, release
build, Retina) with a minimal macroquad 0.4.16 loop
(`clear_background` + `draw_rectangle`):

| Backend | RSS growth |
|---|---|
| Metal | 34 → 51 MB over 180 s, still rising |
| OpenGL | 35 → 39 MB over 70 s, flat |

After 60 s on Metal, `heap` showed 273 live `MTLIGAccelCommandBuffer` and
819 each of `MTLRenderPassDescriptorInternal` and
`MTLIGAccelRenderCommandEncoder`. In tai, which draws far more per frame,
the growth was about 1.2 MB/s and showed up as GPU regions. Reported
upstream as not-fl3/miniquad#697.

**What tai does:** OpenGL is the macOS default, and `TAI_APPLE_GFX=metal`
is opt-in (`macroquad_conf` in `src/main.rs`). Before making Metal the
default, check that miniquad drains an autorelease pool each frame.

## The text API already applies the DPI scale

In macroquad 0.4.16, `measure_text` and `draw_text_ex` multiply
`font_size` by `dpi_scale` themselves, then return and draw in logical
units (`text.rs`). Multiplying by `screen_dpi_scale()` again makes glyphs
twice as wide as their cells on Retina. It doesn't show at 1x, so testing
only on Linux/Xvfb hides it.

**Rule:** measure and draw at the logical font size. Use physical pixels
(logical × `dpi_scale`) only for what the terminal reports outward:
`ws_xpixel`/`ws_ypixel`, `terminal.resize` cell pixels, and XTWINOPS
replies.

## Ghostty needs an exact Zig major.minor

Ghostty's `requireZig` (`src/build/zig.zig`) rejects any Zig whose
major.minor differs from `build.zig.zon`'s `minimum_zig_version`. Newer
isn't accepted. libghostty-vt 0.2.2 pins a Ghostty that needs 0.15.x, so
`scripts/fetch-zig.sh` fetches that version into `.local/`. When bumping
the crate, read the new pin's `build.zig.zon`.

## Ignored signals survive exec

A launcher that ignores SIGINT, such as a non-interactive shell starting a
background job, passes that disposition through `forkpty` and `exec` to
the user's shell, so Ctrl+C interrupts nothing. You can see it as a
nonzero `SigIgn` in `/proc/<pid>/status`. tai's pty child resets job-control
signals to their defaults and clears the signal mask before exec (the
`pty` module in `src/main.rs`).

## Testing with xdotool: hold the modifiers across frames

`xdotool key ctrl+equal` presses and releases everything faster than a
frame, so by the time tai polls `is_key_down`, Ctrl is already up and the
chord arrives unmodified. Press with `keydown`, sleep about 0.1 s, then
`keyup`. Mouse drags have the same problem, so use
`mousedown`/`mousemove`/`mouseup` with sleeps in between.
