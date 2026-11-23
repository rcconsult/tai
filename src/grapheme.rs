//! How wide a grapheme cluster is, as Ghostty's `grapheme-width-method`
//! sets it (src/config/Config.zig and src/termio/Termio.zig at 108cacb1):
//!
//! - `unicode` turns on mode 2027 (grapheme clustering) at start: VS16
//!   makes a narrow symbol such as ☸ two cells wide, and an emoji ZWJ
//!   sequence is one two-cell cluster. This is Ghostty's default.
//! - `legacy`, tai's default, leaves it off: each codepoint is as wide as
//!   wcwidth says, as bash's readline, zsh's line editor and PSReadLine
//!   count it.
//!
//! tai doesn't follow Ghostty's default because those line editors keep
//! counting with wcwidth under 2027, so editing a line that holds such an
//! emoji puts the cursor in the wrong cell, and Backspace redraws the
//! wrong cells. Tests on Linux (bash) and on Windows (bash in WSL through
//! ConPTY) showed it; Ghostty has the same behaviour and leaves it to the
//! shells (ghostty-org/ghostty issue #2027). `TAI_GRAPHEME_WIDTH=unicode`
//! chooses Ghostty's way. A program can still turn the mode on itself
//! (`CSI ? 2027 h`), and a full reset (RIS) turns it off, since
//! libghostty-vt's C API has no default-modes option.
//!
//! Either way tai draws a cluster within the cells the terminal gives it
//! (`fonts::Coverage::cluster_face`).

/// Whether a terminal starts with mode 2027 on: `TAI_GRAPHEME_WIDTH`.
pub fn clustering() -> bool {
    clustering_for(std::env::var("TAI_GRAPHEME_WIDTH").ok().as_deref())
}

fn clustering_for(value: Option<&str>) -> bool {
    match value.map(str::trim) {
        Some("unicode") => true,
        None | Some("" | "legacy") => false,
        Some(other) => {
            tracing::warn!(
                "TAI_GRAPHEME_WIDTH={other:?}: expected unicode or legacy; using legacy"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libghostty_vt::{Terminal, mode::Mode};

    #[test]
    fn the_setting_reads_as_ghosttys() {
        assert!(clustering_for(Some("unicode")));
        assert!(!clustering_for(Some("legacy")));
        assert!(!clustering_for(None));
        assert!(!clustering_for(Some("")));
        assert!(!clustering_for(Some("Unicode")));
    }

    /// What the mode changes: ☸ (U+2638) is one cell, and with VS16 under
    /// mode 2027 it's two.
    #[test]
    fn clustering_widens_vs16() {
        let mut terminal = Terminal::new(80, 24).unwrap();
        terminal.vt_write("\u{2638}\u{fe0f}".as_bytes());
        assert_eq!(terminal.cursor_x().unwrap(), 1);
        terminal.set_mode(Mode::GRAPHEME_CLUSTER, true).unwrap();
        terminal.vt_write("\r\u{2638}\u{fe0f}".as_bytes());
        assert_eq!(terminal.cursor_x().unwrap(), 2);
    }
}
