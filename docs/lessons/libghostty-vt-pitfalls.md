# libghostty-vt pitfalls

Hazards in how tai builds and uses libghostty-vt. Each one holds on every
OS and can be checked with grep against the named file.

## A debug build of tai stalls on heavy output

`libghostty-vt-sys`'s build script (`build.rs`, `zig_optimize_mode`)
compiles Ghostty in Zig `Debug` mode whenever cargo sets `DEBUG=true`, as
it does for the `dev` profile. `[profile.dev.package."*"] opt-level` does
not change that. Ghostty's `Debug` mode turns on `slow_runtime_safety`
(`src/build/Config.zig`), and with it `Page.assertIntegrity`
(`src/terminal/page.zig`) re-verifies a whole page, cell by cell, each time
the scrollback grows. Each cell's check includes a hyperlink-map lookup
that can probe the whole map.

On 2026-10-24 a debug tai on Windows froze ("Not Responding", one core at
100%, memory flat) during a recursive `ls` in WSL after font-size steps.
Every stack sample was `linefeed` → `PageList.grow` →
`Page.assertIntegrity` → `verifyIntegrity` → `lookupHyperlink`.

`scripts/cargo.sh` exports `LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseSafe` unless
the build is `-r`, `--release` or `--profile release*`. The build script honours
that variable over `DEBUG`, and applies it to every profile, which is why
it isn't set in `.cargo/config.toml`: there it would also turn release
builds from ReleaseFast into ReleaseSafe. ReleaseSafe keeps Zig's safety
checks (bounds, overflow) but not `slow_runtime_safety`. Plain `cargo`
skips the wrapper and gets the Debug mode back.

tai logs the mode at startup: `tai | simd: enabled, optimize: ReleaseSafe`.
To debug libghostty-vt itself, override it for one build:
`LIBGHOSTTY_VT_SYS_OPTIMIZE=Debug scripts/cargo.sh build`, and expect
heavy output to be slow.

## Output with many hyperlinks crashes the pinned Ghostty

At the pin (a887df42), `Terminal.print` and `printCell`
(`src/terminal/Terminal.zig`) keep cell pointers across calls that can grow,
and so replace, the cursor's page. When a page runs out of hyperlink space,
they read or write freed memory. On 2026-10-25 a ReleaseSafe tai on macOS
crashed in `Terminal.print` within seconds of
`ls -R --hyperlink=always /usr/share`, right after the log line "error
reallocating for more hyperlink space". Upstream fixed it in 33cda4dc5
"terminal: reload cell pointers when print grows a page", which needs Zig
0.16. Until the pin moved to 521cf2c5, which contains it, tai backported
its code hunks as a patch in `patches/ghostty/`.

`scripts/repro-hyperlink-ls.sh target/debug/tai` reproduces it. It runs
the listing inside tai and prints whether tai finished, froze or crashed,
the Ghostty optimize mode, and how many "OutOfSpace" warnings were logged.
Thousands of warnings with no crash mean the patch works. Run it on dev and
release builds after moving the pin or dropping the patch. It works on
macOS and Linux only, because Windows tai ignores `$SHELL`
(`src/pty_windows.rs`, `default_shell`). On 2026-10-25,
on macOS with 16,950 lines: the unpatched ReleaseSafe build crashed in
about 4 s, Zig Debug froze for 300 s, and the patched builds finished in
about 15 s (ReleaseSafe) and 9 s (ReleaseFast).

## Changing the Ghostty source doesn't rebuild libghostty-vt

`libghostty-vt-sys`'s build script reruns only when its environment
variables change (`rerun-if-env-changed`), not when files under
`GHOSTTY_SOURCE_DIR` change. A new pin or patch would leave the old library
in `target/`. `scripts/cargo.sh` therefore keeps the Ghostty checkout's
HEAD in `<target dir>/.tai-ghostty-head` and runs
`cargo clean -p libghostty-vt-sys` when it differs. Plain `cargo` skips
that check.

## Overlapping Kitty images draw in no fixed order

libghostty-vt's placement iterator walks the image storage's hash map
(`src/terminal/c/kitty_graphics.zig`, `storage.placements.iterator()`,
a887df42), so it returns placements in no particular order. Its layer
filter only sorts them into the three z ranges (below the background,
below the text, above the text). Ghostty's own renderer sorts them first,
by z and then by image id, so that with equal z the newer image (the
higher id) draws on top (`src/renderer/image.zig`, the `sortUnstable` call
and `Id.zLessThan`, a887df42).

Drawn in the iterator's order, two images that overlap at the same z, as
when a program redraws a carousel or swaps a picture in place, can show
the older one over the newer one. This affects translated Sixel images
and Kitty images with a fixed placement, but not Unicode placeholders,
which are cells. tai sorts as Ghostty does (`src/main.rs`,
`kitty_image_draws`). The test `overlapping_images_draw_oldest_first`
draws eight images on one cell. Without the sort, the iterator returned
them as ...647, 649, 648, 650, 654, 653, 651, 652.

Translated Sixel images also carry no image id the program could delete
them by (`src/sixel.rs`, `kitty_commands`), so a replaced image stays in
storage underneath the new one. It's hidden only if the new one is opaque
and at least as large.

## Grapheme clustering (mode 2027) is off on purpose

Ghostty's app turns on mode 2027 by default (`src/termio/Termio.zig`,
`grapheme-width-method = unicode`, at a887df42). tai doesn't. With 2027
on, VS16 makes a narrow symbol such as ☸️ two cells wide, and a ZWJ
sequence such as 👨‍👩‍👧 takes two cells instead of six. Shells don't
count it that way: bash's readline and zsh's line editor use `wcwidth`,
which gives ☸️ one cell and 👨‍👩‍👧 six. On 2026-11-01, with 2027 on,
editing a recalled line over those emoji in bash (Linux, and WSL under
ConPTY) put the cursor in the middle of a glyph, made Backspace redraw
the wrong cells, and ran a line that differed from what the screen
showed. Ghostty has the same behaviour and calls it the shells' bug:
ghostty-org/ghostty#2027, "Grapheme clusters and backspace cause
incorrect cursor positioning".

ConPTY isn't a factor on Windows: asked from inside tai
(`GetConsoleScreenBufferInfo`), the bundled OpenConsole moved the cursor
by mode 2027's widths in every case tried (☸️ 2, ☸︎ 1, 👨‍👩‍👧 2).
PowerShell's PSReadLine misaligns with 2027 off as well as on, because it
does its own width arithmetic and steps by UTF-16 unit. Leaving 2027 off
keeps bash and zsh right; it doesn't fix PowerShell.

The cost of leaving it off is cosmetic. "☸️ helm" draws as "☸helm",
because a narrow symbol may grow into a blank next cell, as in Ghostty
(`src/renderer/cell.zig`, `constraintWidth`). Programs that handle
grapheme widths can still turn 2027 on themselves; tai honours it.

Branch `grapheme-cluster` turns it on at startup. Before reviving it,
tai also needs to shape ZWJ sequences, flags and skin tones into one
glyph: with 2027 on, each such sequence gets two cells, and tai draws
every codepoint in them, so they overlap the next cells. The bundled Noto
Emoji has GSUB ligatures for these sequences. A full reset (`ESC c`)
would also turn the mode off, because libghostty-vt's C API has no
default-modes option.

## encode_to_vec can drop a key or mouse event

libghostty-vt 0.2.2's `Encoder::encode_to_vec`, for keys (`src/key.rs`)
and for the mouse (`src/mouse.rs`), retries a buffer that's too small
with `vec.reserve(required - remaining)`. `Vec::reserve` counts from the
length, so when bytes are already queued in the Vec the retry can be too
small as well, and the call returns `OutOfSpace`: the event is lost.
Amortised growth hides it most of the time.

tai-gui hit it on Windows, logged as "input: out of space, 16 bytes
required", when typed text and a special key landed in the same buffer
in one frame. Both binaries now reserve 64 bytes before every encode
(`ENCODE_ROOM` in `src/main.rs`; `encode_key` in
`src/bin/tai-gui/terminal.rs`). The test in `src/main.rs`
`a_key_queued_after_other_bytes_isnt_lost` reproduces the drop with seven
bytes queued in an 8-byte buffer, and fails once the crate is fixed; drop
the workaround then. Still present upstream (libghostty-rs) at 575ed6b8ce.
