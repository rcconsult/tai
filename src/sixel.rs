//! Sixel images, translated into Kitty images (ADR 0006, ADR 0009).
//!
//! libghostty-vt has no Sixel: its DCS handler drops any DCS it doesn't
//! know. `Translator` sits between the pty and `vt_write`, finds Sixel DCS
//! sequences (`ESC P <params> q <data> ESC \`) that aren't inside another
//! string, decodes each with `icy_sixel` into RGBA, and passes on a Kitty
//! transmit-and-display command in its place. Everything else passes
//! through unchanged.
//!
//! The Kitty command carries `q=2` and no image id, so libghostty-vt picks
//! its own id and writes nothing back to the program (`graphics_exec.zig`,
//! Ghostty a887df42). It carries `C=1` and is followed by one line feed per
//! cell row and a carriage return, which leaves the cursor at the start of
//! the row below the image, as xterm does (checked in XTerm 372 with
//! `-ti vt340`; ADR 0009).

use icy_sixel::SixelImage;

/// The largest decoded image accepted, in bytes of RGBA. The same as tai's
/// whole Kitty image storage (`set_kitty_image_storage_limit`).
pub const MAX_RGBA: usize = 64 * 1024 * 1024;

/// The largest Sixel sequence collected before it's given up on. Sixel
/// repeats make the encoded form smaller than the image, so this is the same
/// bound as the decoded one.
const MAX_SEQUENCE: usize = MAX_RGBA;

/// The longest DCS head (`ESC P` and its parameters) held back while
/// waiting for the final byte. Real heads are a few bytes.
const MAX_HEAD: usize = 256;

/// The largest base64 payload in one Kitty command.
const KITTY_CHUNK: usize = 4096;

const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const ST_C1: u8 = 0x9c;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Passing output through.
    Ground,
    /// An ESC ended the previous chunk; the next byte says whether a DCS
    /// starts.
    Escape,
    /// After `ESC P`, reading the DCS parameters up to the final byte.
    DcsHead,
    /// Inside a Sixel DCS, collecting it.
    Sixel,
    /// After an ESC inside a Sixel DCS.
    SixelEscape,
    /// Inside a Sixel DCS that grew too large: dropping it to its end.
    Discard,
    /// After an ESC while dropping.
    DiscardEscape,
}

/// Finds Sixel images in pty output and replaces them with Kitty images.
///
/// Only `ESC P` needs finding. Strings (OSC, APC, other DCS) need no
/// tracking: an ESC inside one ends it in libghostty-vt's parser too, so
/// `ESC P` starts a DCS wherever it appears.
pub struct Translator {
    state: State,
    /// The DCS head (`ESC P` and parameters) while it's being read, then the
    /// whole Sixel sequence while it's collected.
    seq: Vec<u8>,
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}

impl Translator {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            seq: Vec::new(),
        }
    }

    /// Pass `input` on to `sink`, with each complete Sixel image replaced by
    /// a Kitty image. `cell_height` is the cell height in pixels, which
    /// decides how many rows the cursor moves down. State carries over
    /// between calls, so a sequence may be split anywhere.
    pub fn feed(&mut self, input: &[u8], cell_height: u32, sink: &mut dyn FnMut(&[u8])) {
        // Bytes from `run` up to the current position are passed on in one
        // piece when the scan leaves Ground or reaches the end, so ordinary
        // output reaches libghostty-vt as large slices.
        let mut i = 0;
        let mut run = 0;
        while i < input.len() {
            match self.state {
                State::Ground => {
                    let Some(n) = input[i..].iter().position(|&c| c == ESC) else {
                        break;
                    };
                    let esc = i + n;
                    match input.get(esc + 1) {
                        Some(b'P') => {
                            sink(&input[run..esc]);
                            self.start_dcs();
                            i = esc + 2;
                            run = i;
                        }
                        Some(_) => i = esc + 1,
                        None => {
                            // The chunk ends on ESC: hold it back.
                            sink(&input[run..esc]);
                            i = esc + 1;
                            run = i;
                            self.state = State::Escape;
                        }
                    }
                }
                State::Escape => {
                    if input[i] == b'P' {
                        self.start_dcs();
                        i += 1;
                        run = i;
                    } else {
                        sink(b"\x1b");
                        run = i;
                        self.state = State::Ground;
                    }
                }
                State::DcsHead => {
                    let b = input[i];
                    i += 1;
                    run = i;
                    match b {
                        b'0'..=b'9' | b';' if self.seq.len() < MAX_HEAD => self.seq.push(b),
                        b'0'..=b'9' | b';' => {
                            // Too long for a real head: stop holding it back.
                            self.seq.push(b);
                            sink(&self.seq);
                            self.seq.clear();
                            self.state = State::Ground;
                        }
                        b'q' => {
                            self.seq.push(b);
                            self.state = State::Sixel;
                        }
                        CAN | SUB => {
                            // Aborted: libghostty-vt would drop it too.
                            self.seq.clear();
                            self.state = State::Ground;
                        }
                        ESC => {
                            // A new sequence cancels this one.
                            self.seq.clear();
                            self.state = State::Escape;
                        }
                        _ => {
                            // Another DCS (XTGETTCAP, DECRQSS, ...): pass on
                            // what was held back and let it through.
                            self.seq.push(b);
                            sink(&self.seq);
                            self.seq.clear();
                            self.state = State::Ground;
                        }
                    }
                }
                State::Sixel | State::Discard => {
                    // Sixel data has no ESC, CAN, SUB or 0x9C inside it.
                    let end = input[i..]
                        .iter()
                        .position(|&c| matches!(c, ESC | CAN | SUB | ST_C1))
                        .map_or(input.len(), |n| i + n);
                    if self.state == State::Sixel {
                        if self.seq.len() + (end - i) > MAX_SEQUENCE {
                            tracing::warn!("sixel: image over {MAX_SEQUENCE} bytes, dropped");
                            self.seq = Vec::new();
                            self.state = State::Discard;
                        } else {
                            self.seq.extend_from_slice(&input[i..end]);
                        }
                    }
                    i = end;
                    run = i;
                    if i == input.len() {
                        break;
                    }
                    let c = input[i];
                    i += 1;
                    run = i;
                    let collecting = self.state == State::Sixel;
                    self.state = match c {
                        ESC if collecting => State::SixelEscape,
                        ESC => State::DiscardEscape,
                        ST_C1 => {
                            if collecting {
                                self.finish(cell_height, sink);
                            }
                            State::Ground
                        }
                        // CAN or SUB: aborted.
                        _ => {
                            self.seq.clear();
                            State::Ground
                        }
                    };
                }
                State::SixelEscape | State::DiscardEscape => {
                    if input[i] == b'\\' {
                        if self.state == State::SixelEscape {
                            self.finish(cell_height, sink);
                        }
                        i += 1;
                        run = i;
                        self.state = State::Ground;
                    } else {
                        // ESC followed by anything else cancels the image and
                        // starts a new sequence: treat it as a held ESC.
                        self.seq.clear();
                        self.state = State::Escape;
                    }
                }
            }
        }
        if self.state == State::Ground && run < input.len() {
            sink(&input[run..]);
        }
    }

    fn start_dcs(&mut self) {
        self.seq.clear();
        self.seq.extend_from_slice(b"\x1bP");
        self.state = State::DcsHead;
    }

    /// Decode the collected Sixel sequence and pass on the Kitty image.
    fn finish(&mut self, cell_height: u32, sink: &mut dyn FnMut(&[u8])) {
        self.seq.extend_from_slice(b"\x1b\\");
        let seq = std::mem::take(&mut self.seq);
        // icy_sixel allows up to 64 Mi pixels (256 MB of RGBA), so check the
        // size the sequence asks for before letting it allocate.
        let (width, height) = drawn_size(&seq);
        if width.saturating_mul(height).saturating_mul(4) > MAX_RGBA as u64 {
            tracing::warn!(
                "sixel: {width}x{height} image is over {MAX_RGBA} bytes of RGBA, dropped"
            );
            return;
        }
        let image = match SixelImage::decode(&seq) {
            Ok(image) => image,
            Err(e) => {
                tracing::warn!("sixel: can't decode image: {e}");
                return;
            }
        };
        if image.width == 0 || image.height == 0 {
            return;
        }
        if image.pixels.len() > MAX_RGBA {
            tracing::warn!(
                "sixel: {}x{} image is over {MAX_RGBA} bytes of RGBA, dropped",
                image.width,
                image.height
            );
            return;
        }
        for command in kitty_commands(&image.pixels, image.width, image.height) {
            sink(&command);
        }
        // Kitty's C=1 left the cursor at the image's top left. xterm leaves
        // it at the start of the row below the image (ADR 0009).
        let rows = cell_rows(image.height, cell_height);
        for _ in 0..rows {
            sink(b"\n");
        }
        sink(b"\r");
    }
}

/// An upper bound on the pixel size a Sixel sequence decodes to, from its
/// raster attributes (`" Pan;Pad;Ph;Pv`), the columns its sixels and repeats
/// (`!n`) reach, and its bands (`-`, six pixel rows each). Saturates instead
/// of overflowing, so a huge repeat count gives a huge size.
pub fn drawn_size(seq: &[u8]) -> (u64, u64) {
    let body = match seq.iter().position(|&c| c == b'q') {
        Some(q) => &seq[q + 1..],
        None => return (0, 0),
    };
    // Up to the terminator: the '\' of ESC \ is in the sixel range.
    let body = &body[..body
        .iter()
        .position(|&c| c == ESC || c == ST_C1)
        .unwrap_or(body.len())];
    let number = |bytes: &[u8], i: &mut usize| -> u64 {
        let mut n: u64 = 0;
        while let Some(&c) = bytes.get(*i).filter(|c| c.is_ascii_digit()) {
            n = n.saturating_mul(10).saturating_add(u64::from(c - b'0'));
            *i += 1;
        }
        n
    };
    let (mut x, mut max_x, mut bands): (u64, u64, u64) = (0, 0, 1);
    let (mut raster_w, mut raster_h) = (0, 0);
    let mut repeat = 1;
    let mut i = 0;
    while i < body.len() {
        let c = body[i];
        i += 1;
        match c {
            b'"' => {
                let mut params = [0u64; 4];
                for (k, param) in params.iter_mut().enumerate() {
                    *param = number(body, &mut i);
                    if k < 3 && body.get(i) == Some(&b';') {
                        i += 1;
                    } else {
                        break;
                    }
                }
                raster_w = params[2];
                raster_h = params[3];
            }
            b'#' => {
                while body
                    .get(i)
                    .is_some_and(|c| c.is_ascii_digit() || *c == b';')
                {
                    i += 1;
                }
            }
            b'!' => repeat = number(body, &mut i).max(1),
            b'?'..=b'~' => {
                x = x.saturating_add(repeat);
                max_x = max_x.max(x);
                repeat = 1;
            }
            b'$' => x = 0,
            b'-' => {
                x = 0;
                bands = bands.saturating_add(1);
            }
            _ => {}
        }
    }
    (max_x.max(raster_w), bands.saturating_mul(6).max(raster_h))
}

/// Cell rows an image of `height` pixels covers: ceil(height / cell height).
pub fn cell_rows(height: usize, cell_height: u32) -> usize {
    let cell_height = cell_height.max(1) as usize;
    height.div_ceil(cell_height)
}

/// The Kitty commands that transmit and display an RGBA image at the cursor
/// without moving it: `a=T,f=32,q=2,C=1`, no id, payload in 4096-byte
/// base64 chunks.
pub fn kitty_commands(rgba: &[u8], width: usize, height: usize) -> Vec<Vec<u8>> {
    let payload = base64(rgba);
    let chunks: Vec<&[u8]> = payload.chunks(KITTY_CHUNK).collect();
    let last = chunks.len().saturating_sub(1);
    chunks
        .iter()
        .enumerate()
        .map(|(n, chunk)| {
            let more = u8::from(n != last);
            let mut command = if n == 0 {
                format!("\x1b_Ga=T,f=32,q=2,C=1,s={width},v={height},m={more};").into_bytes()
            } else {
                format!("\x1b_Gm={more};").into_bytes()
            };
            command.extend_from_slice(chunk);
            command.extend_from_slice(b"\x1b\\");
            command
        })
        .collect()
}

/// Standard base64 with padding.
fn base64(data: &[u8]) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for group in data.chunks(3) {
        let n = match *group {
            [a, b, c] => u32::from(a) << 16 | u32::from(b) << 8 | u32::from(c),
            [a, b] => u32::from(a) << 16 | u32::from(b) << 8,
            [a] => u32::from(a) << 16,
            _ => unreachable!(),
        };
        let sextets = [n >> 18, n >> 12, n >> 6, n].map(|s| TABLE[(s & 63) as usize]);
        let keep = group.len() + 1;
        out.extend_from_slice(&sextets[..keep]);
        out.resize(out.len() + 4 - keep, b'=');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Translator, base64, cell_rows};

    /// A 3x6 red Sixel image: one sixel band, three columns.
    const RED: &[u8] = b"\x1bPq#0;2;100;0;0#0~~~\x1b\\";

    fn run(chunks: &[&[u8]], cell_height: u32) -> Vec<u8> {
        let mut t = Translator::new();
        let mut out = Vec::new();
        for chunk in chunks {
            t.feed(chunk, cell_height, &mut |b| out.extend_from_slice(b));
        }
        out
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn base64_matches_rfc4648() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected.as_bytes());
        }
    }

    #[test]
    fn cell_rows_round_up() {
        assert_eq!(cell_rows(6, 20), 1);
        assert_eq!(cell_rows(20, 20), 1);
        assert_eq!(cell_rows(21, 20), 2);
        assert_eq!(cell_rows(100, 0), 100);
    }

    #[test]
    fn plain_output_passes_through() {
        let text: &[u8] = b"hello \x1b[31mred\x1b[0m\r\n\xc3\x90 done";
        assert_eq!(run(&[text], 20), text);
    }

    #[test]
    fn sixel_becomes_a_quiet_kitty_image() {
        let out = run(&[b"before ", RED, b" after"], 20);
        assert!(out.starts_with(b"before \x1b_Ga=T,f=32,q=2,C=1,s=3,v=6,m=0;"));
        assert!(out.ends_with(b"\x1b\\\n\r after"));
        assert!(!contains(&out, b"\x1bP"));
        // 3x6 RGBA is 72 bytes: 96 base64 characters.
        let start = out.iter().position(|&c| c == b';').unwrap() + 1;
        let end = start + out[start..].iter().position(|&c| c == 0x1b).unwrap();
        assert_eq!(end - start, 96);
    }

    #[test]
    fn cursor_moves_below_the_image() {
        // Six bands of six pixels: 36 px tall, two 20 px rows: two line
        // feeds and a carriage return.
        let tall = b"\x1bPq#0;2;0;0;100#0~-~-~-~-~-~\x1b\\";
        let out = run(&[tall.as_slice()], 20);
        assert!(out.ends_with(b"\x1b\\\n\n\r"));
        assert_eq!(out.iter().filter(|&&c| c == b'\n').count(), 2);
    }

    #[test]
    fn split_anywhere_gives_the_same_output() {
        let stream = [b"a\x1b[1mb".as_slice(), RED, b"c\x1b]0;t\x07d"].concat();
        let whole = run(&[&stream], 20);
        for cut in 0..=stream.len() {
            let (x, y) = stream.split_at(cut);
            assert_eq!(run(&[x, y], 20), whole, "cut at {cut}");
        }
        let bytes: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(run(&bytes, 20), whole);
    }

    #[test]
    fn other_dcs_passes_through() {
        // XTGETTCAP and DECRQSS are libghostty-vt's to answer.
        for dcs in [
            b"\x1bP+q544e\x1b\\".as_slice(),
            b"\x1bP$qm\x1b\\".as_slice(),
        ] {
            assert_eq!(run(&[dcs], 20), dcs);
        }
    }

    #[test]
    fn esc_p_inside_other_strings_is_not_sixel() {
        // "Pq~" in an OSC title or an APC payload is just string content.
        // (An ESC inside a string ends it, in libghostty-vt as here, so an
        // ESC P there would start a real DCS.)
        let osc: &[u8] = b"\x1b]2;title Pq#0~\x07rest";
        assert_eq!(run(&[osc], 20), osc);
        let apc: &[u8] = b"\x1b_Gf=24;Pq~~\x1b\\rest";
        assert_eq!(run(&[apc], 20), apc);
    }

    #[test]
    fn kitty_apc_passes_through() {
        let apc: &[u8] = b"\x1b_Ga=T,f=24,s=1,v=1;AAAA\x1b\\";
        assert_eq!(run(&[apc], 20), apc);
    }

    #[test]
    fn can_and_sub_abort_an_image() {
        for abort in [0x18u8, 0x1a] {
            let stream = [b"\x1bPq#0~~".as_slice(), &[abort], b"after"].concat();
            assert_eq!(run(&[&stream], 20), b"after");
        }
    }

    #[test]
    fn c1_st_ends_an_image() {
        let out = run(&[b"\x1bPq#0;2;100;0;0#0~\x9cafter".as_slice()], 20);
        assert!(contains(&out, b"\x1b_Ga=T"));
        assert!(out.ends_with(b"after"));
    }

    #[test]
    fn esc_inside_an_image_cancels_it() {
        let out = run(&[b"\x1bPq#0~~\x1b[31mred".as_slice()], 20);
        assert_eq!(out, b"\x1b[31mred");
    }

    #[test]
    fn libghostty_stores_the_image_and_the_cursor_lands_on_its_last_row() {
        use libghostty_vt::{Terminal, TerminalOptions};
        let mut term = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 100,
        })
        .unwrap();
        term.resize(80, 24, 10, 20).unwrap();
        term.set_kitty_image_storage_limit(64 * 1024 * 1024)
            .unwrap();

        // Six bands of six pixels: 3x36, so two 20 px rows.
        let tall = b"\x1bPq#0;2;0;0;100#0~~~-~~~-~~~-~~~-~~~-~~~\x1b\\";
        let mut t = Translator::new();
        let mut write = |b: &[u8]| term.vt_write(b);
        t.feed(b"AB", 20, &mut write);
        t.feed(tall, 20, &mut write);

        // As in xterm, the cursor ends at the start of the row below.
        assert_eq!(term.cursor_x().unwrap(), 0);
        assert_eq!(term.cursor_y().unwrap(), 2);
        // No id was sent, so libghostty-vt numbered it from 2147483647.
        let graphics = term.kitty_graphics().unwrap();
        let image = graphics.image(2_147_483_647).expect("image stored");
        assert_eq!(image.width().unwrap(), 3);
        assert_eq!(image.height().unwrap(), 36);
    }

    #[test]
    fn drawn_size_bounds_the_decoded_image() {
        use super::drawn_size;
        // Three columns, one band.
        assert_eq!(drawn_size(RED), (3, 6));
        // Raster attributes larger than what's drawn win.
        assert_eq!(drawn_size(b"\x1bPq\"1;1;100;50#0~\x1b\\"), (100, 50));
        // Repeats and bands.
        assert_eq!(drawn_size(b"\x1bPq#0!20~-!5~$!7~\x1b\\"), (20, 12));
        // A repeat count past u64 saturates instead of wrapping.
        let huge = b"\x1bPq!99999999999999999999999~\x1b\\";
        assert_eq!(drawn_size(huge).0, u64::MAX);
    }

    #[test]
    fn oversized_images_are_dropped_before_decoding() {
        // 20000 x 1200 x 4 bytes is over 64 MB, from a sequence of a few bytes.
        let big: &[u8] = b"before\x1bPq#0;2;100;0;0#0!20000~\x22\x31;1;20000;1200\x1b\\after";
        assert_eq!(run(&[big], 20), b"beforeafter");
    }

    #[test]
    fn a_long_dcs_head_is_passed_through() {
        let head = [b"\x1bP".as_slice(), &[b'1'; 400], b"q#0~\x1b\\"].concat();
        let out = run(&[&head], 20);
        // Not held back or translated: everything reaches libghostty-vt.
        assert_eq!(out, head);
    }

    #[test]
    fn raw_0x90_is_text_not_dcs() {
        // libghostty-vt decodes UTF-8 and never starts a DCS from 0x90.
        let text: &[u8] = b"\x90q~~\x9c";
        assert_eq!(run(&[text], 20), text);
    }
}
