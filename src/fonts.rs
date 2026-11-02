//! Bundled fonts and per-codepoint fallback.
//!
//! Text is drawn with JetBrains Mono. Codepoints it lacks fall back, in
//! order, to Symbols Nerd Font Mono (the Nerd Font icons in shell prompts),
//! Iosevka Fixed (general symbols such as ⎈ and ⏵, and braille) and Noto
//! Emoji (monochrome emoji). All four fonts are bundled (`fonts/README.md`
//! records where they come from). Unicode spaces draw blank without a font.
//! A codepoint none of them has goes to the font the OS's own fallback
//! picks for the user's locale (`discovery.rs`), as Ghostty does, which is
//! how CJK is drawn. Colour emoji are left for later.
//!
//! Fonts are read in place with skrifa and each glyph's outline is filled
//! with zeno when the glyph is first drawn (ADR 0012). Nothing is parsed up
//! front: fontdue, which tai used before, built every glyph's outline when
//! it loaded a font, about 100 MB of heap for these four.

use std::{
    cell::{OnceCell, RefCell},
    collections::HashMap,
    path::Path,
};

use skrifa::{
    FontRef, GlyphId, MetadataProvider,
    instance::{LocationRef, Size},
    outline::{DrawSettings, OutlinePen},
};
use zeno::{Command, Mask, Point};

use crate::discovery::{self, FontFile};

pub const PRIMARY_BYTES: &[u8] = include_bytes!("../fonts/JetBrainsMono-Medium.ttf");
pub const SYMBOLS_BYTES: &[u8] = include_bytes!("../fonts/SymbolsNerdFontMono-Regular.ttf");
pub const IOSEVKA_BYTES: &[u8] = include_bytes!("../fonts/IosevkaFixedSS01-Medium.ttf");
pub const EMOJI_BYTES: &[u8] = include_bytes!("../fonts/NotoEmoji-Regular.ttf");

/// How a codepoint is drawn: with one of the bundled fonts, a font the OS
/// found, or not at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Face {
    Primary,
    Symbols,
    Iosevka,
    Emoji,
    /// A system font found for a codepoint the bundled fonts lack, by its
    /// slot in `Coverage`.
    Discovered(u8),
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

/// Whether `ch` is "symbol-like", as Ghostty's `is_symbol` decides
/// (`src/build/uucode_config.zig` at 521cf2c5): private use, or one of the
/// symbol and pictograph blocks. Only these may grow into a blank next cell.
pub fn is_symbol(ch: char) -> bool {
    matches!(
        u32::from(ch),
        0xe000..=0xf8ff | 0xf0000..=0xffffd | 0x100000..=0x10fffd // private use
            | 0x2190..=0x21ff // Arrows
            | 0x2460..=0x24ff // Enclosed Alphanumerics
            | 0x2600..=0x26ff // Miscellaneous Symbols
            | 0x2700..=0x27bf // Dingbats
            | 0x1f100..=0x1f1ff // Enclosed Alphanumeric Supplement
            | 0x1f300..=0x1f5ff // Miscellaneous Symbols and Pictographs
            | 0x1f600..=0x1f64f // Emoticons
            | 0x1f680..=0x1f6ff // Transport and Map Symbols
    )
}

/// Box drawing, block elements, legacy computing symbols and Powerline
/// glyphs: Ghostty's `isGraphicsElement` (`src/renderer/cell.zig`).
fn is_graphics_element(ch: char) -> bool {
    matches!(
        u32::from(ch),
        0x2500..=0x259f | 0x1fb00..=0x1fbff | 0x1cc00..=0x1cebf | 0xe0b0..=0xe0d7
    )
}

/// Ghostty's `constraintWidth` (`src/renderer/cell.zig` at 521cf2c5), fed
/// one row's cells from left to right. A narrow symbol spans two cells when
/// the next cell is empty or a space, unless it's in the last column or the
/// cell before it holds a symbol other than a graphics element, so a run of
/// icons stays one per cell. A wide cell always spans two: its next cell is
/// its empty spacer tail, so `next` says a wide symbol grows into it.
#[derive(Default)]
pub struct SymbolSpan {
    /// The last cell's first codepoint.
    prev: Option<char>,
    /// Whether the last cell is a narrow symbol that may take this one.
    open: bool,
}

impl SymbolSpan {
    /// Reads the next cell's first codepoint, `None` when it's empty.
    /// Returns whether the symbol in the cell before it spans two cells.
    pub fn next(&mut self, cp: Option<char>) -> bool {
        let grow =
            std::mem::take(&mut self.open) && cp.is_none_or(|ch| matches!(ch, ' ' | '\u{2002}'));
        self.open = cp.is_some_and(is_symbol)
            && !self
                .prev
                .is_some_and(|p| is_symbol(p) && !is_graphics_element(p));
        self.prev = cp;
        grow
    }
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

    /// Whether the font has an outline for `ch`, which tai can draw. A
    /// bitmap-only font, such as Apple Color Emoji (sbix), has glyphs but
    /// no outlines.
    pub fn has_outline(&self, ch: char) -> bool {
        self.font
            .charmap()
            .map(ch)
            .is_some_and(|glyph| self.font.outline_glyphs().get(glyph).is_some())
    }

    /// The metrics Ghostty sizes fallback fonts by, in em.
    fn size_metrics(&self) -> SizeMetrics {
        let em = |units: f32| units / self.units_per_em;
        let m = self.font.metrics(Size::unscaled(), LocationRef::default());
        let glyphs = self
            .font
            .glyph_metrics(Size::unscaled(), LocationRef::default());
        let charmap = self.font.charmap();
        // As Ghostty's FreeType face measures them (src/font/face/freetype.zig,
        // `getMetrics`): the widest advance and the inked height over the
        // printable ASCII range.
        let (mut cell_width, mut top, mut bottom) = (0.0f32, 0.0f32, 0.0f32);
        for glyph in (' '..='~').filter_map(|ch| charmap.map(ch)) {
            cell_width = cell_width.max(glyphs.advance_width(glyph).unwrap_or(0.0));
            if let Some(b) = glyphs.bounds(glyph) {
                top = top.max(b.y_max);
                bottom = bottom.min(b.y_min);
            }
        }
        // 水's advance, unless its ink is wider than that, which Ghostty
        // takes as a broken advance (a CJK font run through a patcher).
        let ic_width = charmap.map('水').and_then(|glyph| {
            let advance = glyphs.advance_width(glyph)?;
            let ink = glyphs.bounds(glyph).map_or(0.0, |b| b.x_max - b.x_min);
            (ink <= advance).then_some(em(advance))
        });
        SizeMetrics {
            ascent: em(m.ascent),
            descent: em(m.descent),
            line_gap: em(m.leading),
            cap_height: m.cap_height.map(em),
            ex_height: m.x_height.map(em),
            ascii_height: (top - bottom > 0.0).then_some(em(top - bottom)),
            cell_width: em(cell_width),
            ic_width,
        }
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

/// The face metrics Ghostty compares to size a fallback font, in em
/// (`src/font/Metrics.zig`, `FaceMetrics`, at a887df42). The optional ones
/// are what the font says; the methods fall back to Ghostty's estimates.
#[derive(Clone, Copy, Debug)]
struct SizeMetrics {
    ascent: f32,
    descent: f32,
    line_gap: f32,
    cap_height: Option<f32>,
    ex_height: Option<f32>,
    ascii_height: Option<f32>,
    cell_width: f32,
    ic_width: Option<f32>,
}

fn positive(value: Option<f32>) -> Option<f32> {
    value.filter(|v| *v > 0.0)
}

impl SizeMetrics {
    fn cap_height(&self) -> f32 {
        positive(self.cap_height).unwrap_or(0.75 * self.ascent)
    }

    fn ex_height(&self) -> f32 {
        positive(self.ex_height).unwrap_or(0.75 * self.cap_height())
    }

    fn ascii_height(&self) -> f32 {
        positive(self.ascii_height).unwrap_or(1.5 * self.cap_height())
    }

    fn ic_width(&self) -> f32 {
        positive(self.ic_width).unwrap_or(self.ascii_height().min(2.0 * self.cell_width))
    }

    fn line_height(&self) -> f32 {
        self.ascent - self.descent + self.line_gap
    }

    /// How much bigger than the primary to draw a fallback face, as
    /// Ghostty's `scaleFactor` with its default `.ic_width` adjustment
    /// (`src/font/Collection.zig`): match the ideograph width when the
    /// face has one, else the ex height, the cap height, then the line
    /// height, each only when the face states it.
    fn scale_from(primary: &Self, face: &Self) -> f32 {
        let (p, f) = if positive(face.ic_width).is_some() {
            (primary.ic_width(), face.ic_width())
        } else if positive(face.ex_height).is_some() {
            (primary.ex_height(), face.ex_height())
        } else if positive(face.cap_height).is_some() {
            (primary.cap_height(), face.cap_height())
        } else {
            (primary.line_height(), face.line_height())
        };
        if f > 0.0 { p / f } else { 1.0 }
    }
}

/// How a fallback font's glyphs are fitted to the primary font's cells.
struct Fit {
    /// Font size as a fraction of the primary size: per cell spanned for
    /// the bundled fonts, so one glyph is one cell wide; for the whole
    /// glyph for a discovered font.
    scale: f32,
    /// Whether `scale` is per cell.
    per_cell: bool,
    /// The largest scale whose line still fits the primary's line;
    /// unlimited for a discovered font.
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
        Self {
            scale: primary.advance(PRIMARY_PROBE, PROBE_PX) / font.advance(probe, PROBE_PX),
            per_cell: true,
            max_scale: line_height(primary) / line_height(font),
            mid: mid(font),
        }
    }

    /// A discovered font's fit: Ghostty's size normalization, centred on
    /// the primary's line like the bundled fonts. It isn't capped at the
    /// primary's line, as Ghostty's isn't (`Collection.zig` `scaleFactor`):
    /// CJK fonts have tall lines, and the cap shrank PingFang's 水 from
    /// 1.05x to 0.94x though its ink fits (ADR 0012, decision 5).
    fn normalized(primary_size: &SizeMetrics, font: &Typeface) -> Self {
        Self {
            scale: SizeMetrics::scale_from(primary_size, &font.size_metrics()),
            per_cell: false,
            max_scale: f32::INFINITY,
            mid: mid(font),
        }
    }
}

/// A font's ascent minus descent at `PROBE_PX`.
fn line_height(font: &Typeface) -> f32 {
    let (ascent, descent) = font.line_metrics(PROBE_PX);
    ascent - descent
}

/// A system font found for fallback.
struct Discovered {
    font: Typeface,
    fit: Fit,
}

/// How many system fonts fallback keeps. Each covers a script or more, so
/// a session rarely needs more than a few.
const MAX_DISCOVERED: usize = 16;

/// `path`'s bytes for the rest of the process. Mapped, not read: a CJK
/// collection is 13–80 MB and only the pages of glyphs drawn are touched.
/// Leaked, because a discovered face is kept until tai exits and there are
/// at most `MAX_DISCOVERED` of them.
#[allow(unsafe_code)]
pub fn map(path: &Path) -> std::io::Result<&'static [u8]> {
    let file = std::fs::File::open(path)?;
    // SAFETY: a mapped file that another process truncates can fault on
    // read. System font files don't change while they're installed, and
    // DirectWrite, CoreText and FreeType map them the same way.
    let mapped: &'static memmap2::Mmap = Box::leak(Box::new(unsafe { memmap2::Mmap::map(&file)? }));
    Ok(&mapped[..])
}

/// The bundled fonts and the system fonts found so far, which codepoints
/// each covers, and how each fits the primary's cells. Covering a
/// codepoint is a lookup in the font's own character map.
pub struct Coverage {
    primary: Typeface,
    symbols: Typeface,
    iosevka: Typeface,
    emoji: Typeface,
    primary_mid: f32,
    primary_size: SizeMetrics,
    symbols_fit: Fit,
    iosevka_fit: Fit,
    emoji_fit: Fit,
    /// System fonts found for fallback, filled in order. A face never
    /// moves once found, so `font` can hand out references to it.
    discovered: [OnceCell<Discovered>; MAX_DISCOVERED],
    /// Each codepoint asked about past the bundled fonts, and the slot that
    /// draws it, `None` included, so the OS is asked once per codepoint.
    found: RefCell<HashMap<char, Option<u8>>>,
    /// Font files the OS has named, and their slot (`None` when unusable).
    files: RefCell<HashMap<FontFile, Option<u8>>>,
    /// Asks the OS; `discovery::find` outside tests.
    find: fn(char) -> Option<FontFile>,
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
        Self::with_discovery(discovery::find)
    }

    /// The bundled fonts only: the OS is never asked, so tests outside this
    /// module see the same faces on every platform.
    #[cfg(test)]
    pub fn without_discovery() -> Result<Self, &'static str> {
        Self::with_discovery(|_| None)
    }

    fn with_discovery(find: fn(char) -> Option<FontFile>) -> Result<Self, &'static str> {
        let primary = Typeface::new(PRIMARY_BYTES, 0)?;
        let symbols = Typeface::new(SYMBOLS_BYTES, 0)?;
        let iosevka = Typeface::new(IOSEVKA_BYTES, 0)?;
        let emoji = Typeface::new(EMOJI_BYTES, 0)?;
        Ok(Self {
            primary_mid: mid(&primary),
            primary_size: primary.size_metrics(),
            symbols_fit: Fit::new(&primary, &symbols, SYMBOLS_PROBE),
            iosevka_fit: Fit::new(&primary, &iosevka, IOSEVKA_PROBE),
            emoji_fit: Fit::new(&primary, &emoji, EMOJI_PROBE),
            primary,
            symbols,
            iosevka,
            emoji,
            discovered: Default::default(),
            found: RefCell::default(),
            files: RefCell::default(),
            find,
        })
    }

    /// The discovered face for `ch`, asking the OS the first time.
    fn discovered(&self, ch: char) -> Option<u8> {
        if let Some(slot) = self.found.borrow().get(&ch) {
            return *slot;
        }
        let slot = self.discover(ch);
        self.found.borrow_mut().insert(ch, slot);
        slot
    }

    fn discover(&self, ch: char) -> Option<u8> {
        // A face found for another codepoint may draw this one too; Ghostty
        // also checks its loaded faces before discovery
        // (`CodepointResolver.getIndex`).
        let loaded = self.discovered.iter().map_while(OnceCell::get);
        if let Some(slot) = loaded.clone().position(|d| d.font.has_outline(ch)) {
            return Some(slot as u8);
        }
        let file = (self.find)(ch)?;
        if self.files.borrow().contains_key(&file) {
            // Loaded already, and it lacks `ch` after all, or unusable.
            return None;
        }
        let slot = self.load(&file, ch, loaded.count());
        self.files.borrow_mut().insert(file, slot);
        slot
    }

    /// Load `file` into slot `next` if it can draw `ch`.
    fn load(&self, file: &FontFile, ch: char, next: usize) -> Option<u8> {
        let path = file.path.display();
        let Some(cell) = self.discovered.get(next) else {
            tracing::warn!("font fallback: {MAX_DISCOVERED} fonts in use; {path} not loaded");
            return None;
        };
        let bytes = map(&file.path)
            .inspect_err(|e| tracing::warn!("font fallback: {path}: {e}"))
            .ok()?;
        let font = Typeface::new(bytes, file.index)
            .inspect_err(|e| tracing::warn!("font fallback: {path} #{}: {e}", file.index))
            .ok()?;
        if !font.has_outline(ch) {
            tracing::info!(
                "font fallback: {path} #{} has no outline for U+{:04X}",
                file.index,
                ch as u32
            );
            return None;
        }
        let fit = Fit::normalized(&self.primary_size, &font);
        tracing::info!(
            "font fallback: U+{:04X} from {path} #{}, at {:.2}x",
            ch as u32,
            file.index,
            fit.scale
        );
        let _ = cell.set(Discovered { font, fit });
        Some(next as u8)
    }

    /// How to draw `ch`. Unicode spaces are blank. Otherwise the first font
    /// in the chain that has a glyph for it: primary, then Symbols, then
    /// Iosevka, then Noto Emoji, then a system font the OS picks. When none
    /// has it, the primary draws its missing-glyph box.
    pub fn face_for(&self, ch: char) -> Face {
        if is_space(ch) {
            Face::Blank
        } else {
            self.find_face(ch).unwrap_or(Face::Primary)
        }
    }

    /// The first font in the chain with a glyph for `ch`, `None` when no
    /// font has one.
    fn find_face(&self, ch: char) -> Option<Face> {
        if self.primary.has(ch) {
            Some(Face::Primary)
        } else if self.symbols.has(ch) {
            Some(Face::Symbols)
        } else if self.iosevka.has(ch) {
            Some(Face::Iosevka)
        } else if self.emoji.has(ch) {
            Some(Face::Emoji)
        } else {
            self.discovered(ch).map(Face::Discovered)
        }
    }

    /// Whether `face` has a glyph for `ch` that tai can draw.
    fn covers(&self, face: Face, ch: char) -> bool {
        match face {
            Face::Blank => false,
            Face::Discovered(_) => self.font(face).has_outline(ch),
            _ => self.font(face).has(ch),
        }
    }

    /// The face for a cell's grapheme cluster, `text`, which is trimmed in
    /// place to the codepoints tai draws.
    ///
    /// One face draws the whole cluster: the first of the faces for its
    /// codepoints that has all of them, ignoring VS15, VS16 and ZWJ, as
    /// Ghostty's `RunIterator.indexForCell` (`src/font/shaper/run.zig` at
    /// a887df42 "terminal: bound page map probe lengths without tombstones
    /// (#13294)") chooses. When none has them all, the cell draws U+FFFD,
    /// as Ghostty's does.
    ///
    /// tai has no shaper, so of the cluster it draws the first codepoint
    /// and the zero-advance marks right after it, such as U+0301 on e,
    /// laid out one after another as fonts design marks to be when nothing
    /// positions them (a mark's ink lies left of its origin). It stops at
    /// the first codepoint that would advance or joins: the rest of an
    /// emoji ZWJ sequence, a skin tone, a flag's second letter, a keycap's
    /// frame, a spacing mark. Only a ligature draws those within the
    /// cluster's cells, so the cluster never draws past the columns the
    /// terminal gave it. When that leaves the base alone, it's drawn in its
    /// own face. VS15 and VS16 are dropped: no font draws them (Ghostty's
    /// `run.zig` doesn't send them to the shaper either).
    ///
    /// A single codepoint, the common case, is just `face_for`.
    pub fn cluster_face(&self, text: &mut String) -> Face {
        let mut chars = text.chars();
        let Some(base) = chars.next() else {
            return Face::Blank;
        };
        if chars.next().is_none() {
            return self.face_for(base);
        }
        text.retain(|ch| !matches!(ch, '\u{fe0e}' | '\u{fe0f}'));
        if text.chars().nth(1).is_none() || is_space(base) {
            return self.face_for(base);
        }
        let rest = || text.chars().skip(1).filter(|&ch| ch != '\u{200d}');
        let candidates: Option<Vec<Face>> = std::iter::once(base)
            .chain(rest())
            .map(|ch| self.find_face(ch))
            .collect();
        let face = candidates.and_then(|faces| {
            faces
                .into_iter()
                .find(|&face| self.covers(face, base) && rest().all(|ch| self.covers(face, ch)))
        });
        let Some(face) = face else {
            text.clear();
            text.push('\u{fffd}');
            return self.face_for('\u{fffd}');
        };
        let font = self.font(face);
        let end = text
            .char_indices()
            .skip(1)
            .find(|&(_, ch)| ch == '\u{200d}' || font.advance(ch, PROBE_PX) != 0.0)
            .map(|(at, _)| at);
        if let Some(end) = end {
            text.truncate(end);
        }
        if text.len() == base.len_utf8() {
            // Only the base is left, so it's drawn in its own face: Noto
            // Emoji has the 1 of 1️⃣ only as a small keycap digit.
            return self.face_for(base);
        }
        face
    }

    /// The font that draws `face`. `Blank` draws nothing, so any font will
    /// do.
    pub fn font(&self, face: Face) -> &Typeface {
        match face {
            Face::Primary | Face::Blank => &self.primary,
            Face::Symbols => &self.symbols,
            Face::Iosevka => &self.iosevka,
            Face::Emoji => &self.emoji,
            Face::Discovered(slot) => self
                .discovered_face(slot)
                .map_or(&self.primary, |d| &d.font),
        }
    }

    fn discovered_face(&self, slot: u8) -> Option<&Discovered> {
        self.discovered.get(usize::from(slot))?.get()
    }

    /// Where to draw a glyph of `face` when the primary font is at
    /// `font_size`. Bundled fallback glyphs are sized to span `cells` cells,
    /// no taller than the primary's line; discovered ones by Ghostty's size
    /// normalization. Either way they're centred on the primary's line.
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
            Face::Discovered(slot) => match self.discovered_face(slot) {
                Some(d) => &d.fit,
                None => return self.place(Face::Primary, font_size, cells),
            },
        };
        let per_cell = if fit.per_cell { f32::from(cells) } else { 1.0 };
        let scale = (fit.scale * per_cell).min(fit.max_scale);
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

    /// The columns whose narrow symbol spans two cells, for a row of narrow
    /// cells given as text, `_` standing for an empty cell.
    fn grown(row: &str) -> Vec<usize> {
        let mut span = SymbolSpan::default();
        let mut out = Vec::new();
        for (i, ch) in row.chars().enumerate() {
            if span.next((ch != '_').then_some(ch)) {
                out.push(i - 1);
            }
        }
        out
    }

    #[test]
    fn a_narrow_symbol_spans_two_cells_before_a_space_or_empty_cell() {
        // ☸ (Miscellaneous Symbols) and a Nerd Font icon (private use).
        assert_eq!(grown("☸ helm"), [0]);
        assert_eq!(grown("x\u{f113}_y"), [1]);
        assert_eq!(grown("☸\u{2002}x"), [0]);
    }

    #[test]
    fn a_narrow_symbol_stays_one_cell_otherwise() {
        // Before a letter, or before a space Ghostty doesn't count.
        assert!(grown("☸helm").is_empty());
        assert!(grown("☸\u{a0}x").is_empty());
        // Not a symbol: ∀ is Mathematical Operators, é a letter.
        assert!(grown("∀ x").is_empty());
        assert!(grown("é x").is_empty());
        // The last column has no next cell.
        assert!(grown("ab☸").is_empty());
    }

    #[test]
    fn a_symbol_after_a_symbol_stays_one_cell() {
        // So a run of icons keeps one per cell; the first may still grow.
        assert!(grown("☸☸ x").is_empty());
        assert_eq!(grown("☸ ☸ x"), [0, 2]);
        // A graphics element before it doesn't count: Powerline, a block.
        assert_eq!(grown("\u{e0b0}\u{f113} x"), [1]);
        assert_eq!(grown("█☸ x"), [1]);
    }

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

    static ASKED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Names Iosevka's file for every codepoint, and counts the calls.
    fn iosevka_file(_ch: char) -> Option<FontFile> {
        ASKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(FontFile {
            path: concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/fonts/IosevkaFixedSS01-Medium.ttf"
            )
            .into(),
            index: 0,
        })
    }

    #[test]
    fn discovery_is_asked_once_per_codepoint() {
        use std::sync::atomic::Ordering::Relaxed;
        let c = Coverage::with_discovery(iosevka_file).unwrap();
        let before = ASKED.load(Relaxed);
        // The file named lacks U+10FFFD: no face, and remembered as such.
        assert_eq!(c.face_for('\u{10fffd}'), Face::Primary);
        assert_eq!(c.face_for('\u{10fffd}'), Face::Primary);
        assert_eq!(ASKED.load(Relaxed) - before, 1);
        // The bundled fonts come first: ⎈ never reaches discovery.
        assert_eq!(c.face_for('\u{2388}'), Face::Iosevka);
        assert_eq!(ASKED.load(Relaxed) - before, 1);
    }

    /// The OS's fallback draws CJK, two cells wide by Ghostty's ideograph
    /// width, centred on the primary's line. Needs the OS's CJK fonts.
    #[cfg(windows)]
    #[test]
    fn cjk_comes_from_the_system() {
        let c = Coverage::new().unwrap();
        let face = c.face_for('水');
        assert_eq!(face, Face::Discovered(0));
        // A second Han character comes from the face already found.
        assert_eq!(c.face_for('木'), face);
        let font = c.font(face);
        let place = c.place(face, 20, 2);
        let px = f32::from(place.font_size);
        // JetBrains Mono's estimated ideograph width is two 0.6 em cells.
        let width = font.advance('水', px);
        eprintln!("水 at {place:?}: advance {width} px against 24 px for two cells");
        assert!((20.0..=24.5).contains(&width), "{width}");
        // The cell count doesn't change a discovered face's size.
        assert_eq!(c.place(face, 20, 1), place);
    }

    /// `text`'s face as a cell's cluster, and what's left of it to draw.
    /// The OS isn't asked, so the answer doesn't depend on the host.
    fn cluster(text: &str) -> (Face, String) {
        let c = Coverage::with_discovery(|_| None).unwrap();
        let mut text = text.to_string();
        (c.cluster_face(&mut text), text)
    }

    #[test]
    fn a_single_codepoint_is_its_own_face() {
        assert_eq!(cluster("A"), (Face::Primary, "A".into()));
        assert_eq!(cluster("\u{e9}"), (Face::Primary, "\u{e9}".into()));
        assert_eq!(cluster(" "), (Face::Blank, " ".into()));
    }

    #[test]
    fn a_decomposed_accent_keeps_its_mark() {
        // é as e + U+0301: JetBrains Mono has both.
        assert_eq!(cluster("e\u{301}"), (Face::Primary, "e\u{301}".into()));
        // U+0305 COMBINING OVERLINE is only in Iosevka, which has e too, so
        // Iosevka draws the whole cluster (Ghostty's indexForCell).
        assert_eq!(cluster("e\u{305}"), (Face::Iosevka, "e\u{305}".into()));
    }

    #[test]
    fn a_cluster_no_font_can_draw_shows_the_replacement_character() {
        // U+1AF1 is in none of the bundled fonts.
        assert_eq!(cluster("e\u{1af1}"), (Face::Primary, "\u{fffd}".into()));
    }

    #[test]
    fn a_mark_is_laid_out_over_its_base() {
        let c = Coverage::with_discovery(|_| None).unwrap();
        let font = c.font(Face::Primary);
        let e = font.metrics('e', PROBE_PX);
        let acute = font.metrics('\u{301}', PROBE_PX);
        assert_eq!(font.advance('\u{301}', PROBE_PX), 0.0);
        // Drawn at the pen after e, the accent's ink lands over e's.
        let ink = e.advance_width + acute.xmin as f32;
        assert!(ink >= e.xmin as f32 && ink + acute.width as f32 <= e.advance_width);
    }

    #[test]
    fn emoji_sequences_draw_their_first_emoji() {
        // Only a ligature fits these in their cells, and tai doesn't shape.
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        assert_eq!(cluster(family), (Face::Emoji, "\u{1f468}".into()));
        let thumbs_medium = "\u{1f44d}\u{1f3fd}";
        assert_eq!(cluster(thumbs_medium), (Face::Emoji, "\u{1f44d}".into()));
        let flag_us = "\u{1f1fa}\u{1f1f8}";
        assert_eq!(cluster(flag_us), (Face::Emoji, "\u{1f1fa}".into()));
        // Noto Emoji has 1 and the keycap frame, but the frame advances,
        // so only the 1 is drawn, in the primary rather than as Noto
        // Emoji's small keycap digit.
        assert_eq!(cluster("1\u{fe0f}\u{20e3}"), (Face::Primary, "1".into()));
    }

    #[test]
    fn variation_selectors_are_dropped() {
        // ☸ with VS16 (emoji) and VS15 (text); Noto Emoji is the only
        // bundled font with it. ❤ is in Iosevka, which comes first.
        assert_eq!(
            cluster("\u{2638}\u{fe0f}"),
            (Face::Emoji, "\u{2638}".into())
        );
        assert_eq!(
            cluster("\u{2638}\u{fe0e}"),
            (Face::Emoji, "\u{2638}".into())
        );
        assert_eq!(
            cluster("\u{2764}\u{fe0f}"),
            (Face::Iosevka, "\u{2764}".into())
        );
    }

    #[test]
    fn what_a_cluster_draws_after_its_base_never_advances() {
        let c = Coverage::with_discovery(|_| None).unwrap();
        let clusters = [
            "e\u{301}\u{323}",
            "a\u{305}\u{331}",
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}",
            "\u{1f44d}\u{1f3fb}",
            "\u{1f1ef}\u{1f1f5}",
            "1\u{fe0f}\u{20e3}",
            "\u{1f3f3}\u{fe0f}\u{200d}\u{1f308}",
        ];
        for text in clusters {
            let mut drawn = text.to_string();
            let face = c.cluster_face(&mut drawn);
            let font = c.font(face);
            let rest: f32 = drawn
                .chars()
                .skip(1)
                .map(|ch| font.advance(ch, PROBE_PX))
                .sum();
            assert_eq!(rest, 0.0, "{text:?} drew {drawn:?} in {face:?}");
        }
    }
}
