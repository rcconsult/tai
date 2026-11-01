# ADR 0012: Ask the OS for fonts that tai lacks, and draw every glyph with skrifa

- **Status:** Accepted 2026-11-01 (owner). Reviewed by the macOS and
  Linux sessions on 2026-11-01, with no objections; their conditions are
  folded in.
- **Deciders:** owner. Written by the Claude session on the Windows dev
  host, with input from the macOS and Linux sessions.
- **Supersedes:** in part, decision 2 of
  [ADR 0008](0008-gpu-cell-renderer.md): the immediate renderer it kept
  for `TAI_APPLE_GFX=metal` is retired (decision 4 below).
- **Related:** [ADR 0003](0003-utf-8-everywhere.md) (rule 4 on spaces,
  rule 5 on one width table), [ADR 0011](0011-native-gui-egui.md)
  (tai-gui, IME through winit).

## Context

CJK text draws as the primary font's missing-glyph box. tai bundles four
fonts: JetBrains Mono, Symbols Nerd Font Mono, Iosevka Fixed and Noto
Emoji. None has Han, kana or Hangul, and `src/fonts.rs` leaves "system-font
discovery for CJK" for later. The grid is already right: libghostty-vt
gives these characters two cells.

Bundling a CJK font is out: Noto Sans CJK is 16–20 MB per weight, and
one font can't serve Chinese, Japanese and Korean readers, who expect
different glyphs for the same unified Han codepoint.

**fontdue can't hold a CJK font, and is costly for the bundled ones.** It
builds every glyph's outline when it parses a font. Measured on the
Windows dev host on 2026-11-01, release build, heap counted with a global
allocator:

| Font | File | Glyphs | fontdue heap | fontdue parse | skrifa open + `水` lookup |
|---|---|---|---|---|---|
| Microsoft YaHei (`msyh.ttc` #0) | 18.8 MB | 30,209 | 161 MB | 0.64 s | 3 µs, no heap |
| Yu Gothic (`YuGothR.ttc` #0) | 13.2 MB | 24,411 | 191 MB | 0.61 s | 5 µs, no heap |
| Malgun Gothic (`malgun.ttf`) | 12.8 MB | 28,215 | 132 MB | 0.50 s | 3 µs, no heap |
| Symbols Nerd Font Mono (bundled) | 2.5 MB | 10,627 | 58 MB | 0.10 s | |
| Iosevka Fixed (bundled) | 1.1 MB | 9,060 | 19 MB | 0.03 s | |
| JetBrains Mono (bundled) | 0.3 MB | 1,743 | 3 MB | 0.01 s | |
| Noto Emoji (bundled, first use) | 0.8 MB | 1,853 | 23 MB | 0.07 s | 3 µs, no heap |

skrifa (fontations) reads tables in place and builds an outline only for
the glyph asked for. It is already in `Cargo.lock` at 0.44.0, with
`vello_cpu` 0.1.0, through epaint 0.36 in tai-gui. tai's own code uses
fontdue in `src/fonts.rs`, `src/cells.rs` (the glyph atlas) and
`src/main.rs` (the immediate renderer's macroquad fonts). macroquad
depends on fontdue itself, so the crate stays in the build either way.

**How Ghostty does it.** Read at a887df42 ("terminal: bound page map
probe lengths without tombstones (#13294)"):

- `src/font/CodepointResolver.zig`, `getIndex`: when no loaded face has a
  codepoint, ask the platform's `discoverFallback` for one that does,
  check it, and add it to the collection as a `DeferredFace`
  (`src/font/DeferredFace.zig`), which loads only when used.
- `src/font/discovery.zig`: CoreText on macOS. For U+4E00–U+9FFF it asks
  `CTFontCreateForString`, because that takes the system locale into
  account. Linux uses fontconfig with the codepoint in the pattern's
  charset. The `freetype_windows` backend walks `%SYSTEMROOT%\Fonts`, then
  the per-user fonts folder, and takes the first face that has the
  codepoint.
- `src/font/main.zig`: `default_fallback_adjustment = .ic_width`.
  `src/font/Collection.zig`, `scaleFactor`, scales a fallback face so its
  `水` (U+6C34) is as wide as the primary's, in ems. A face without `水`
  falls back to ex height, then cap height, then line height. The
  primary's ideograph width, when it has no `水`, is estimated as
  `min(ascii_height, 2 × cell_width)` (`src/font/Metrics.zig`, `icWidth`).
- `src/renderer/cell.zig`, `constraintWidth`: a cell of grid width 2 is
  always drawn two cells wide.

**Ghostty's Windows walk picks the wrong font.** Run over the Windows
dev host's font folders with skrifa, it takes 106 ms and 100 MB of reads
to find a face, and the first face with `水`, `あ` and `한` alike is
`malgun.ttf`, a Korean font, because directory order is alphabetical.
Chinese and Japanese text would draw with Korean glyph forms. Ghostty
doesn't run on Windows, so this backend is little used.

**How Windows Terminal does it.** Read at `703ef7f969` ("Settings: make
clickable SettingsCards support UIA Invoke (#20871)"):
`src/renderer/atlas/AtlasEngine.cpp`, `_mapCharacters`, calls
`IDWriteFontFallback::MapCharacters` on the system font fallback, with
the locale from `GetUserDefaultLocaleName`.

**What the platforms return.** On the Linux dev host (Pop!_OS, `en_US`,
415 families), `fc-match ':charset=<cp>'` gives Noto Sans CJK JP
(`NotoSansCJK-Regular.ttc`, 19.5 MB) for `水`, `あ` and `한`. Adding
`:lang=` changes the Han pick: `zh-cn` gives SC, `zh-tw` TC, `ja` JP, and
`ko` gives JP. That last one is the distro's choice, not missing
coverage: Noto Sans CJK KR is in the same `.ttc`, but Pop!_OS's
`/etc/fonts/conf.d/64-language-selector-prefer.conf` lists JP first and
it ships no Korean language-selector rule. So fontconfig needs the
locale's language passed in, or an English locale gets Japanese Han
forms, and beyond that tai takes what fontconfig returns rather than
second-guessing the distro's rules. macOS's
CJK faces are collections too (`PingFang.ttc` is 77 MB).

**The immediate renderer.** `TAI_RENDERER=immediate` draws glyphs with
macroquad's `draw_text_ex`, from fontdue fonts. It runs in three cases
(`src/main.rs`, the renderer choice):

- `TAI_APPLE_GFX=metal`, because the cell renderer has no MSL shaders.
  Metal is opt-in and leaked GPU memory on the macOS dev host. tai-gui
  already gets Metal through wgpu.
- `TAI_RENDERER=immediate`, which served only as the A/B baseline for
  ADR 0008's measurements. The Linux session's logs show all 16 Linux
  immediate runs were explicit A/B runs. The cell renderer was picked on
  every normal run: 32 on Mesa with OpenGL 4.5 and GLSL 3.30, and a GTX
  980 with OpenGL 4.6. The macOS session has used the cell renderer
  daily on the Intel Iris Pro since ADR 0008 without a failure.
- Automatically, when `TextPass::new` fails (`src/cells.rs`): no GLSL
  3.30 or 1.50, no instancing, or `GL_MAX_TEXTURE_SIZE` below the atlas
  width.

## Decision

1. **Discover on a miss, as Ghostty does.** When the bundled chain
   (primary, Symbols, Iosevka, Noto Emoji) has no glyph for a codepoint,
   ask the platform for a font that has it. The answer is a font file
   path and a face index. A face found once is kept and checked first on
   later misses, before asking the platform again. Codepoints nobody has
   are remembered, so a missing glyph doesn't query the OS every frame.
   This works for any script, not only CJK, but tai doesn't shape text,
   so scripts that need shaping (Devanagari, Arabic) still draw poorly.
   A discovered face that has no outlines, such as Apple Color Emoji
   (sbix bitmaps), is skipped. Emoji stay on the bundled Noto Emoji.
2. **The platform query, on every platform the OS's own fallback, with
   the user's locale:**
   - **Windows:** DirectWrite `IDWriteFontFallback::MapCharacters` with
     `GetUserDefaultLocaleName`, as Windows Terminal does. The font file
     path comes from the mapped face (`IDWriteFontFace::GetFiles`, the
     local loader's `GetFilePathFromKey`) and the face index from
     `GetIndex`. It needs the `windows` crate (`Win32_Graphics_DirectWrite`),
     because `windows-sys` has no COM interfaces.
   - **macOS:** CoreText `CTFontCreateForString`, as Ghostty does,
     through `objc2-core-text`, which matches the objc2 0.6 family
     already in the lock through winit and wgpu. The file comes from
     `kCTFontURLAttribute` plus the face index. A font with no file URL
     is skipped for the next candidate. The APIs used must exist on
     macOS 12.
   - **Linux:** fontconfig with the codepoint in the charset and the
     locale's language in `:lang`, as Ghostty does, plus `FC_INDEX` for
     the face. tai uses the face fontconfig returns as is, and doesn't
     override the distro's language rules. It links libfontconfig,
     which every desktop Linux has.
     Building needs its dev package (`libfontconfig-dev`), which the
     README and CI must list. Shelling out to `fc-match` is not an
     option.

   The Windows session builds the Windows backend. The macOS and Linux
   backends go to those sessions. Until a platform has one, it keeps
   today's missing-glyph box.
3. **Every glyph tai draws comes from skrifa, on every platform.** The
   bundled fonts and discovered faces alike are read with skrifa, and
   their outlines are rasterized one glyph at a time into the shared
   glyph atlas (`GlyphAtlas` in `src/cells.rs`), for both binaries:
   - Outlines are unhinted, as fontdue's are today, so text keeps its
     current weight and doesn't drift from what other apps on the OS
     draw.
   - Glyphs are rasterized at device pixels,
     `ceil(font_size × dpi_scale)`, as now.
   - The rasterizer is a small coverage rasterizer: `zeno` first, or
     `vello_cpu`, which tai-gui already links, if `zeno` falls short.
   - Discovered font files are memory-mapped (`memmap2`, already in the
     lock at 0.9.12), not read onto the heap. Bundled fonts stay
     `include_bytes!`.
   - tai stops calling fontdue. macroquad still depends on it.
4. **Retire the immediate renderer and `TAI_APPLE_GFX=metal`.** The cell
   renderer becomes tai's only renderer. `TAI_RENDERER=immediate` and the
   Metal opt-in are removed, with the CLAUDE.md and README lines that
   describe them. A host whose OpenGL can't run the cell renderer (no
   GLSL 3.30 or 1.50, no instancing, or a small `GL_MAX_TEXTURE_SIZE`)
   gets an error that names the reason, instead of a slower window. ADR
   0008's measurements, which used the immediate renderer as their
   baseline, stay as a record.
5. **Size by ideograph width, as Ghostty does.** A discovered face is
   scaled so its `水` is as wide as JetBrains Mono's estimated ideograph
   width, `min(ascii_height, 2 × cell_width)`, with Ghostty's fallbacks
   for a face without `水`. It's centred on the primary's line, like the
   other fallbacks (`fonts::Fit`), but not capped at its height: Ghostty
   doesn't cap it, and CJK fonts' tall lines made the cap shrink glyphs
   whose ink fits (PingFang's `水` went from 1.05x to 0.94x, leaving about
   0.6 cell empty on macOS). Like Ghostty, tai doesn't centre a CJK glyph
   in its two cells; it's drawn at its own bearing. A wide cell is
   drawn two cells wide in both binaries. tai-gui passes a cell count of
   1 for every glyph today (`src/bin/tai-gui/terminal.rs`, the
   `coverage.place` call), which also shrinks wide emoji. That gets fixed
   here.
6. **The chat pane gets minimal support.** egui's default fonts have no
   CJK. The first time chat text or the input box holds a codepoint
   egui's fonts lack, tai-gui asks the same discovery for it and adds the
   face to egui's `FontDefinitions` as a fallback, for both the
   proportional and monospace families. epaint reads it lazily with
   skrifa.
7. **IME is out of scope.** CJK text renders and pastes. Typing it
   through an input method doesn't work in either binary: neither handles
   IME events, nor draws preedit text. This is recorded as a known gap
   under investigation in the README's "Known gaps".
8. **Order of work, each step checked on all three hosts before the
   next:**
   1. skrifa and the rasterizer in place of fontdue for the bundled
      fonts, with the immediate renderer and Metal removed (decisions 3
      and 4). The macOS session compares pixels against main at 1x and
      Retina 2x. The Linux session runs the AE torture comparison and
      reads heap from `smaps_rollup`.
   2. Discovery and sizing, per platform (decisions 1, 2 and 5). Each
      session checks `水`, `あ` and `한` under its locale.
   3. The chat pane (decision 6), and the README (decision 7).

## Why this and not the alternatives

- **Bundling a CJK font:** 16–20 MB per weight in the binary, and one
  font is wrong for two of the three languages.
- **fontdue for discovered faces only:** 130–190 MB of heap and half a
  second of parsing per face, on the UI thread.
- **skrifa for discovered faces only, fontdue for the bundled ones:** two
  readers, two rasterizers and two sizing paths for one grid, and 80 MB
  of heap kept for the bundled fonts.
- **Porting the immediate renderer to the atlas:** it would keep a second
  renderer alive only for Metal, which leaks, and for OpenGL older than
  3.2, which none of the dev hosts has.
- **Ghostty's Windows directory walk:** Korean glyph forms for Chinese
  and Japanese on a stock Windows 11 install, and 100 MB of reads to find
  a face.
- **A hard-coded list of CJK fonts by locale:** this is the OS's job, and
  it already knows about installed language packs and user fonts.
- **`fontdb` on every platform:** it scans font folders and doesn't
  follow the OS's fallback or locale rules.
- **ab_glyph instead of skrifa:** it would work, but skrifa is already in
  tai-gui's build, is what epaint uses, and is the font reader measured
  above.

## Accepted risks

- Text looks slightly different after the rasterizer change. The pixel
  comparison in step 8.1 decides whether that's acceptable.
- Hosts with OpenGL older than 3.2 can no longer run tai.
- Linux builds need `libfontconfig-dev`.

## Triggers to revisit

- A user sees the wrong regional glyph forms for their locale. Then look
  at how the locale reaches DirectWrite, CoreText or fontconfig.
- The pixel comparison shows a visible regression that the rasterizer
  can't fix. Then try the other rasterizer, or skrifa's hinting.
- Someone needs tai on OpenGL older than 3.2, or without a GPU. Then a
  fallback renderer comes back, drawing from the same atlas.
- A user asks to type CJK. Then wire IME: winit's `Ime` events in
  tai-gui with preedit drawn at the cursor and measured with
  `libghostty_vt::unicode` (ADR 0003 rule 5). macroquad's `tai` would
  need IME support from miniquad.
- Shaped scripts show up in real use. Then shaping (rustybuzz, or
  harfrust) becomes its own decision.
