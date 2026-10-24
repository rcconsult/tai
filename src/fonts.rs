//! Bundled fonts and per-codepoint fallback.
//!
//! Text is drawn with JetBrains Mono. Codepoints it lacks, such as the Nerd
//! Font icons in shell prompts, fall back to Symbols Nerd Font Mono. Both
//! fonts are bundled (`fonts/README.md` records where they come from).
//! System-font discovery for emoji and CJK is left for later.

use macroquad::Error;
use macroquad::text::{Font, load_ttf_font_from_bytes};

pub const PRIMARY_BYTES: &[u8] = include_bytes!("../fonts/JetBrainsMono-Medium.ttf");
pub const FALLBACK_BYTES: &[u8] = include_bytes!("../fonts/SymbolsNerdFontMono-Regular.ttf");

/// Which bundled font draws a codepoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Face {
    Primary,
    Fallback,
}

/// Which codepoints each bundled font covers. This doesn't depend on the
/// font size, so it's built once. fontdue keeps a codepoint → glyph map, so
/// a lookup is a hash lookup with no rasterizing.
pub struct Coverage {
    primary: fontdue::Font,
    fallback: fontdue::Font,
    /// Fallback size as a fraction of the primary size, so its glyphs are
    /// one cell wide. Symbols Nerd Font Mono advances 1 em and JetBrains
    /// Mono 0.6 em, so this is 0.6.
    fallback_scale: f32,
    /// The largest fallback scale whose line still fits the primary's line.
    fallback_max_scale: f32,
    /// Vertical middle of each font's line, (ascent + descent) / 2, in em
    /// above the baseline. Used to centre fallback glyphs in the row.
    primary_mid: f32,
    fallback_mid: f32,
}

/// How to draw a glyph in a cell: the font size to draw at and how far to
/// raise the baseline, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub font_size: u16,
    pub rise: f32,
}

/// Glyphs used to measure each font's advance. Both fonts are monospaced.
const PRIMARY_PROBE: char = 'M';
const FALLBACK_PROBE: char = '\u{f113}';
const PROBE_PX: f32 = 100.0;

impl Coverage {
    pub fn new() -> Result<Self, &'static str> {
        let settings = fontdue::FontSettings::default();
        let primary = fontdue::Font::from_bytes(PRIMARY_BYTES, settings)?;
        let fallback = fontdue::Font::from_bytes(FALLBACK_BYTES, settings)?;
        let advance = |font: &fontdue::Font, ch| font.metrics(ch, PROBE_PX).advance_width;
        let lines = |font: &fontdue::Font| {
            font.horizontal_line_metrics(PROBE_PX)
                .ok_or("font has no horizontal line metrics")
        };
        let (p_lines, f_lines) = (lines(&primary)?, lines(&fallback)?);
        let mid = |m: fontdue::LineMetrics| (m.ascent + m.descent) / 2.0 / PROBE_PX;
        let height = |m: fontdue::LineMetrics| m.ascent - m.descent;
        Ok(Self {
            fallback_scale: advance(&primary, PRIMARY_PROBE) / advance(&fallback, FALLBACK_PROBE),
            fallback_max_scale: height(p_lines) / height(f_lines),
            primary_mid: mid(p_lines),
            fallback_mid: mid(f_lines),
            primary,
            fallback,
        })
    }

    /// The font to draw `ch` with. Prefer the primary font. Use the
    /// fallback only when it has the glyph and the primary doesn't. When
    /// neither has it, the primary draws its missing-glyph box.
    pub fn face_for(&self, ch: char) -> Face {
        if self.primary.lookup_glyph_index(ch) == 0 && self.fallback.lookup_glyph_index(ch) != 0 {
            Face::Fallback
        } else {
            Face::Primary
        }
    }

    /// Where to draw a glyph of `face` when the primary font is at
    /// `font_size`. Fallback glyphs are sized to span `cells` cells, but no
    /// taller than the primary's line, and centred on that line.
    pub fn place(&self, face: Face, font_size: u16, cells: u8) -> Placement {
        match face {
            Face::Primary => Placement {
                font_size,
                rise: 0.0,
            },
            Face::Fallback => {
                let scale = (self.fallback_scale * f32::from(cells)).min(self.fallback_max_scale);
                let size = (f32::from(font_size) * scale).round().max(1.0);
                Placement {
                    font_size: size as u16,
                    rise: self.primary_mid * f32::from(font_size) - self.fallback_mid * size,
                }
            }
        }
    }
}

/// The macroquad fonts used for drawing. Each one keeps a glyph atlas per
/// size it has drawn, so they're reloaded on font-size changes to drop the
/// old atlases.
pub struct Fonts {
    pub primary: Font,
    pub fallback: Font,
}

impl Fonts {
    pub fn load() -> Result<Self, Error> {
        Ok(Self {
            primary: load_ttf_font_from_bytes(PRIMARY_BYTES)?,
            fallback: load_ttf_font_from_bytes(FALLBACK_BYTES)?,
        })
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
    fn nerd_font_icons_fall_back() {
        let c = Coverage::new().unwrap();
        // U+F113 GitHub, U+E7AD devicon, U+F308 Linux logo, U+F0A0 hard disk.
        for ch in ['\u{f113}', '\u{e7ad}', '\u{f308}', '\u{f0a0}'] {
            assert_eq!(c.face_for(ch), Face::Fallback, "U+{:04X}", ch as u32);
        }
    }

    #[test]
    fn primary_keeps_glyphs_it_has() {
        let c = Coverage::new().unwrap();
        // JetBrains Mono ships the core powerline separators itself.
        assert_eq!(c.face_for('\u{e0b0}'), Face::Primary);
    }

    #[test]
    fn fallback_glyphs_fit_their_cells() {
        let c = Coverage::new().unwrap();
        let primary = Placement {
            font_size: 20,
            rise: 0.0,
        };
        assert_eq!(c.place(Face::Primary, 20, 1), primary);
        let p = c.place(Face::Fallback, 20, 1);
        // Cell width is 0.6 em of the primary font: 12 px at size 20.
        assert_eq!(p.font_size, 12);
        assert!(p.rise > 0.0, "{p:?}");
        // Two cells are 1.2 em, still shorter than the primary's 1.32 em line.
        assert_eq!(c.place(Face::Fallback, 20, 2).font_size, 24);
    }

    #[test]
    fn missing_everywhere_stays_primary() {
        let c = Coverage::new().unwrap();
        assert_eq!(c.face_for('\u{10fffd}'), Face::Primary);
    }
}
