//! Input methods and dead keys, for tai and tai-gui alike (ADR 0015): where
//! the preedit (the text being composed) sits on the cursor's row, and
//! where the input method's candidate window may open. The preedit itself,
//! and committing text, live with the keys (`keys.rs`), whose rules decide
//! when it starts and ends.

/// One grapheme cluster of the preedit, placed on the cursor's row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreeditCell {
    pub col: u16,
    pub text: String,
    /// 1, or 2 for a wide cluster.
    pub cells: u8,
}

/// Lay `preedit` out from the cursor's column `start` on a row of `cols`
/// cells, as Ghostty does (`renderer/State.zig` `Preedit.range` and
/// `generic.zig` at 108cacb1): clusters measured with libghostty-vt's
/// width table (ADR 0003 rule 5); a preedit that would run past the row's
/// end shifts left, and one wider than the row keeps only its end, where
/// the user is typing.
pub fn layout(preedit: &str, start: u16, cols: u16) -> Vec<PreeditCell> {
    if cols == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = preedit.chars().collect();
    let mut clusters: Vec<(String, u8)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (n, width) = libghostty_vt::unicode::grapheme_width(&chars[i..]);
        let n = n.max(1);
        let text: String = chars[i..(i + n).min(chars.len())].iter().collect();
        // A zero-width cluster still takes a cell, so it can be seen.
        clusters.push((text, width.clamp(1, 2)));
        i += n;
    }
    // Keep the end that fits in the row.
    let mut width: u16 = 0;
    let mut first = clusters.len();
    while first > 0 {
        let w = u16::from(clusters[first - 1].1);
        if width + w > cols {
            break;
        }
        width += w;
        first -= 1;
    }
    let start = start.min(cols - 1);
    let start = if start + width > cols {
        cols - width
    } else {
        start
    };
    let mut col = start;
    clusters[first..]
        .iter()
        .map(|(text, cells)| {
            let cell = PreeditCell {
                col,
                text: text.clone(),
                cells: *cells,
            };
            col += u16::from(*cells);
            cell
        })
        .collect()
}

/// Where the input method's candidate window opens: the cursor's cell, once
/// the cursor has stayed on it for two frames. TUIs that redraw a status
/// line move the cursor there and back within a frame or two; following
/// every move would drag the window around (wispterm `src/ime_caret.zig`,
/// `docs/lessons/from-libghostty-projects.md` lesson 2).
#[derive(Clone, Copy, Debug, Default)]
// Until each front end places its candidate window (ADR 0015, parts 2-6).
#[allow(dead_code)]
pub struct Caret {
    seen: Option<(u16, u16)>,
    frames: u8,
    placed: Option<(u16, u16)>,
}

#[allow(dead_code)]
impl Caret {
    /// This frame's cursor cell (column, row), or none when it's off the
    /// screen; returns the cell the candidate window should open at.
    pub fn update(&mut self, cursor: Option<(u16, u16)>) -> Option<(u16, u16)> {
        if cursor == self.seen {
            self.frames = self.frames.saturating_add(1);
        } else {
            self.seen = cursor;
            self.frames = 1;
        }
        if (self.frames >= 2 || self.placed.is_none())
            && let Some(cell) = self.seen
        {
            self.placed = Some(cell);
        }
        self.placed
    }
}

#[cfg(test)]
mod tests {
    use super::{Caret, PreeditCell, layout};

    fn cols(cells: &[PreeditCell]) -> Vec<(u16, &str, u8)> {
        cells
            .iter()
            .map(|c| (c.col, c.text.as_str(), c.cells))
            .collect()
    }

    /// Wide clusters take two cells, from the cursor's column.
    #[test]
    fn a_preedit_lays_out_from_the_cursor() {
        let cells = layout("にほn", 3, 80);
        assert_eq!(cols(&cells), vec![(3, "に", 2), (5, "ほ", 2), (7, "n", 1)]);
    }

    /// Near the row's end it shifts left; wider than the row, it keeps
    /// its end.
    #[test]
    fn a_preedit_stays_on_the_row() {
        assert_eq!(
            cols(&layout("abc", 9, 10)),
            vec![(7, "a", 1), (8, "b", 1), (9, "c", 1)]
        );
        assert_eq!(
            cols(&layout("にほんご", 0, 5)),
            vec![(0, "ん", 2), (2, "ご", 2)]
        );
        assert!(layout("a", 0, 0).is_empty());
    }

    /// The candidate window follows the cursor only once it has stayed on
    /// a cell for two frames; it starts where the cursor first is.
    #[test]
    fn the_caret_waits_for_the_cursor_to_settle() {
        let mut caret = Caret::default();
        assert_eq!(caret.update(Some((4, 2))), Some((4, 2)));
        // A status line redraw: the cursor visits the last row for a frame.
        assert_eq!(caret.update(Some((0, 23))), Some((4, 2)));
        assert_eq!(caret.update(Some((5, 2))), Some((4, 2)));
        assert_eq!(caret.update(Some((5, 2))), Some((5, 2)));
        // Off the screen: the window stays.
        assert_eq!(caret.update(None), Some((5, 2)));
        assert_eq!(caret.update(None), Some((5, 2)));
    }
}
