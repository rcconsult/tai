//! Bundled fonts and per-codepoint fallback.
//!
//! Text is drawn with JetBrains Mono. Codepoints it lacks fall back, in
//! order, to Symbols Nerd Font Mono (the Nerd Font icons in shell prompts),
//! Iosevka Fixed (general symbols such as ⎈ and ⏵, and braille) and Noto
//! Emoji (monochrome emoji). All four fonts are bundled (`fonts/README.md`
//! records where they come from). Unicode spaces draw blank without a font.
//! Colour emoji and system-font discovery for CJK are left for later.
//!
//! Fonts are read in place with skrifa and each glyph's outline is filled
//! with zeno when the glyph is first drawn (ADR 0012). Nothing is parsed up
//! front: fontdue, which tai used before, built every glyph's outline when
//! it loaded a font, about 100 MB of heap for these four.

use skrifa::{
    FontRef, GlyphId, MetadataProvider,
    instance::{LocationRef, Size},
    outline::{DrawSettings, OutlinePen},
};
use zeno::{Command, Mask, Point};

pub const PRIMARY_BYTES: &[u8] = include_bytes!("../fonts/JetBrainsMono-Medium.ttf");
pub const SYMBOLS_BYTES: &[u8] = include_bytes!("../fonts/SymbolsNerdFontMono-Regular.ttf");
pub const IOSEVKA_BYTES: &[u8] = include_bytes!("../fonts/IosevkaFixedSS01-Medium.ttf");
pub const EMOJI_BYTES: &[u8] = include_bytes!("../fonts/NotoEmoji-Regular.ttf");

/// How a codepoint is drawn: with one of the bundled fonts, or not at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Face {
    Primary,
    Symbols,
    Iosevka,
    Emoji,
    /// A Unicode space: the cell keeps its background and draws no glyph.
    Blank,
}

/// Whether `ch` is a space that should draw as an empty cell. These are
/// the Zs spaces except U+1680 OGHAM SPACE MARK, which is visible. Locales
/// print some of them, e.g. U+202F as a thousands separator. U+3000 is a
/// wide character, so it spans two cells like any other.
pub fn is_space(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\u{a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// A glyph's bitmap box and advance at one size, in pixels. `xmin` and
/// `ymin` are the box's left and bottom edges from the pen position on the
/// baseline, with y up, as fontdue reported them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Metrics {
    pub xmin: i32,
    pub ymin: i32,
    pub width: usize,
    pub height: usize,
    pub advance_width: f32,
}

/// One face of a font file, read in place.
///
/// Sizes are applied here, as `units × px / units_per_em` in f32, as fontdue
/// did, not through skrifa's fixed-point scaling: that gives JetBrains
/// Mono's 600-unit advance as 59.99994 px at 100 px, and the cell size in
/// pixels, which is truncated, would lose a pixel.
#[derive(Clone)]
pub struct Typeface {
    font: FontRef<'static>,
    units_per_em: f32,
}

/// Collects a glyph's outline as zeno path commands, from font units to
/// pixels. Font outlines have y up and zeno's masks y down, so y is flipped.
struct Pen {
    scale: f32,
    path: Vec<Command>,
}

impl Pen {
    fn point(&self, x: f32, y: f32) -> Point {
        Point::new(x * self.scale, -y * self.scale)
    }
}

impl OutlinePen for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.path.push(Command::MoveTo(self.point(x, y)));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.path.push(Command::LineTo(self.point(x, y)));
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let command = Command::QuadTo(self.point(cx0, cy0), self.point(x, y));
        self.path.push(command);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let command =
            Command::CurveTo(self.point(cx0, cy0), self.point(cx1, cy1), self.point(x, y));
        self.path.push(command);
    }

    fn close(&mut self) {
        self.path.push(Command::Close);
    }
}

impl Typeface {
    /// Face `index` of the font file in `bytes` (0 unless it's a collection).
    pub fn new(bytes: &'static [u8], index: u32) -> Result<Self, &'static str> {
        let font = FontRef::from_index(bytes, index).map_err(|_| "not a font file")?;
        let units_per_em = f32::from(
            font.metrics(Size::unscaled(), LocationRef::default())
                .units_per_em,
        );
        if units_per_em <= 0.0 {
            return Err("font has no units per em");
        }
        Ok(Self { font, units_per_em })
    }

    /// The glyph for `ch`, or the missing-glyph box when the font has none.
    fn glyph_id(&self, ch: char) -> GlyphId {
        self.font.charmap().map(ch).unwrap_or(GlyphId::NOTDEF)
    }

    /// Whether the font has a glyph for `ch`.
    pub fn has(&self, ch: char) -> bool {
        self.font.charmap().map(ch).is_some()
    }

    /// Pixels per font unit at `px` pixels per em.
    fn scale(&self, px: f32) -> f32 {
        px / self.units_per_em
    }

    /// How far glyph `glyph` moves the pen at `px` pixels per em.
    fn glyph_advance(&self, glyph: GlyphId, px: f32) -> f32 {
        let units = self
            .font
            .glyph_metrics(Size::unscaled(), LocationRef::default())
            .advance_width(glyph)
            .unwrap_or(0.0);
        units * self.scale(px)
    }

    /// How far `ch` moves the pen at `px` pixels per em.
    pub fn advance(&self, ch: char, px: f32) -> f32 {
        self.glyph_advance(self.glyph_id(ch), px)
    }

    /// The line's ascent and descent (negative below the baseline) at `px`.
    fn line_metrics(&self, px: f32) -> (f32, f32) {
        let m = self.font.metrics(Size::unscaled(), LocationRef::default());
        (m.ascent * self.scale(px), m.descent * self.scale(px))
    }

    /// `ch`'s box and advance at `px`, as `rasterize` would draw it.
    pub fn metrics(&self, ch: char, px: f32) -> Metrics {
        self.rasterize(ch, px).0
    }

    /// `ch` at `px` pixels per em: its box and advance, and one coverage
    /// byte per pixel, row by row from the top. Unhinted, as fontdue drew
    /// it, so text keeps its weight.
    pub fn rasterize(&self, ch: char, px: f32) -> (Metrics, Vec<u8>) {
        let glyph = self.glyph_id(ch);
        let advance_width = self.glyph_advance(glyph, px);
        let mut pen = Pen {
            scale: self.scale(px),
            path: Vec::new(),
        };
        if let Some(outline) = self.font.outline_glyphs().get(glyph) {
            let settings = DrawSettings::unhinted(Size::unscaled(), LocationRef::default());
            if outline.draw(settings, &mut pen).is_err() {
                pen.path.clear();
            }
        }
        let empty = Metrics {
            advance_width,
            ..Metrics::default()
        };
        if pen.path.is_empty() {
            return (empty, Vec::new());
        }
        let (bitmap, place) = Mask::new(&pen.path).render();
        if place.width == 0 || place.height == 0 {
            return (empty, Vec::new());
        }
        let metrics = Metrics {
            xmin: place.left,
            ymin: -(place.top + place.height as i32),
            width: place.width as usize,
            height: place.height as usize,
            advance_width,
        };
        (metrics, bitmap)
    }
}

/// How a fallback font's glyphs are fitted to the primary font's cells.
struct Fit {
    /// Font size as a fraction of the primary size, so one glyph is one
    /// cell wide.
    scale: f32,
    /// The largest scale whose line still fits the primary's line.
    max_scale: f32,
    /// Vertical middle of the font's line, (ascent + descent) / 2, in em
    /// above the baseline. Used to centre glyphs in the row.
    mid: f32,
}

/// Glyphs used to measure each font's advance. All fonts are monospaced.
const PRIMARY_PROBE: char = 'M';
const SYMBOLS_PROBE: char = '\u{f113}';
const IOSEVKA_PROBE: char = 'M';
const EMOJI_PROBE: char = '\u{1f600}';
const PROBE_PX: f32 = 100.0;

fn mid(font: &Typeface) -> f32 {
    let (ascent, descent) = font.line_metrics(PROBE_PX);
    (ascent + descent) / 2.0 / PROBE_PX
}

impl Fit {
    fn new(primary: &Typeface, font: &Typeface, probe: char) -> Self {
        let height = |font: &Typeface| {
            let (ascent, descent) = font.line_metrics(PROBE_PX);
            ascent - descent
        };
        Self {
            scale: primary.advance(PRIMARY_PROBE, PROBE_PX) / font.advance(probe, PROBE_PX),
            max_scale: height(primary) / height(font),
            mid: mid(font),
        }
    }
}

/// The bundled fonts, which codepoints each covers, and how each fits the
/// primary's cells. Covering a codepoint is a lookup in the font's own
/// character map.
pub struct Coverage {
    primary: Typeface,
    symbols: Typeface,
    iosevka: Typeface,
    emoji: Typeface,
    primary_mid: f32,
    symbols_fit: Fit,
    iosevka_fit: Fit,
    emoji_fit: Fit,
}

/// How to draw a glyph in a cell: the font size to draw at and how far to
/// raise the baseline, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub font_size: u16,
    pub rise: f32,
}

impl Coverage {
    pub fn new() -> Result<Self, &'static str> {
        let primary = Typeface::new(PRIMARY_BYTES, 0)?;
        let symbols = Typeface::new(SYMBOLS_BYTES, 0)?;
        let iosevka = Typeface::new(IOSEVKA_BYTES, 0)?;
        let emoji = Typeface::new(EMOJI_BYTES, 0)?;
        Ok(Self {
            primary_mid: mid(&primary),
            symbols_fit: Fit::new(&primary, &symbols, SYMBOLS_PROBE),
            iosevka_fit: Fit::new(&primary, &iosevka, IOSEVKA_PROBE),
            emoji_fit: Fit::new(&primary, &emoji, EMOJI_PROBE),
            primary,
            symbols,
            iosevka,
            emoji,
        })
    }

    /// How to draw `ch`. Unicode spaces are blank. Otherwise the first font
    /// in the chain that has a glyph for it: primary, then Symbols, then
    /// Iosevka, then Noto Emoji. When none has it, the primary draws its
    /// missing-glyph box.
    pub fn face_for(&self, ch: char) -> Face {
        if is_space(ch) {
            Face::Blank
        } else if self.primary.has(ch) {
            Face::Primary
        } else if self.symbols.has(ch) {
            Face::Symbols
        } else if self.iosevka.has(ch) {
            Face::Iosevka
        } else if self.emoji.has(ch) {
            Face::Emoji
        } else {
            Face::Primary
        }
    }

    /// The font that draws `face`. `Blank` draws nothing, so any font will
    /// do.
    pub fn font(&self, face: Face) -> &Typeface {
        match face {
            Face::Primary | Face::Blank => &self.primary,
            Face::Symbols => &self.symbols,
            Face::Iosevka => &self.iosevka,
            Face::Emoji => &self.emoji,
        }
    }

    /// Where to draw a glyph of `face` when the primary font is at
    /// `font_size`. Fallback glyphs are sized to span `cells` cells, but no
    /// taller than the primary's line, and centred on that line.
    pub fn place(&self, face: Face, font_size: u16, cells: u8) -> Placement {
        let fit = match face {
            Face::Primary | Face::Blank => {
                return Placement {
                    font_size,
                    rise: 0.0,
                };
            }
            Face::Symbols => &self.symbols_fit,
            Face::Iosevka => &self.iosevka_fit,
            Face::Emoji => &self.emoji_fit,
        };
        let scale = (fit.scale * f32::from(cells)).min(fit.max_scale);
        let size = (f32::from(font_size) * scale).round().max(1.0);
        Placement {
            font_size: size as u16,
            rise: self.primary_mid * f32::from(font_size) - fit.mid * size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_uses_the_primary_font() {
        let c = Coverage::new().unwrap();
        for ch in ['A', 'z', '0', '|', '~'] {
            assert_eq!(c.face_for(ch), Face::Primary, "{ch:?}");
        }
    }

    #[test]
    fn nerd_font_icons_use_symbols() {
        let c = Coverage::new().unwrap();
        // U+F113 GitHub, U+E7AD devicon, U+F308 Linux logo, U+F0A0 hard disk.
        for ch in ['\u{f113}', '\u{e7ad}', '\u{f308}', '\u{f0a0}'] {
            assert_eq!(c.face_for(ch), Face::Symbols, "U+{:04X}", ch as u32);
        }
    }

    #[test]
    fn general_symbols_use_iosevka() {
        let c = Coverage::new().unwrap();
        // U+2388 ⎈ kube-ps1's prompt symbol; ⏵ ⏸ ⏺ ⎿ ✲ from Claude Code's
        // status lines; U+28FF a braille spinner frame.
        let claude_code = ['\u{23f5}', '\u{23f8}', '\u{23fa}', '\u{23bf}', '\u{2732}'];
        for ch in ['\u{2388}', '\u{28ff}'].into_iter().chain(claude_code) {
            assert_eq!(c.face_for(ch), Face::Iosevka, "U+{:04X}", ch as u32);
        }
    }

    #[test]
    fn primary_keeps_glyphs_it_has() {
        let c = Coverage::new().unwrap();
        // JetBrains Mono ships the core powerline separators and ✗ / ➜,
        // which Iosevka also has.
        for ch in ['\u{e0b0}', '\u{2717}', '\u{279c}'] {
            assert_eq!(c.face_for(ch), Face::Primary, "U+{:04X}", ch as u32);
        }
    }

    #[test]
    fn unicode_spaces_are_blank() {
        let c = Coverage::new().unwrap();
        for ch in [
            ' ', '\u{a0}', '\u{2007}', '\u{2009}', '\u{202f}', '\u{205f}', '\u{3000}',
        ] {
            assert_eq!(c.face_for(ch), Face::Blank, "U+{:04X}", ch as u32);
        }
        // Visible, though Zs.
        assert!(!is_space('\u{1680}'));
    }

    #[test]
    fn fallback_glyphs_fit_their_cells() {
        let c = Coverage::new().unwrap();
        let primary = Placement {
            font_size: 20,
            rise: 0.0,
        };
        assert_eq!(c.place(Face::Primary, 20, 1), primary);
        let p = c.place(Face::Symbols, 20, 1);
        // Cell width is 0.6 em of the primary font: 12 px at size 20.
        assert_eq!(p.font_size, 12);
        assert!(p.rise > 0.0, "{p:?}");
        // Two cells are 1.2 em, still shorter than the primary's 1.32 em line.
        assert_eq!(c.place(Face::Symbols, 20, 2).font_size, 24);
        // Iosevka advances 0.5 em, so filling a cell would need 1.2x, but its
        // 1.25 em line caps it at about 1.06x of the primary's 1.32 em line.
        // A second cell doesn't make it any bigger.
        assert_eq!(c.place(Face::Iosevka, 20, 1).font_size, 21);
        assert_eq!(c.place(Face::Iosevka, 20, 2).font_size, 21);
    }

    #[test]
    fn emoji_use_noto_emoji() {
        let c = Coverage::new().unwrap();
        // Textual's file-tree icons 📁 📄, ppxai's 💭 📊, and U+2638 ☸
        // (kube-ps1's alternative symbol).
        for ch in [
            '\u{1f4c1}',
            '\u{1f4c4}',
            '\u{1f4ad}',
            '\u{1f4ca}',
            '\u{2638}',
        ] {
            assert_eq!(c.face_for(ch), Face::Emoji, "U+{:04X}", ch as u32);
        }
        // One cell is 12 px at size 20, and Noto Emoji's glyphs are about
        // 1.3 em wide. Two cells would allow 24 px, but its 1.4 em line
        // caps it at about 0.95x of the primary's 1.32 em line.
        assert_eq!(c.place(Face::Emoji, 20, 1).font_size, 9);
        assert_eq!(c.place(Face::Emoji, 20, 2).font_size, 19);
    }

    #[test]
    fn missing_everywhere_stays_primary() {
        let c = Coverage::new().unwrap();
        assert_eq!(c.face_for('\u{10fffd}'), Face::Primary);
    }

    #[test]
    fn glyph_boxes_sit_on_the_baseline() {
        let c = Coverage::new().unwrap();
        let primary = c.font(Face::Primary);
        // JetBrains Mono's M: 0.6 em advance, flat on the baseline, about
        // 0.73 em tall (its cap height).
        let (m, bitmap) = primary.rasterize('M', 100.0);
        assert_eq!(m.advance_width, 60.0);
        assert_eq!(m.ymin, 0);
        assert!((70..=75).contains(&m.height), "{m:?}");
        assert_eq!(bitmap.len(), m.width * m.height);
        assert!(bitmap.contains(&255));
        // A descender reaches below the baseline.
        assert!(primary.metrics('g', 100.0).ymin < 0);
        // A space has an advance and no pixels.
        let (space, bitmap) = primary.rasterize(' ', 100.0);
        assert_eq!((space.width, space.height), (0, 0));
        assert!(space.advance_width > 0.0 && bitmap.is_empty());
    }
}
