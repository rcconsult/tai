# Vendored macroquad

Source: the crates.io release of macroquad 0.4.16 (not-fl3/macroquad),
MIT OR Apache-2.0. Only `src/`, `Cargo.toml`, the licences and the README
are vendored. The `[[example]]`, `[[test]]` and `[[bench]]` targets were
removed from `Cargo.toml` because their files aren't vendored.

tai's root `Cargo.toml` uses this copy through `[patch.crates-io]`.

## Patches

1. `src/ui.rs`, `Ui::key_down`: skip the clipboard write when the UI
   selection is empty. macroquad's built-in UI processes input on every
   frame, even when the app draws no UI. With Ctrl held and C or X down, it
   set the system clipboard to its empty selection. That broke tai's copy
   (Ctrl+Shift+C) and made every Ctrl+C in the shell wipe the clipboard.
   Upstream: issue #546 (open), fix proposed in PR #1073 (open as of
   2026-10-24).
2. `Cargo.toml`: `[lints.rust] warnings = "allow"`. Path dependencies
   aren't lint-capped the way registry crates are, so without this,
   macroquad's own warnings would show up in tai's builds.
3. `src/text.rs`, new `Font::with_fresh_cache`: a font sharing the parsed
   font data (`Arc<fontdue::Font>`) with an empty glyph cache in a new
   atlas. tai calls it on font-size changes instead of loading the fonts
   again, which re-parsed every glyph (about 0.1 s per step in release,
   0.8 s in debug, for the three bundled fonts). Upstream has no way to
   clear a font's cache or share its parsed data, because `set_atlas` and
   `set_characters` are `pub(crate)`.
4. `src/text.rs`, new `load_ttf_font_from_fontdue`: a font from an
   `Arc<fontdue::Font>` the caller already parsed. tai's fallback lookup
   (`Coverage` in `src/fonts.rs`) needs the parsed fonts too, and fontdue
   keeps every glyph's outline, so parsing the three bundled fonts twice
   cost about 100 MB (a fresh tai went from 283 MB to 180 MB footprint on
   macOS). Upstream only loads fonts from bytes.

Drop this vendored copy once a macroquad release includes the clipboard fix,
a way to reset a font's glyph cache without parsing it again, and a way to
build a font from parsed font data.
