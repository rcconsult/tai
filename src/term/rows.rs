//! A terminal row into what to draw, for tai and tai-gui alike (ADR 0014,
//! step 4): each cell's background, the glyphs and their faces, Kitty
//! Unicode placeholder runs, and what the padding beside the row may take.
//! Each renderer turns a `Row` into its own draw data: tai a texel per
//! cell, tai-gui a quad, and both the glyph instances through `push_glyphs`.
//!
//! The two renderers used to read rows each their own way and differed in
//! what users saw: tai drew the cursor as a block over its cell, hiding the
//! character, where tai-gui draws the character in the background colour
//! on the cursor's colour, as Ghostty does; and only tai-gui extended the
//! padding in the edge cells' colours (Ghostty's window-padding-color =
//! extend), so a full-screen app's background reaches the window's edge.
//! Both now do what tai-gui did.

use libghostty_vt::{
    color::Rgb,
    render::{CellIterator, Colors, RowIteration},
    screen::{CellWide, RowSemanticPrompt},
};
use macroquad::color::Color;

use crate::{
    cells::{GlyphAtlas, Instance},
    fonts::{Coverage, Face, SymbolSpan},
    placeholders,
    term::PADDING,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// One cell's glyph: a grapheme cluster in one face.
#[derive(Clone, Debug, PartialEq)]
pub struct Glyph {
    pub col: u16,
    pub text: String,
    pub face: Face,
    pub fg: Rgb,
    pub bold: bool,
    /// How many cells it spans: 2 for a wide cell, or a narrow symbol from
    /// a fallback face that takes the blank cell after it (`SymbolSpan`).
    pub cells: u8,
}

/// One row, read.
#[derive(Clone, Debug, Default)]
pub struct Row {
    /// Each cell's background, `None` where the terminal's own shows.
    pub bg: Vec<Option<Rgb>>,
    /// Each cell's background including the default, after inverse but
    /// before the selection and the cursor: what the padding beside it may
    /// take.
    pub edge: Vec<Rgb>,
    /// The row's glyphs: the primary font's in cell order, then the
    /// fallback faces', so an icon that spreads into the next cell isn't
    /// covered by that cell's glyph.
    pub glyphs: Vec<Glyph>,
    /// The row's Kitty Unicode placeholder runs (src/placeholders.rs).
    pub virtual_runs: Vec<placeholders::Placement>,
    /// Whether the padding above or below may take this row's colours.
    pub extend: bool,
}

/// The cursor, when it's on the row being read: its column and colour.
#[derive(Clone, Copy, Debug)]
pub struct Cursor {
    pub col: u16,
    pub color: Rgb,
}

/// Read `row`'s cells. Colours: inverse and the selection swap foreground
/// and background; the cursor's cell takes the cursor colour, its glyph the
/// default background. Each glyph's face is the one `coverage` picks for
/// its whole cluster, trimmed to what tai draws without a shaper.
pub fn read_row<'alloc>(
    row: &RowIteration<'alloc, '_>,
    cell_it: &mut CellIterator<'alloc>,
    colors: &Colors,
    coverage: &Coverage,
    cursor: Option<Cursor>,
    text: &mut String,
) -> Result<Row> {
    let raw = row.raw_row()?;
    let prompt = !matches!(raw.semantic_prompt(), Ok(RowSemanticPrompt::None) | Err(_));
    let placeholder_row = raw.has_kitty_virtual_placeholder()?;
    let mut out = Row::default();
    let mut placeholder_cells = Vec::new();
    // Each cell's own background and first character, for `extends`.
    let mut extend_cells = Vec::new();
    let mut icons: Vec<Glyph> = Vec::new();
    // Whether the last cell drew a narrow fallback glyph, which may take
    // this cell too.
    let mut icon_open = false;
    let mut span = SymbolSpan::default();
    let mut cells = cell_it.update(row)?;
    let mut col: u16 = 0;
    while let Some(cell) = cells.next() {
        let graphemes = cell.graphemes_len()?;
        if graphemes > 0 {
            cell.graphemes_utf8(text)?;
        }
        let first = (graphemes > 0).then(|| text.chars().next()).flatten();
        // SymbolSpan sees every cell, in order.
        let grow = span.next(first);
        if std::mem::take(&mut icon_open)
            && grow
            && let Some(icon) = icons.last_mut()
        {
            icon.cells = 2;
        }
        let raw_bg = cell.bg_color()?;
        extend_cells.push((raw_bg, first));
        let mut fg = cell.fg_color()?.unwrap_or(colors.foreground);
        let mut has_bg = raw_bg.is_some();
        let mut bg = raw_bg.unwrap_or(colors.background);
        let mut bold = false;
        if graphemes > 0 && cell.has_styling()? {
            let style = cell.style()?;
            // Inverse (reverse video) swaps foreground and background.
            if style.inverse {
                std::mem::swap(&mut fg, &mut bg);
                has_bg = true;
            }
            bold = style.bold;
        }
        out.edge.push(bg);
        // Selected cells are drawn in reverse video.
        if cell.is_selected()? {
            std::mem::swap(&mut fg, &mut bg);
            has_bg = true;
        }
        if let Some(cursor) = cursor
            && cursor.col == col
        {
            bg = cursor.color;
            has_bg = true;
            fg = colors.background;
        }
        out.bg.push(has_bg.then_some(bg));

        // A placeholder cell shows part of an image, drawn with the images;
        // its own character has no glyph.
        let placeholder = if placeholder_row {
            let cell_placeholder = crate::term::images::placeholder_cell(cell)?;
            let is = cell_placeholder.is_some();
            placeholder_cells.push(cell_placeholder);
            is
        } else {
            false
        };
        if graphemes > 0 && !placeholder {
            let face = coverage.cluster_face(text);
            if face != Face::Blank && !text.is_empty() {
                let wide = cell.raw_cell()?.wide()? == CellWide::Wide;
                let glyph = Glyph {
                    col,
                    text: text.clone(),
                    face,
                    fg,
                    bold,
                    cells: if wide { 2 } else { 1 },
                };
                if face == Face::Primary {
                    out.glyphs.push(glyph);
                } else {
                    // A narrow fallback glyph may take the next cell too.
                    icon_open = !wide;
                    icons.push(glyph);
                }
            }
        }
        col += 1;
    }
    out.glyphs.append(&mut icons);
    out.extend = extends(prompt, &extend_cells, colors.background);
    if placeholder_row {
        out.virtual_runs = placeholders::row_placements(placeholder_cells);
    }
    Ok(out)
}

/// The glyph instances of a row whose top is at `y`, each glyph placed in
/// its face's size and rise (`Coverage::place`).
#[allow(clippy::too_many_arguments)]
pub fn push_glyphs(
    atlas: &mut GlyphAtlas,
    coverage: &Coverage,
    out: &mut Vec<Instance>,
    row: &Row,
    y: f32,
    cell_width: f32,
    baseline: f32,
    font_size: u16,
    dpi_scale: f32,
) {
    for glyph in &row.glyphs {
        let place = coverage.place(glyph.face, font_size, glyph.cells);
        atlas.push_text(
            out,
            coverage.font(glyph.face),
            glyph.face,
            &glyph.text,
            PADDING + f32::from(glyph.col) * cell_width,
            y + baseline - place.rise,
            place.font_size,
            dpi_scale,
            Color::from_rgba(glyph.fg.r, glyph.fg.g, glyph.fg.b, 255),
            glyph.bold,
        );
    }
}

/// The padding around the grid in the colours of the nearest cells, as
/// Ghostty's `window-padding-color = extend` draws it (src/renderer/
/// generic.zig and shaders.metal at 108cacb1): a full-screen app with its
/// own background, such as Helix, then fills the pane without a frame in
/// the terminal's default colour. The sides always take the edge cells'
/// colours; the top and bottom only when the nearest row allows it
/// (`extends`). Colours equal to the default `background` are left out, as
/// the pane is already that colour. Rectangles are x, y, width, height in
/// logical units from the pane's corner.
pub fn padding_rects(
    rows: &[impl AsRef<Row>],
    width: f32,
    height: f32,
    cell_width: f32,
    cell_height: f32,
    background: Rgb,
) -> Vec<([f32; 4], Rgb)> {
    let mut out = Vec::new();
    // One strip of the padding above or below a row, in runs of one colour.
    let strip = |edge: &[Rgb], y: f32, h: f32, out: &mut Vec<([f32; 4], Rgb)>| {
        let mut run: Option<(f32, Rgb)> = None;
        for (c, &color) in edge.iter().enumerate() {
            let start = if c == 0 {
                0.0
            } else {
                PADDING + c as f32 * cell_width
            };
            if let Some((x, held)) = run
                && held != color
            {
                out.push(([x, y, start - x, h], held));
                run = None;
            }
            run.get_or_insert((start, color));
        }
        if let Some((x, held)) = run {
            out.push(([x, y, width - x, h], held));
        }
    };
    let mut y = PADDING;
    for row in rows.iter().map(AsRef::as_ref) {
        if let (Some(&first), Some(&last)) = (row.edge.first(), row.edge.last()) {
            out.push(([0.0, y, PADDING, cell_height], first));
            let right = PADDING + row.edge.len() as f32 * cell_width;
            out.push(([right, y, width - right, cell_height], last));
        }
        y += cell_height;
    }
    if let Some(top) = rows.first().map(AsRef::as_ref).filter(|r| r.extend) {
        strip(&top.edge, 0.0, PADDING, &mut out);
    }
    if let Some(bottom) = rows.last().map(AsRef::as_ref).filter(|r| r.extend) {
        strip(&bottom.edge, y, height - y, &mut out);
    }
    out.retain(|(rect, color)| *color != background && rect[2] > 0.0 && rect[3] > 0.0);
    out
}

/// Whether the padding may take a row's colours (Ghostty's
/// `neverExtendBg`, src/renderer/row.zig): not a prompt row, and no cell
/// with the default background or a powerline glyph, which is drawn to fit
/// its cell exactly.
fn extends(prompt: bool, cells: &[(Option<Rgb>, Option<char>)], background: Rgb) -> bool {
    !prompt
        && cells.iter().all(|&(bg, first)| {
            bg.is_some_and(|bg| bg != background) && !first.is_some_and(is_powerline)
        })
}

fn is_powerline(c: char) -> bool {
    matches!(c as u32, 0xE0B0..=0xE0C8 | 0xE0CA | 0xE0CC..=0xE0D2 | 0xE0D4)
}

impl AsRef<Row> for Row {
    fn as_ref(&self) -> &Row {
        self
    }
}

#[cfg(test)]
mod tests {
    use libghostty_vt::color::Rgb;

    use super::{Cursor, Row, extends, padding_rects, read_row};
    use crate::{fonts::Face, term::PADDING};

    /// The first row after `line`, read with the cursor where it is.
    fn first_row(line: &[u8]) -> (Row, Rgb, Rgb) {
        use libghostty_vt::{
            Terminal,
            render::{CellIterator, RenderState, RowIterator},
        };
        let coverage = crate::fonts::Coverage::without_discovery().unwrap();
        let mut terminal = Terminal::new(10, 2).unwrap();
        terminal.vt_write(line);
        let mut state = RenderState::new().unwrap();
        let mut rows = RowIterator::new().unwrap();
        let mut cells = CellIterator::new().unwrap();
        let snapshot = state.update(&terminal).unwrap();
        let colors = snapshot.colors().unwrap();
        let cursor = snapshot
            .cursor_viewport()
            .unwrap()
            .filter(|vp| vp.y == 0)
            .map(|vp| Cursor {
                col: vp.x,
                color: Rgb { r: 9, g: 9, b: 9 },
            });
        let mut row_it = rows.update(&snapshot).unwrap();
        let row = row_it.next().unwrap();
        let mut text = String::new();
        let read = read_row(row, &mut cells, &colors, &coverage, cursor, &mut text).unwrap();
        (read, colors.foreground, colors.background)
    }

    /// Inverse swaps a cell's colours, and the cursor's cell takes the
    /// cursor colour with its glyph in the background colour, as Ghostty
    /// draws a block cursor; the rest keep the terminal's background.
    #[test]
    fn a_row_reads_inverse_and_the_cursor_into_its_cells() {
        // "ab", then an inverse "c", then the cursor back on the "a".
        let (row, fg, bg) = first_row(b"ab\x1b[7mc\x1b[0m\x1b[1G");
        let cursor = Rgb { r: 9, g: 9, b: 9 };
        assert_eq!(row.bg[..4], [Some(cursor), None, Some(fg), None]);
        let glyphs: Vec<_> = row
            .glyphs
            .iter()
            .map(|g| (g.col, g.text.as_str(), g.fg, g.face))
            .collect();
        assert_eq!(
            glyphs,
            [
                (0, "a", bg, Face::Primary),
                (1, "b", fg, Face::Primary),
                (2, "c", bg, Face::Primary),
            ]
        );
        // The padding takes the colours before the cursor's.
        assert_eq!(row.edge[..3], [bg, bg, fg]);
        assert!(!row.extend, "default cells keep the padding plain");
    }

    /// A full-screen app's background reaches the pane's edges (Ghostty's
    /// `window-padding-color = extend`); a shell's default one doesn't
    /// draw, and a row with a default cell keeps the top or bottom plain.
    #[test]
    fn the_padding_takes_the_edge_cells_colours() {
        let (width, height) = (2.0 * PADDING + 30.0 + 4.0, 2.0 * PADDING + 40.0 + 7.0);
        let rgb = |v| Rgb { r: v, g: v, b: v };
        let (bg, a, b) = (rgb(0), rgb(1), rgb(2));
        let row = |edge: [Rgb; 3], extend| Row {
            edge: edge.to_vec(),
            extend,
            ..Default::default()
        };
        let p = PADDING;
        let right = p + 30.0;
        // Top: a, a, b in two runs; bottom row has a default cell.
        let rows = [row([a, a, b], true), row([b, a, bg], false)];
        assert_eq!(
            padding_rects(&rows, width, height, 10.0, 20.0, bg),
            vec![
                ([0.0, p, p, 20.0], a),
                ([right, p, p + 4.0, 20.0], b),
                ([0.0, p + 20.0, p, 20.0], b),
                ([0.0, 0.0, p + 20.0, p], a),
                ([p + 20.0, 0.0, width - p - 20.0, p], b),
            ]
        );
        let shell = [row([bg, bg, bg], false), row([bg, bg, bg], false)];
        assert!(padding_rects(&shell, width, height, 10.0, 20.0, bg).is_empty());

        let black = Rgb { r: 0, g: 0, b: 0 };
        let blue = Some(Rgb { r: 0, g: 0, b: 9 });
        assert!(extends(false, &[(blue, None), (blue, Some('x'))], black));
        assert!(!extends(true, &[(blue, None)], black), "a prompt row");
        assert!(!extends(false, &[(blue, None), (None, None)], black));
        assert!(!extends(false, &[(Some(black), None)], black));
        assert!(!extends(false, &[(blue, Some('\u{e0b0}'))], black));
    }
}
