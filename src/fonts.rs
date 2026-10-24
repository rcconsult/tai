//! Bundled fonts and per-codepoint fallback.
//!
//! Text is drawn with JetBrains Mono. Codepoints it lacks fall back, in
//! order, to Symbols Nerd Font Mono (the Nerd Font icons in shell prompts)
//! and Iosevka Fixed (general symbols such as ⎈ and ⏵, and braille). All
//! three fonts are bundled (`fonts/README.md` records where they come
//! from). Unicode spaces draw blank without a font. System-font discovery
//! for emoji and CJK is left for later.

use macroquad::Error;
use macroquad::text::{Font, load_ttf_font_from_bytes};

pub const PRIMARY_BYTES: &[u8] = include_bytes!("../fonts/JetBrainsMono-Medium.ttf");
pub const SYMBOLS_BYTES: &[u8] = include_bytes!("../fonts/SymbolsNerdFontMono-Regular.ttf");
pub const IOSEVKA_BYTES: &[u8] = include_bytes!("../fonts/IosevkaFixedSS01-Medium.ttf");

/// How a codepoint is drawn: with one of the bundled fonts, or not at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Face {
    Primary,
    Symbols,
    Iosevka,
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

/// Which codepoints each bundled font covers. This doesn't depend on the
/// font size, so it's built once. fontdue keeps a codepoint → glyph map, so
/// a lookup is a hash lookup with no rasterizing.
pub struct Coverage {
    primary: fontdue::Font,
    symbols: fontdue::Font,
    iosevka: fontdue::Font,
    primary_mid: f32,
    symbols_fit: Fit,
    iosevka_fit: Fit,
}

/// How to draw a glyph in a cell: the font size to draw at and how far to
/// raise the baseline, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub font_size: u16,
    pub rise: f32,
}

/// Glyphs used to measure each font's advance. All fonts are monospaced.
const PRIMARY_PROBE: char = 'M';
const SYMBOLS_PROBE: char = '\u{f113}';
const IOSEVKA_PROBE: char = 'M';
const PROBE_PX: f32 = 100.0;

fn line_metrics(font: &fontdue::Font) -> Result<fontdue::LineMetrics, &'static str> {
    font.horizontal_line_metrics(PROBE_PX)
        .ok_or("font has no horizontal line metrics")
}

fn mid(m: fontdue::LineMetrics) -> f32 {
    (m.ascent + m.descent) / 2.0 / PROBE_PX
}

impl Fit {
    fn new(
        primary: &fontdue::Font,
        font: &fontdue::Font,
        probe: char,
    ) -> Result<Self, &'static str> {
        let advance = |font: &fontdue::Font, ch| font.metrics(ch, PROBE_PX).advance_width;
        let height = |m: fontdue::LineMetrics| m.ascent - m.descent;
        let (p_lines, f_lines) = (line_metrics(primary)?, line_metrics(font)?);
        Ok(Self {
            scale: advance(primary, PRIMARY_PROBE) / advance(font, probe),
            max_scale: height(p_lines) / height(f_lines),
            mid: mid(f_lines),
        })
    }
}

impl Coverage {
    pub fn new() -> Result<Self, &'static str> {
        let settings = fontdue::FontSettings::default();
        let primary = fontdue::Font::from_bytes(PRIMARY_BYTES, settings)?;
        let symbols = fontdue::Font::from_bytes(SYMBOLS_BYTES, settings)?;
        let iosevka = fontdue::Font::from_bytes(IOSEVKA_BYTES, settings)?;
        Ok(Self {
            primary_mid: mid(line_metrics(&primary)?),
            symbols_fit: Fit::new(&primary, &symbols, SYMBOLS_PROBE)?,
            iosevka_fit: Fit::new(&primary, &iosevka, IOSEVKA_PROBE)?,
            primary,
            symbols,
            iosevka,
        })
    }

    /// How to draw `ch`. Unicode spaces are blank. Otherwise the first font
    /// in the chain that has a glyph for it (glyph index ≠ 0): primary,
    /// then Symbols, then Iosevka. When none has it, the primary draws its
    /// missing-glyph box.
    pub fn face_for(&self, ch: char) -> Face {
        let has = |font: &fontdue::Font| font.lookup_glyph_index(ch) != 0;
        if is_space(ch) {
            Face::Blank
        } else if has(&self.primary) {
            Face::Primary
        } else if has(&self.symbols) {
            Face::Symbols
        } else if has(&self.iosevka) {
            Face::Iosevka
        } else {
            Face::Primary
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
        };
        let scale = (fit.scale * f32::from(cells)).min(fit.max_scale);
        let size = (f32::from(font_size) * scale).round().max(1.0);
        Placement {
            font_size: size as u16,
            rise: self.primary_mid * f32::from(font_size) - fit.mid * size,
        }
    }
}

/// The macroquad fonts used for drawing. Each one keeps a glyph atlas per
/// size it has drawn, so they're reloaded on font-size changes to drop the
/// old atlases.
pub struct Fonts {
    pub primary: Font,
    pub symbols: Font,
    pub iosevka: Font,
}

impl Fonts {
    pub fn load() -> Result<Self, Error> {
        Ok(Self {
            primary: load_ttf_font_from_bytes(PRIMARY_BYTES)?,
            symbols: load_ttf_font_from_bytes(SYMBOLS_BYTES)?,
            iosevka: load_ttf_font_from_bytes(IOSEVKA_BYTES)?,
        })
    }

    /// The font that draws `face`. `Blank` draws nothing, so any font will do.
    pub fn get(&self, face: Face) -> &Font {
        match face {
            Face::Primary | Face::Blank => &self.primary,
            Face::Symbols => &self.symbols,
            Face::Iosevka => &self.iosevka,
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
    fn missing_everywhere_stays_primary() {
        let c = Coverage::new().unwrap();
        // U+2638 ☸ (kube-ps1's alternative symbol) needs an emoji font.
        for ch in ['\u{2638}', '\u{10fffd}'] {
            assert_eq!(c.face_for(ch), Face::Primary, "U+{:04X}", ch as u32);
        }
    }
}
