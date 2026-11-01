//! The selection as Copy takes it, and the badge's summary of it. Shared by
//! tai and tai-gui.

use libghostty_vt::{Terminal, fmt::Format, selection::FormatOptions};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The selection as Copy puts it on the clipboard, the way Ghostty does:
/// plain text, soft-wrapped lines joined, trailing blanks trimmed. `None`
/// without a selection.
pub fn text(terminal: &Terminal<'_, '_>) -> Result<Option<String>> {
    let options = FormatOptions::new()
        .with_emit_format(Format::Plain)
        .with_unwrap(true)
        .with_trim(true);
    Ok(terminal
        .format_selection_alloc(None, options)?
        .filter(|text| !text.is_empty())
        .map(|text| String::from_utf8_lossy(&text).into_owned()))
}

/// The selection badge's text: lines and characters of what Copy would put
/// on the clipboard. Lines are logical (a soft-wrapped line counts once) and
/// characters are grapheme clusters (👨‍👩‍👧 is one), line breaks included. ASCII
/// only, so drawing it never adds a glyph to the font atlas mid-frame.
pub fn summary(text: &str) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let lines = text.lines().count().max(1);
    let chars = text.graphemes(true).count();
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    format!("{}, {}", plural(lines, "line"), plural(chars, "char"))
}

#[cfg(test)]
mod tests {
    use super::summary;

    #[test]
    fn selection_summary_counts_lines_and_graphemes() {
        assert_eq!(summary("ls"), "1 line, 2 chars");
        // Lines are what Copy gives: three lines, line breaks counted.
        assert_eq!(summary("a\nbc\nd"), "3 lines, 6 chars");
        // A family emoji is one character, not five code points.
        assert_eq!(
            summary("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"),
            "1 line, 1 char"
        );
        // Text from a Windows program may end lines with CR LF: one break.
        assert_eq!(summary("a\r\nb"), "2 lines, 3 chars");
    }
}
