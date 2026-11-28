//! Keys to the program, for tai and tai-gui alike (ADR 0014, step 5): each
//! front end turns its toolkit's key into a `KeyIn` (the libghostty key,
//! its unshifted character, the modifiers and the text it typed) and
//! `Keys::encode` writes what the program gets, through libghostty-vt's
//! encoder, so the Kitty keyboard protocol, application cursor keys and
//! the rest follow the terminal's modes in both apps.
//!
//! The rules here were tai's (`flush_pending_key` in src/main.rs); tai-gui
//! had only some of them: AltGr's character on Windows, a Ctrl chord's own
//! character dropped, the text sent when the encoder has no sequence, the
//! C0 byte for Ctrl+[ and its kind, and releases and repeats for the
//! protocols that report them.

use libghostty_vt::{
    Terminal,
    key::{self, Key},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// One key, as the front end saw it.
#[derive(Clone, Copy, Debug)]
pub struct KeyIn<'a> {
    pub action: key::Action,
    pub key: Key,
    /// The character the key gives with no modifiers, '\0' for none.
    pub ucp: char,
    pub mods: key::Mods,
    /// The text the platform produced for it, if any.
    pub text: Option<&'a str>,
}

/// The encoder and its event, kept between keys.
pub struct Keys {
    encoder: key::Encoder<'static>,
    event: key::Event<'static>,
}

impl Keys {
    pub fn new() -> Result<Self> {
        Ok(Self {
            encoder: key::Encoder::new()?,
            event: key::Event::new()?,
        })
    }

    /// Append what the program gets for `k` to `out`.
    pub fn encode(
        &mut self,
        terminal: &Terminal<'_, '_>,
        k: KeyIn<'_>,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        // Windows reports AltGr as Ctrl+Alt. When it produced a character
        // (AltGr+2 gives '@' on Swiss and German layouts), that character
        // is the input: with Ctrl+Alt left in, the encoder sends ESC NUL or
        // a CSI u sequence instead. Marking them consumed isn't enough (it
        // sends NUL), so drop them.
        let altgr = key::Mods::CTRL | key::Mods::ALT;
        let mods = if cfg!(windows) && k.text.is_some() && k.mods.contains(altgr) {
            k.mods - altgr
        } else {
            k.mods
        };

        // A Ctrl chord's own character as text ('[' for Ctrl+[ on X11)
        // makes the encoder send CSI u. Not with Alt as well, which is
        // AltGr on Windows and has real text. Only for keys with a C0 form:
        // Ctrl+; and the like keep their text, or they would send nothing.
        let ctrl_only =
            mods.contains(key::Mods::CTRL) && !mods.intersects(key::Mods::ALT | key::Mods::SUPER);
        let own_char = |t: &str| {
            let mut chars = t.chars();
            chars.next().is_some_and(|c| c.eq_ignore_ascii_case(&k.ucp)) && chars.next().is_none()
        };
        let text = if ctrl_only && c0_control(k.ucp).is_some() && k.text.is_some_and(own_char) {
            None
        } else {
            k.text
        };

        // Modifiers the platform's text already accounts for: Shift, for a
        // printable key ('a' to 'A'); nothing for the others.
        let consumed = if k.ucp != '\0' && mods.contains(key::Mods::SHIFT) {
            key::Mods::SHIFT
        } else {
            key::Mods::empty()
        };
        self.event
            .set_action(k.action)
            .set_key(k.key)
            .set_mods(mods)
            .set_consumed_mods(consumed)
            // The Kitty protocol identifies keys by this, whatever Shift.
            .set_unshifted_codepoint(k.ucp)
            .set_utf8(text);
        // A fresh buffer: libghostty-vt's encode_to_vec retries a buffer
        // that's too small with reserve(required - remaining), which counts
        // from the length, so with bytes already queued the retry fails
        // too and the key is lost ("out of space, 16 bytes required").
        let mut seq = Vec::with_capacity(64);
        self.encoder
            // The terminal's modes now: application cursor keys, the Kitty
            // keyboard protocol and the rest.
            .set_options_from_terminal(terminal)
            .encode_to_vec(&self.event, &mut seq)?;
        if !seq.is_empty() {
            out.extend_from_slice(&seq);
        } else if k.action != key::Action::Release {
            if let Some(text) = text {
                // A key the encoder has no sequence for still typed its text.
                out.extend_from_slice(text.as_bytes());
            } else if ctrl_only && let Some(byte) = c0_control(k.ucp) {
                // The encoder sends nothing for Ctrl+[ (it does for Ctrl+]
                // and Ctrl+\). Send the classic control byte: ESC for Ctrl+[.
                out.push(byte);
            }
        }
        Ok(())
    }
}

/// The C0 control byte a Ctrl chord has always sent in terminals: Ctrl+@
/// through Ctrl+_ (letters in either case) map to 0x00-0x1F.
pub fn c0_control(ucp: char) -> Option<u8> {
    let upper = ucp.to_ascii_uppercase();
    ('@'..='_').contains(&upper).then_some(upper as u8 & 0x1F)
}

#[cfg(test)]
mod tests {
    use libghostty_vt::{
        Terminal,
        key::{self, Key},
    };

    use super::{KeyIn, Keys, c0_control};

    fn press(key: Key, ucp: char, mods: key::Mods, text: Option<&str>) -> KeyIn<'_> {
        KeyIn {
            action: key::Action::Press,
            key,
            ucp,
            mods,
            text,
        }
    }

    #[test]
    fn ctrl_chords_map_to_c0_controls() {
        assert_eq!(c0_control('['), Some(0x1B));
        assert_eq!(c0_control('c'), Some(0x03));
        assert_eq!(c0_control('@'), Some(0x00));
        assert_eq!(c0_control('_'), Some(0x1F));
        assert_eq!(c0_control('1'), None);
        assert_eq!(c0_control('='), None);
    }

    /// Ctrl+[ is ESC, with or without its own character as text; Ctrl+C is
    /// ETX; a key the encoder has no sequence for sends its text.
    #[test]
    fn ctrl_chords_and_plain_text_reach_the_program() {
        let terminal = Terminal::new(80, 24).unwrap();
        let mut keys = Keys::new().unwrap();
        let ctrl = key::Mods::CTRL;
        let mut out = Vec::new();
        keys.encode(
            &terminal,
            press(Key::BracketLeft, '[', ctrl, None),
            &mut out,
        )
        .unwrap();
        keys.encode(
            &terminal,
            press(Key::BracketLeft, '[', ctrl, Some("[")),
            &mut out,
        )
        .unwrap();
        keys.encode(&terminal, press(Key::C, 'c', ctrl, None), &mut out)
            .unwrap();
        keys.encode(
            &terminal,
            press(Key::A, 'a', key::Mods::empty(), Some("a")),
            &mut out,
        )
        .unwrap();
        assert_eq!(out, b"\x1b\x1b\x03a");
    }

    /// A long Kitty sequence queued after other bytes arrives whole: the
    /// encoder gets a fresh buffer (libghostty-vt's retry counts from the
    /// length and would lose it).
    #[test]
    fn a_key_queued_after_other_bytes_isnt_lost() {
        let mut terminal = Terminal::new(80, 24).unwrap();
        // Every Kitty keyboard flag, for a long sequence.
        terminal.vt_write(b"\x1b[>31u");
        let mut keys = Keys::new().unwrap();
        let all = key::Mods::CTRL | key::Mods::ALT | key::Mods::SHIFT | key::Mods::SUPER;
        let k = press(Key::A, 'a', all, Some("A"));
        let mut alone = Vec::new();
        keys.encode(&terminal, k, &mut alone).unwrap();
        assert!(alone.len() >= 10, "{alone:?}");
        let mut queued = Vec::with_capacity(8);
        queued.extend_from_slice(b"1234567");
        keys.encode(&terminal, k, &mut queued).unwrap();
        assert_eq!(&queued[7..], &alone[..]);
    }

    /// Releases send nothing in the legacy encoding, and only with the
    /// Kitty protocol's event types flag.
    #[test]
    fn releases_reach_only_a_program_that_asked() {
        let mut terminal = Terminal::new(80, 24).unwrap();
        let mut keys = Keys::new().unwrap();
        let release = KeyIn {
            action: key::Action::Release,
            ..press(Key::A, 'a', key::Mods::empty(), None)
        };
        let mut out = Vec::new();
        keys.encode(&terminal, release, &mut out).unwrap();
        assert!(out.is_empty(), "{out:?}");
        // Disambiguate and report event types.
        terminal.vt_write(b"\x1b[>3u");
        keys.encode(&terminal, release, &mut out).unwrap();
        assert_eq!(out, b"\x1b[97;1:3u");
    }
}
