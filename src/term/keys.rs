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
//!
//! Input methods and dead keys (ADR 0015) go through here too: a key that
//! arrives while a composition is open is marked composing, text an input
//! method commits goes out as typed text, and the preedit (the text being
//! composed) is kept here, never written to the pty, until it ends.

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
    /// Whether it arrived while an input method or a dead key was
    /// composing: the legacy encoding sends nothing for it, the Kitty
    /// encoding only plain modifiers (libghostty-vt's `composing`).
    pub composing: bool,
}

/// The encoder and its event, kept between keys.
pub struct Keys {
    encoder: key::Encoder<'static>,
    event: key::Event<'static>,
    /// Keys whose press went to the program and that aren't released yet.
    down: Vec<Key>,
    /// The text an input method or a dead key is composing, if any.
    preedit: Option<String>,
}

impl Keys {
    pub fn new() -> Result<Self> {
        Ok(Self {
            encoder: key::Encoder::new()?,
            event: key::Event::new()?,
            down: Vec::new(),
            preedit: None,
        })
    }

    /// The text being composed, to draw at the cursor (`rows.rs`).
    pub fn preedit(&self) -> Option<&str> {
        self.preedit.as_deref()
    }

    /// The input method's preedit changed; empty or none ends it. Returns
    /// whether it changed, so the front end repaints.
    // Until each front end reports its input method (ADR 0015, parts 2-6).
    #[allow(dead_code)]
    pub fn set_preedit(&mut self, text: Option<&str>) -> bool {
        let text = text.filter(|t| !t.is_empty()).map(str::to_owned);
        let changed = text != self.preedit;
        self.preedit = text;
        changed
    }

    /// The terminal lost the focus: a composition can't go on, and keys
    /// held now are released where tai doesn't see it.
    #[allow(dead_code)]
    pub fn focus_lost(&mut self) {
        self.preedit = None;
        self.down.clear();
    }

    /// Text an input method committed, or text with no key at all (the
    /// emoji picker, dictation): typed input, never a paste. It goes
    /// through the encoder as a key event with no key and no modifiers, as
    /// Ghostty's `committedTextAction` sends it (macOS
    /// `SurfaceView_AppKit.swift` at 108cacb1), so the Kitty protocol sees
    /// it the way Ghostty's programs do. It ends the composition. Control
    /// characters on their own (an input method's Backspace or Enter while
    /// composing) aren't the program's.
    #[allow(dead_code)]
    pub fn commit(
        &mut self,
        terminal: &Terminal<'_, '_>,
        text: &str,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        self.preedit = None;
        if text.is_empty() || text.chars().all(char::is_control) {
            return Ok(());
        }
        self.event
            .set_action(key::Action::Press)
            .set_key(Key::Unidentified)
            .set_mods(key::Mods::empty())
            .set_consumed_mods(key::Mods::empty())
            .set_unshifted_codepoint('\0')
            .set_composing(false)
            .set_utf8(Some(text));
        let mut seq = Vec::with_capacity(64);
        self.encoder
            .set_options_from_terminal(terminal)
            .encode_to_vec(&self.event, &mut seq)?;
        if seq.is_empty() {
            out.extend_from_slice(text.as_bytes());
        } else {
            out.extend_from_slice(&seq);
        }
        Ok(())
    }

    /// Append what the program gets for `k` to `out`.
    pub fn encode(
        &mut self,
        terminal: &Terminal<'_, '_>,
        k: KeyIn<'_>,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        // A release goes to the program only after its press did, as a
        // mouse button's does (pointer.rs): a key whose press was an app
        // shortcut (Cmd+C, Cmd+=), or was pressed before the terminal had
        // the focus, would send a Kitty release with no press.
        // A composing press doesn't reach the program, so neither does its
        // release; nor does the release of the key that committed a
        // composition, whose press the toolkit gave the input method.
        if k.action == key::Action::Release {
            let Some(i) = self.down.iter().position(|&d| d == k.key) else {
                return Ok(());
            };
            self.down.swap_remove(i);
        } else if !k.composing && !self.down.contains(&k.key) {
            self.down.push(k.key);
        }
        // An ordinary key with a preedit still held: the composition has
        // ended without a commit the front end saw (both toolkits send
        // keys again only once composing is over).
        if !k.composing && k.action != key::Action::Release {
            self.preedit = None;
        }

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
            .set_composing(k.composing)
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
        } else if k.action != key::Action::Release && !k.composing {
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
            composing: false,
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
        let a = press(Key::A, 'a', key::Mods::empty(), None);
        let release = KeyIn {
            action: key::Action::Release,
            ..a
        };
        let mut out = Vec::new();
        keys.encode(&terminal, a, &mut out).unwrap();
        out.clear();
        keys.encode(&terminal, release, &mut out).unwrap();
        assert!(out.is_empty(), "{out:?}");
        // Disambiguate and report event types.
        terminal.vt_write(b"\x1b[>3u");
        keys.encode(&terminal, a, &mut out).unwrap();
        out.clear();
        keys.encode(&terminal, release, &mut out).unwrap();
        assert_eq!(out, b"\x1b[97;1:3u");
    }

    /// A release whose press never reached the program (an app shortcut
    /// took it) sends nothing, even to a program that asked for releases.
    #[test]
    fn a_release_goes_out_only_after_its_press() {
        let mut terminal = Terminal::new(80, 24).unwrap();
        terminal.vt_write(b"\x1b[>3u");
        let mut keys = Keys::new().unwrap();
        let c = press(Key::C, 'c', key::Mods::SUPER, None);
        let release = KeyIn {
            action: key::Action::Release,
            ..c
        };
        let mut out = Vec::new();
        keys.encode(&terminal, release, &mut out).unwrap();
        assert!(out.is_empty(), "{out:?}");
        keys.encode(&terminal, c, &mut out).unwrap();
        let pressed = out.len();
        assert!(pressed > 0);
        keys.encode(&terminal, release, &mut out).unwrap();
        assert!(out.len() > pressed, "{out:?}");
        // Released once: a second release has no press.
        let released = out.len();
        keys.encode(&terminal, release, &mut out).unwrap();
        assert_eq!(out.len(), released);
    }

    /// While composing (a dead key on its way to é), keys send nothing in
    /// either encoding, and their releases don't either; the commit goes
    /// out as text, and clears the preedit.
    #[test]
    fn a_composition_sends_only_its_commit() {
        for flags in [&b""[..], b"\x1b[>11u"] {
            let mut terminal = Terminal::new(80, 24).unwrap();
            terminal.vt_write(flags);
            let mut keys = Keys::new().unwrap();
            let mut out = Vec::new();
            let dead = KeyIn {
                composing: true,
                ..press(Key::Quote, '\'', key::Mods::empty(), None)
            };
            keys.encode(&terminal, dead, &mut out).unwrap();
            assert!(keys.set_preedit(Some("´")));
            assert_eq!(keys.preedit(), Some("´"));
            let release = KeyIn {
                action: key::Action::Release,
                ..dead
            };
            keys.encode(&terminal, release, &mut out).unwrap();
            assert!(out.is_empty(), "{flags:?}: {out:?}");
            keys.commit(&terminal, "é", &mut out).unwrap();
            assert_eq!(keys.preedit(), None);
            // The committing key's release: its press went to the input
            // method, never to the program.
            let e_release = KeyIn {
                action: key::Action::Release,
                ..press(Key::E, 'e', key::Mods::empty(), None)
            };
            keys.encode(&terminal, e_release, &mut out).unwrap();
            let text = String::from_utf8(out).unwrap();
            assert!(
                text.contains('é') || text.contains("233"),
                "{flags:?}: {text:?}"
            );
            assert!(!text.contains(":3u"), "{flags:?}: {text:?}");
        }
    }

    /// In the legacy encoding a commit is its UTF-8, and control
    /// characters on their own aren't the program's.
    #[test]
    fn a_commit_is_typed_text() {
        let terminal = Terminal::new(80, 24).unwrap();
        let mut keys = Keys::new().unwrap();
        let mut out = Vec::new();
        keys.commit(&terminal, "你好", &mut out).unwrap();
        keys.commit(&terminal, "\r", &mut out).unwrap();
        keys.commit(&terminal, "b", &mut out).unwrap();
        assert_eq!(out, "你好b".as_bytes());
    }

    /// The preedit ends on a commit, on focus loss, and when an ordinary
    /// key arrives; the focus loss also forgets held keys.
    #[test]
    fn the_preedit_ends_when_composing_does() {
        let mut terminal = Terminal::new(80, 24).unwrap();
        terminal.vt_write(b"\x1b[>3u");
        let mut keys = Keys::new().unwrap();
        let mut out = Vec::new();
        assert!(!keys.set_preedit(Some("")));
        keys.set_preedit(Some("に"));
        keys.encode(
            &terminal,
            press(Key::Escape, '\0', key::Mods::empty(), None),
            &mut out,
        )
        .unwrap();
        assert_eq!(keys.preedit(), None);
        keys.set_preedit(Some("に"));
        let a = press(Key::A, 'a', key::Mods::empty(), Some("a"));
        keys.encode(&terminal, a, &mut out).unwrap();
        keys.focus_lost();
        assert_eq!(keys.preedit(), None);
        out.clear();
        let release = KeyIn {
            action: key::Action::Release,
            ..a
        };
        keys.encode(&terminal, release, &mut out).unwrap();
        assert!(out.is_empty(), "{out:?}");
    }
}
