//! Kitty graphics Unicode placeholders: virtual placements (ADR 0005,
//! decision 2).
//!
//! A program can place an image with `U=1` and then fill cells with
//! U+10EEEE. Each placeholder cell names its image by its foreground colour
//! (plus an optional third diacritic for the top 8 bits), its placement by
//! its underline colour, and its row and column in the image by two
//! diacritics. libghostty-vt stores the image and the virtual placement but
//! leaves resolving the cells to the renderer, as Ghostty's own app does.
//!
//! Ported from Ghostty's `src/terminal/kitty/graphics_unicode.zig`
//! (`PlacementIterator`, `IncompletePlacement`, `Placement.renderPlacement`,
//! `grid` and the diacritic table), ghostty-org/ghostty a887df42
//! "terminal: bound page map probe lengths without tombstones (#13294)".
//! The tests are Ghostty's, with the same expected numbers.

/// The Unicode placeholder character.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// A style colour as the protocol reads it: the raw value, not the colour
/// it resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdColor {
    None,
    Palette(u8),
    Rgb(u8, u8, u8),
}

impl IdColor {
    /// The 24-bit id a colour carries (`colorToId`).
    fn id(self) -> u32 {
        match self {
            IdColor::None => 0,
            IdColor::Palette(v) => u32::from(v),
            IdColor::Rgb(r, g, b) => u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b),
        }
    }
}

/// What a placeholder cell says about its image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderCell {
    /// The combining characters after U+10EEEE.
    pub diacritics: Vec<char>,
    pub fg: IdColor,
    pub underline: IdColor,
}

/// A run of placeholder cells in one row, showing one horizontal strip of
/// an image's grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// The run's first cell, as a column in its row.
    pub x: u16,
    pub image_id: u32,
    /// Zero means "the first virtual placement of the image".
    pub placement_id: u32,
    /// The run's first column and its row in the image's grid.
    pub col: u32,
    pub row: u32,
    /// The run's size in cells. Runs are always one row high.
    pub width: u32,
}

/// One cell's placement information before runs are joined. Row, column
/// and the high id byte may be missing and continue the run on the left.
#[derive(Debug, Clone, Copy)]
struct Incomplete {
    x: u16,
    image_id_low: u32,
    image_id_high: Option<u8>,
    placement_id: Option<u32>,
    row: Option<u32>,
    col: Option<u32>,
    width: u32,
}

impl Incomplete {
    fn new(x: u16, cell: &PlaceholderCell) -> Self {
        let placement_id = cell.underline.id();
        // Invalid diacritics count as missing, as in Kitty.
        let mut d = cell.diacritics.iter().map(|&c| diacritic_index(c));
        let row = d.next().flatten();
        let col = d.next().flatten();
        let image_id_high = d.next().flatten().and_then(|h| u8::try_from(h).ok());
        Self {
            x,
            image_id_low: cell.fg.id(),
            image_id_high,
            placement_id: (placement_id != 0).then_some(placement_id),
            row,
            col,
            width: 1,
        }
    }

    /// The first cell of a run: missing row and column count as zero.
    fn start(mut self) -> Self {
        self.row.get_or_insert(0);
        self.col.get_or_insert(0);
        self
    }

    /// Kitty's continuation rule (`canAppend`).
    fn can_append(&self, other: &Incomplete) -> bool {
        self.image_id_low == other.image_id_low
            && self.placement_id == other.placement_id
            && (other.row.is_none() || other.row == self.row)
            && (other.col.is_none() || other.col == self.col.map(|c| c + self.width))
            && (other.image_id_high.is_none() || other.image_id_high == self.image_id_high)
    }

    fn complete(&self) -> Placement {
        Placement {
            x: self.x,
            image_id: self.image_id_low | u32::from(self.image_id_high.unwrap_or(0)) << 24,
            placement_id: self.placement_id.unwrap_or(0),
            col: self.col.unwrap_or(0),
            row: self.row.unwrap_or(0),
            width: self.width,
        }
    }
}

/// Join one row's cells into placement runs. Each item is a cell: `Some`
/// for a U+10EEEE cell, `None` for anything else.
pub fn row_placements(cells: impl IntoIterator<Item = Option<PlaceholderCell>>) -> Vec<Placement> {
    let mut out = Vec::new();
    let mut run: Option<Incomplete> = None;
    for (x, cell) in (0u16..).zip(cells) {
        let Some(cell) = cell else {
            if let Some(prev) = run.take() {
                out.push(prev.complete());
            }
            continue;
        };
        let curr = Incomplete::new(x, &cell);
        match &mut run {
            Some(prev) if prev.can_append(&curr) => prev.width += 1,
            Some(prev) => {
                out.push(prev.complete());
                run = Some(curr.start());
            }
            None => run = Some(curr.start()),
        }
    }
    if let Some(prev) = run {
        out.push(prev.complete());
    }
    out
}

/// The image's grid in cells (`grid`): the virtual placement's `c`/`r`, or
/// enough cells to show the image at its own size where one is zero. This
/// stretches; the aspect ratio is kept in [`render`].
pub fn grid(
    columns: u32,
    rows: u32,
    img_w: u32,
    img_h: u32,
    cell_w: u32,
    cell_h: u32,
) -> (u32, u32) {
    let columns = if columns == 0 {
        img_w.div_ceil(cell_w.max(1))
    } else {
        columns
    };
    let rows = if rows == 0 {
        img_h.div_ceil(cell_h.max(1))
    } else {
        rows
    };
    (columns, rows)
}

/// Where to draw a run: offsets from its first cell and a destination size,
/// in the cell-size units passed to [`render`], and a source rectangle in
/// image pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Draw {
    pub offset_x: i64,
    pub offset_y: i64,
    pub source_x: i64,
    pub source_y: i64,
    pub source_width: i64,
    pub source_height: i64,
    pub dest_width: i64,
    pub dest_height: i64,
}

/// Fit the image into its grid, keeping its aspect ratio and centring it,
/// and work out the slice one run shows (`renderPlacement`). `None` when
/// the run lies in the padding around the image.
pub fn render(
    p: &Placement,
    (grid_cols, grid_rows): (u32, u32),
    img_w: u32,
    img_h: u32,
    cell_w: u32,
    cell_h: u32,
) -> Option<Draw> {
    let img_w = f64::from(img_w);
    let img_h = f64::from(img_h);

    // Fit the image into the grid, keeping its aspect ratio, centred.
    let rows_px = f64::from(grid_rows * cell_h);
    let cols_px = f64::from(grid_cols * cell_w);
    let (x_scale, y_scale, x_pad, y_pad) = if img_w * rows_px > img_h * cols_px {
        let s = cols_px / img_w.max(1.0);
        (s, s, 0.0, (rows_px - img_h * s) / 2.0)
    } else {
        let s = rows_px / img_h.max(1.0);
        (s, s, (cols_px - img_w * s) / 2.0, 0.0)
    };

    // The image with its padding, in image pixels.
    let scaled_x_off = x_pad / x_scale;
    let scaled_y_off = y_pad / y_scale;
    let scaled_w = img_w + scaled_x_off * 2.0;
    let scaled_h = img_h + scaled_y_off * 2.0;

    // The run's slice of the padded image.
    let (gc, gr) = (f64::from(grid_cols), f64::from(grid_rows));
    let mut src_w = scaled_w * (f64::from(p.width) / gc);
    let mut src_h = scaled_h * (1.0 / gr);
    let mut src_x = scaled_w * (f64::from(p.col) / gc);
    let mut src_y = scaled_h * (f64::from(p.row) / gr);

    let mut x_off = 0.0;
    let mut y_off = 0.0;
    let mut width = f64::from(p.width * cell_w);
    let mut height = f64::from(cell_h);

    // The texture has no padding, so clip the slice to the image and move
    // the destination instead.
    if src_y < scaled_y_off {
        let offset = scaled_y_off - src_y;
        src_h -= offset;
        y_off = offset;
        height -= offset * y_scale;
        src_y = 0.0;
        if src_h > img_h {
            src_h = img_h;
            height = img_h * y_scale;
        }
    } else if src_y + src_h > scaled_h - scaled_y_off {
        src_y -= scaled_y_off;
        src_h = scaled_h - scaled_y_off - src_y;
        src_h -= scaled_y_off;
        height = src_h * y_scale;
    } else {
        src_y -= scaled_y_off;
    }

    if src_x < scaled_x_off {
        let offset = scaled_x_off - src_x;
        src_w -= offset;
        x_off = offset;
        width -= offset * x_scale;
        src_x = 0.0;
        if src_w > img_w {
            src_w = img_w;
            width = img_w * x_scale;
        }
    } else if src_x + src_w > scaled_w - scaled_x_off {
        src_x -= scaled_x_off;
        src_w = scaled_w - scaled_x_off - src_x;
        src_w -= scaled_x_off;
        width = src_w * x_scale;
    } else {
        src_x -= scaled_x_off;
    }

    if src_w <= 0.0 || src_h <= 0.0 {
        return None;
    }
    // Zig's @round rounds halves away from zero, like f64::round.
    let r = |v: f64| v.round() as i64;
    Some(Draw {
        offset_x: r(x_off * x_scale),
        offset_y: r(y_off * y_scale),
        source_x: r(src_x),
        source_y: r(src_y),
        source_width: r(src_w),
        source_height: r(src_h),
        dest_width: r(width),
        dest_height: r(height),
    })
}

/// A diacritic's row or column value (its index in the table).
fn diacritic_index(c: char) -> Option<u32> {
    DIACRITICS.binary_search(&c).ok().map(|i| i as u32)
}

/// Kitty's row and column diacritics; a character's index is its value.
/// Generated from Ghostty's table (see the module header), which comes from
/// Kitty's rowcolumn-diacritics.txt.
#[rustfmt::skip]
const DIACRITICS: [char; 297] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
    '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}', '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}',
    '\u{05A8}', '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
    '\u{0659}', '\u{065A}', '\u{065B}', '\u{065D}', '\u{065E}', '\u{06D6}', '\u{06D7}', '\u{06D8}',
    '\u{06D9}', '\u{06DA}', '\u{06DB}', '\u{06DC}', '\u{06DF}', '\u{06E0}', '\u{06E1}', '\u{06E2}',
    '\u{06E4}', '\u{06E7}', '\u{06E8}', '\u{06EB}', '\u{06EC}', '\u{0730}', '\u{0732}', '\u{0733}',
    '\u{0735}', '\u{0736}', '\u{073A}', '\u{073D}', '\u{073F}', '\u{0740}', '\u{0741}', '\u{0743}',
    '\u{0745}', '\u{0747}', '\u{0749}', '\u{074A}', '\u{07EB}', '\u{07EC}', '\u{07ED}', '\u{07EE}',
    '\u{07EF}', '\u{07F0}', '\u{07F1}', '\u{07F3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}',
    '\u{081B}', '\u{081C}', '\u{081D}', '\u{081E}', '\u{081F}', '\u{0820}', '\u{0821}', '\u{0822}',
    '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082A}', '\u{082B}', '\u{082C}',
    '\u{082D}', '\u{0951}', '\u{0953}', '\u{0954}', '\u{0F82}', '\u{0F83}', '\u{0F86}', '\u{0F87}',
    '\u{135D}', '\u{135E}', '\u{135F}', '\u{17DD}', '\u{193A}', '\u{1A17}', '\u{1A75}', '\u{1A76}',
    '\u{1A77}', '\u{1A78}', '\u{1A79}', '\u{1A7A}', '\u{1A7B}', '\u{1A7C}', '\u{1B6B}', '\u{1B6D}',
    '\u{1B6E}', '\u{1B6F}', '\u{1B70}', '\u{1B71}', '\u{1B72}', '\u{1B73}', '\u{1CD0}', '\u{1CD1}',
    '\u{1CD2}', '\u{1CDA}', '\u{1CDB}', '\u{1CE0}', '\u{1DC0}', '\u{1DC1}', '\u{1DC3}', '\u{1DC4}',
    '\u{1DC5}', '\u{1DC6}', '\u{1DC7}', '\u{1DC8}', '\u{1DC9}', '\u{1DCB}', '\u{1DCC}', '\u{1DD1}',
    '\u{1DD2}', '\u{1DD3}', '\u{1DD4}', '\u{1DD5}', '\u{1DD6}', '\u{1DD7}', '\u{1DD8}', '\u{1DD9}',
    '\u{1DDA}', '\u{1DDB}', '\u{1DDC}', '\u{1DDD}', '\u{1DDE}', '\u{1DDF}', '\u{1DE0}', '\u{1DE1}',
    '\u{1DE2}', '\u{1DE3}', '\u{1DE4}', '\u{1DE5}', '\u{1DE6}', '\u{1DFE}', '\u{20D0}', '\u{20D1}',
    '\u{20D4}', '\u{20D5}', '\u{20D6}', '\u{20D7}', '\u{20DB}', '\u{20DC}', '\u{20E1}', '\u{20E7}',
    '\u{20E9}', '\u{20F0}', '\u{2CEF}', '\u{2CF0}', '\u{2CF1}', '\u{2DE0}', '\u{2DE1}', '\u{2DE2}',
    '\u{2DE3}', '\u{2DE4}', '\u{2DE5}', '\u{2DE6}', '\u{2DE7}', '\u{2DE8}', '\u{2DE9}', '\u{2DEA}',
    '\u{2DEB}', '\u{2DEC}', '\u{2DED}', '\u{2DEE}', '\u{2DEF}', '\u{2DF0}', '\u{2DF1}', '\u{2DF2}',
    '\u{2DF3}', '\u{2DF4}', '\u{2DF5}', '\u{2DF6}', '\u{2DF7}', '\u{2DF8}', '\u{2DF9}', '\u{2DFA}',
    '\u{2DFB}', '\u{2DFC}', '\u{2DFD}', '\u{2DFE}', '\u{2DFF}', '\u{A66F}', '\u{A67C}', '\u{A67D}',
    '\u{A6F0}', '\u{A6F1}', '\u{A8E0}', '\u{A8E1}', '\u{A8E2}', '\u{A8E3}', '\u{A8E4}', '\u{A8E5}',
    '\u{A8E6}', '\u{A8E7}', '\u{A8E8}', '\u{A8E9}', '\u{A8EA}', '\u{A8EB}', '\u{A8EC}', '\u{A8ED}',
    '\u{A8EE}', '\u{A8EF}', '\u{A8F0}', '\u{A8F1}', '\u{AAB0}', '\u{AAB2}', '\u{AAB3}', '\u{AAB7}',
    '\u{AAB8}', '\u{AABE}', '\u{AABF}', '\u{AAC1}', '\u{FE20}', '\u{FE21}', '\u{FE22}', '\u{FE23}',
    '\u{FE24}', '\u{FE25}', '\u{FE26}', '\u{10A0F}', '\u{10A38}', '\u{1D185}', '\u{1D186}', '\u{1D187}',
    '\u{1D188}', '\u{1D189}', '\u{1D1AA}', '\u{1D1AB}', '\u{1D1AC}', '\u{1D1AD}', '\u{1D242}', '\u{1D243}',
    '\u{1D244}',
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A row from cell strings: "-" is a non-placeholder cell, anything else
    /// a placeholder with those diacritics ("" for none).
    fn row(cells: &[&str], fg: IdColor, underline: IdColor) -> Vec<Option<PlaceholderCell>> {
        cells
            .iter()
            .map(|d| {
                (*d != "-").then(|| PlaceholderCell {
                    diacritics: d.chars().collect(),
                    fg,
                    underline,
                })
            })
            .collect()
    }

    fn p(x: u16, image_id: u32, placement_id: u32, row: u32, col: u32, width: u32) -> Placement {
        Placement {
            x,
            image_id,
            placement_id,
            col,
            row,
            width,
        }
    }

    const N: IdColor = IdColor::None;

    #[test]
    fn diacritics_sorted_and_indexed() {
        assert!(DIACRITICS.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(diacritic_index('\u{483}'), Some(30));
        assert_eq!(diacritic_index('\u{1d242}'), Some(294));
        assert_eq!(diacritic_index('a'), None);
    }

    #[test]
    fn none() {
        assert!(row_placements(row(&["-", "-"], N, N)).is_empty());
    }

    #[test]
    fn single_row_col() {
        assert_eq!(
            row_placements(row(&["\u{305}\u{305}"], N, N)),
            [p(0, 0, 0, 0, 0, 1)]
        );
    }

    #[test]
    fn continuation_break() {
        // Column 0, then column 2: two runs.
        assert_eq!(
            row_placements(row(&["\u{305}\u{305}", "\u{305}\u{30E}"], N, N)),
            [p(0, 0, 0, 0, 0, 1), p(1, 0, 0, 0, 2, 1)]
        );
    }

    #[test]
    fn continuation_with_diacritics_set() {
        assert_eq!(
            row_placements(row(
                &["\u{305}\u{305}", "\u{305}\u{30D}", "\u{305}\u{30E}"],
                N,
                N
            )),
            [p(0, 0, 0, 0, 0, 3)]
        );
    }

    #[test]
    fn continuation_with_no_col() {
        assert_eq!(
            row_placements(row(&["\u{305}\u{305}", "\u{305}", "\u{305}"], N, N)),
            [p(0, 0, 0, 0, 0, 3)]
        );
    }

    #[test]
    fn continuation_with_no_diacritics() {
        assert_eq!(
            row_placements(row(&["", "", ""], N, N)),
            [p(0, 0, 0, 0, 0, 3)]
        );
    }

    #[test]
    fn run_ending() {
        assert_eq!(
            row_placements(row(
                &["\u{305}\u{305}", "\u{305}\u{30D}", "-", "-", "-"],
                N,
                N
            )),
            [p(0, 0, 0, 0, 0, 2)]
        );
    }

    #[test]
    fn run_starting_in_the_middle() {
        assert_eq!(
            row_placements(row(
                &["-", "-", "-", "\u{305}\u{305}", "\u{305}\u{30D}"],
                N,
                N
            )),
            [p(3, 0, 0, 0, 0, 2)]
        );
    }

    #[test]
    fn image_id_as_palette() {
        let cells = row(&["\u{305}\u{305}"], IdColor::Palette(42), N);
        assert_eq!(row_placements(cells), [p(0, 42, 0, 0, 0, 1)]);
    }

    #[test]
    fn image_id_with_high_bits() {
        let cells = row(&["\u{305}\u{305}\u{30E}"], IdColor::Palette(42), N);
        assert_eq!(row_placements(cells), [p(0, 33554474, 0, 0, 0, 1)]);
    }

    #[test]
    fn placement_id_as_palette() {
        let cells = row(
            &["\u{305}\u{305}"],
            IdColor::Palette(42),
            IdColor::Palette(21),
        );
        assert_eq!(row_placements(cells), [p(0, 42, 21, 0, 0, 1)]);
    }

    #[test]
    fn image_id_as_rgb() {
        let cells = row(&["\u{305}\u{305}"], IdColor::Rgb(0, 1, 2), N);
        assert_eq!(row_placements(cells)[0].image_id, 0x000102);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(ox: i64, oy: i64, sx: i64, sy: i64, sw: i64, sh: i64, dw: i64, dh: i64) -> Draw {
        Draw {
            offset_x: ox,
            offset_y: oy,
            source_x: sx,
            source_y: sy,
            source_width: sw,
            source_height: sh,
            dest_width: dw,
            dest_height: dh,
        }
    }

    // Ghostty's "dog" cases: a 500x306 image, cells 36x80.
    #[test]
    fn render_dog_4x2() {
        let g = grid(4, 2, 500, 306, 36, 80);
        assert_eq!(
            render(&p(0, 1, 0, 0, 0, 4), g, 500, 306, 36, 80),
            Some(draw(0, 36, 0, 0, 500, 153, 144, 44))
        );
        assert_eq!(
            render(&p(0, 1, 0, 1, 0, 4), g, 500, 306, 36, 80),
            Some(draw(0, 0, 0, 153, 500, 153, 144, 44))
        );
    }

    #[test]
    fn render_dog_2x2_with_blank_cells() {
        let g = grid(2, 2, 500, 306, 36, 80);
        assert_eq!(
            render(&p(0, 1, 0, 0, 0, 4), g, 500, 306, 36, 80),
            Some(draw(0, 58, 0, 0, 500, 153, 72, 22))
        );
        assert_eq!(
            render(&p(0, 1, 0, 1, 0, 4), g, 500, 306, 36, 80),
            Some(draw(0, 0, 0, 153, 500, 153, 72, 22))
        );
    }

    #[test]
    fn render_dog_1x1() {
        let g = grid(1, 1, 500, 306, 36, 80);
        assert_eq!(
            render(&p(0, 1, 0, 0, 0, 4), g, 500, 306, 36, 80),
            Some(draw(0, 29, 0, 0, 500, 306, 36, 22))
        );
    }

    #[test]
    fn grid_defaults_to_image_size() {
        assert_eq!(grid(0, 0, 500, 306, 36, 80), (14, 4));
        assert_eq!(grid(3, 0, 500, 306, 36, 80), (3, 4));
    }
}
