//! App-level shortcuts: copy, paste and font size.
//!
//! Bindings follow Ghostty's defaults. On macOS they use Cmd; on Linux,
//! copy and paste need Ctrl+Shift so plain Ctrl+C and Ctrl+V still reach
//! the shell. A chord that maps to an [`Action`] is handled by tai and is
//! never sent to the PTY.

use libghostty_vt::{key::Mods, paste};
use macroquad::input::KeyCode;

/// Default font size in logical points.
pub const DEFAULT_FONT_SIZE: u16 = 10;
/// Smallest and largest font size the font-size shortcuts allow.
pub const MIN_FONT_SIZE: u16 = 8;
pub const MAX_FONT_SIZE: u16 = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Copy,
    Paste,
    FontBigger,
    FontSmaller,
    FontReset,
}

/// Map a key press to an app shortcut, if it is one.
///
/// `macos` selects the platform's bindings, so both sets can be tested on
/// any host.
pub fn action_for(key: KeyCode, mods: Mods, macos: bool) -> Option<Action> {
    let shift = mods.contains(Mods::SHIFT);
    let others = mods.difference(Mods::SHIFT);

    // The one modifier (besides Shift) every binding requires.
    let primary = if macos { Mods::SUPER } else { Mods::CTRL };
    if others != primary {
        return None;
    }

    match key {
        // On Linux, Ctrl+C and Ctrl+V without Shift belong to the shell.
        KeyCode::C if macos || shift => Some(Action::Copy),
        KeyCode::V if macos || shift => Some(Action::Paste),
        // "+" is Shift+= on most layouts, so accept "=" with or without Shift.
        KeyCode::Equal => Some(Action::FontBigger),
        KeyCode::Minus if !shift => Some(Action::FontSmaller),
        KeyCode::Key0 if !shift => Some(Action::FontReset),
        _ => None,
    }
}

/// The font size after a font-size action, clamped to the allowed range.
/// Other actions leave it unchanged.
pub fn next_font_size(current: u16, action: Action) -> u16 {
    match action {
        Action::FontBigger => current.saturating_add(1).min(MAX_FONT_SIZE),
        Action::FontSmaller => current.saturating_sub(1).max(MIN_FONT_SIZE),
        Action::FontReset => DEFAULT_FONT_SIZE,
        Action::Copy | Action::Paste => current,
    }
}

/// Encode clipboard text for writing to the PTY.
///
/// libghostty-vt does the work: it replaces unsafe control bytes (ESC
/// among them, which defuses an embedded `ESC[201~`) with spaces, and
/// either wraps the text in bracketed-paste markers or turns newlines
/// into carriage returns.
pub fn encode_paste(text: &str, bracketed: bool) -> libghostty_vt::error::Result<Vec<u8>> {
    let mut data = text.as_bytes().to_vec();
    // Room for the input plus the two 6-byte bracketed-paste markers.
    let mut buf = vec![0u8; data.len() + 12];
    let len = paste::encode(&mut data, bracketed, &mut buf)?;
    buf.truncate(len);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_cmd_chords() {
        let cmd = Mods::SUPER;
        assert_eq!(action_for(KeyCode::C, cmd, true), Some(Action::Copy));
        assert_eq!(action_for(KeyCode::V, cmd, true), Some(Action::Paste));
        assert_eq!(
            action_for(KeyCode::Equal, cmd, true),
            Some(Action::FontBigger)
        );
        assert_eq!(
            action_for(KeyCode::Equal, cmd | Mods::SHIFT, true),
            Some(Action::FontBigger)
        );
        assert_eq!(
            action_for(KeyCode::Minus, cmd, true),
            Some(Action::FontSmaller)
        );
        assert_eq!(
            action_for(KeyCode::Key0, cmd, true),
            Some(Action::FontReset)
        );
    }

    #[test]
    fn macos_ctrl_goes_to_the_shell() {
        assert_eq!(action_for(KeyCode::C, Mods::CTRL, true), None);
        assert_eq!(action_for(KeyCode::V, Mods::CTRL, true), None);
        assert_eq!(action_for(KeyCode::C, Mods::SUPER | Mods::CTRL, true), None);
        assert_eq!(action_for(KeyCode::C, Mods::SUPER | Mods::ALT, true), None);
    }

    #[test]
    fn linux_copy_paste_need_ctrl_shift() {
        let ctrl_shift = Mods::CTRL | Mods::SHIFT;
        assert_eq!(
            action_for(KeyCode::C, ctrl_shift, false),
            Some(Action::Copy)
        );
        assert_eq!(
            action_for(KeyCode::V, ctrl_shift, false),
            Some(Action::Paste)
        );
        // Plain Ctrl+C (SIGINT) and Ctrl+V (literal next) stay with the shell.
        assert_eq!(action_for(KeyCode::C, Mods::CTRL, false), None);
        assert_eq!(action_for(KeyCode::V, Mods::CTRL, false), None);
        // Super is not a Linux binding.
        assert_eq!(action_for(KeyCode::C, Mods::SUPER, false), None);
        assert_eq!(action_for(KeyCode::C, ctrl_shift | Mods::ALT, false), None);
    }

    #[test]
    fn linux_font_size() {
        let ctrl = Mods::CTRL;
        assert_eq!(
            action_for(KeyCode::Equal, ctrl, false),
            Some(Action::FontBigger)
        );
        assert_eq!(
            action_for(KeyCode::Equal, ctrl | Mods::SHIFT, false),
            Some(Action::FontBigger)
        );
        assert_eq!(
            action_for(KeyCode::Minus, ctrl, false),
            Some(Action::FontSmaller)
        );
        assert_eq!(
            action_for(KeyCode::Key0, ctrl, false),
            Some(Action::FontReset)
        );
        // Ctrl+Shift+- (Ctrl+_ ) and Ctrl+Shift+0 stay with the shell.
        assert_eq!(action_for(KeyCode::Minus, ctrl | Mods::SHIFT, false), None);
        assert_eq!(action_for(KeyCode::Key0, ctrl | Mods::SHIFT, false), None);
    }

    #[test]
    fn unmodified_keys_are_not_shortcuts() {
        for macos in [true, false] {
            assert_eq!(action_for(KeyCode::C, Mods::empty(), macos), None);
            assert_eq!(action_for(KeyCode::Equal, Mods::SHIFT, macos), None);
        }
    }

    #[test]
    fn font_size_steps_and_clamps() {
        assert_eq!(next_font_size(10, Action::FontBigger), 11);
        assert_eq!(next_font_size(10, Action::FontSmaller), 9);
        assert_eq!(
            next_font_size(MAX_FONT_SIZE, Action::FontBigger),
            MAX_FONT_SIZE
        );
        assert_eq!(
            next_font_size(MIN_FONT_SIZE, Action::FontSmaller),
            MIN_FONT_SIZE
        );
        assert_eq!(next_font_size(30, Action::FontReset), DEFAULT_FONT_SIZE);
        assert_eq!(next_font_size(30, Action::Copy), 30);
    }

    #[test]
    fn paste_unbracketed_turns_newlines_into_returns() {
        assert_eq!(encode_paste("ls\npwd\n", false).unwrap(), b"ls\rpwd\r");
    }

    #[test]
    fn paste_bracketed_wraps_and_keeps_newlines() {
        assert_eq!(
            encode_paste("a\nb", true).unwrap(),
            b"\x1b[200~a\nb\x1b[201~"
        );
    }

    #[test]
    fn paste_cannot_end_bracketed_mode_early() {
        let out = encode_paste("x\x1b[201~rm -rf ~\n", true).unwrap();
        // Only the real end marker may remain.
        let marker = b"\x1b[201~";
        let count = out.windows(marker.len()).filter(|w| w == marker).count();
        assert_eq!(count, 1);
        assert!(out.ends_with(marker));
    }

    #[test]
    fn paste_empty() {
        assert_eq!(encode_paste("", false).unwrap(), b"");
    }
}
