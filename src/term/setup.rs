//! Creating a terminal, and its answers to programs' queries (ADR 0014,
//! step 1). tai and tai-gui answered differently before: only tai answered
//! device attributes and the colour-scheme query. The mode 2048 size
//! report needs nothing here beyond the size callback: libghostty-vt sends
//! it itself when a program sets the mode and on each resize (Ghostty
//! src/terminal/stream_terminal.zig, reportMode2048, at 108cacb1).

use libghostty_vt::{
    Terminal,
    effect::{
        ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType,
        PrimaryDeviceAttributes, SecondaryDeviceAttributes,
    },
    mode::Mode,
    report::SizeReportSize,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Lines of scrollback a terminal keeps.
const SCROLLBACK_LINES: usize = 1000;

/// A terminal of `cols` by `rows` cells of `cell_pixels`, with tai's
/// options. The cell size is set by an initial resize, since the options
/// have none: without it, Kitty graphics placements divide by zero.
pub fn new_terminal<'alloc, 'cb>(
    cols: u16,
    rows: u16,
    (cell_w, cell_h): (u32, u32),
) -> Result<Terminal<'alloc, 'cb>> {
    let mut terminal = Terminal::new(cols, rows)?;
    terminal.set_scrollback_max_lines(Some(SCROLLBACK_LINES))?;
    // ConPTY keeps its own screen and can't take scrollback back, so on
    // Windows a resize mustn't pull scrollback into the active screen
    // (docs/lessons/from-libghostty-projects.md, section 1).
    #[cfg(windows)]
    terminal.set_resize_pull_scrollback(Some(false))?;
    // Grapheme clustering, mode 2027, only when asked for (grapheme.rs).
    if crate::grapheme::clustering() {
        terminal.set_mode(Mode::GRAPHEME_CLUSTER, true)?;
    }
    terminal.resize(cols, rows, cell_w, cell_h)?;
    Ok(terminal)
}

/// Answer the queries programs send at startup, as Ghostty's app does.
/// Without these, vim, tmux and htop wait for answers that never come, or
/// fall back. `reply` writes to the pty; `size` gives the grid in cells
/// and the cell size in pixels, as they are now.
pub fn answer_queries<'alloc, 'cb>(
    terminal: &mut Terminal<'alloc, 'cb>,
    reply: impl Fn(&[u8]) + 'cb,
    size: impl Fn() -> ((u16, u16), (u32, u32)) + 'cb,
) -> Result<()> {
    // Whether DA1 lists Sixel (ADR 0009). tai translates Sixel either way.
    let advertise_sixel = std::env::var("TAI_SIXEL").as_deref() == Ok("1");
    terminal
        // Answers the terminal writes back: device status reports, mode
        // queries, device attributes and so on.
        .on_pty_write(move |_t, data| reply(data))?
        // XTWINOPS size queries (CSI 14/16/18 t), in cells and pixels.
        .on_size(move |_t| {
            let ((columns, rows), (cell_width, cell_height)) = size();
            Some(SizeReportSize {
                rows,
                columns,
                cell_width,
                cell_height,
            })
        })?
        // DA1/DA2/DA3: VT220-level with a modest feature set.
        .on_device_attributes(move |_t| {
            Some(DeviceAttributes {
                // Sixel only on request (ADR 0009): textual-image picks
                // Sixel over Kitty placeholders whenever it's listed.
                primary: PrimaryDeviceAttributes::new(
                    ConformanceLevel::VT220,
                    if advertise_sixel {
                        &[
                            DeviceAttributeFeature::COLUMNS_132,
                            DeviceAttributeFeature::SIXEL,
                            DeviceAttributeFeature::SELECTIVE_ERASE,
                            DeviceAttributeFeature::ANSI_COLOR,
                        ]
                    } else {
                        &[
                            DeviceAttributeFeature::COLUMNS_132,
                            DeviceAttributeFeature::SELECTIVE_ERASE,
                            DeviceAttributeFeature::ANSI_COLOR,
                        ]
                    },
                ),
                // DA2: VT220-type, version 1, no ROM cartridge.
                secondary: SecondaryDeviceAttributes {
                    device_type: DeviceType::VT220,
                    firmware_version: 1,
                    rom_cartridge: 0,
                },
                // DA3: the default unit id, 0.
                tertiary: Default::default(),
            })
        })?
        // CSI > q: the application's name.
        .on_xtversion(|_t| Some("tai"))?
        // CSI ? 996 n: there's no API here to ask the OS for its colour
        // scheme, so the query goes unanswered rather than guessed.
        .on_color_scheme(|_t| None)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::{answer_queries, new_terminal};

    #[test]
    fn setting_mode_2048_sends_one_size_report() {
        // libghostty-vt reports the size itself when a program sets mode
        // 2048, given the size callback answer_queries installs (Ghostty
        // src/terminal/stream_terminal.zig, reportMode2048, at 108cacb1).
        // tai sent its own too, so programs got it twice (macos-tai,
        // popos-tai).
        let mut terminal = new_terminal(80, 24, (10, 20)).unwrap();
        let sent = Rc::new(RefCell::new(Vec::new()));
        let s = sent.clone();
        answer_queries(
            &mut terminal,
            move |data| s.borrow_mut().extend_from_slice(data),
            || ((80, 24), (10, 20)),
        )
        .unwrap();
        terminal.vt_write(b"\x1b[?2048h");
        let set = String::from_utf8(std::mem::take(&mut *sent.borrow_mut())).unwrap();
        assert_eq!(set, "\x1b[48;24;80;480;800t");
        // And one on each resize while the mode is on.
        terminal.resize(100, 30, 9, 18).unwrap();
        let resized = String::from_utf8(std::mem::take(&mut *sent.borrow_mut())).unwrap();
        assert_eq!(resized, "\x1b[48;30;100;540;900t");
    }

    #[test]
    fn device_attributes_and_xtversion_are_answered() {
        let mut terminal = new_terminal(80, 24, (10, 20)).unwrap();
        let sent = Rc::new(RefCell::new(Vec::new()));
        let s = sent.clone();
        answer_queries(
            &mut terminal,
            move |data| s.borrow_mut().extend_from_slice(data),
            || ((80, 24), (10, 20)),
        )
        .unwrap();
        terminal.vt_write(b"\x1b[c");
        let da1 = String::from_utf8(std::mem::take(&mut *sent.borrow_mut())).unwrap();
        assert!(da1.starts_with("\x1b[?62;"), "DA1 {da1:?}");
        terminal.vt_write(b"\x1b[>q");
        let xtversion = String::from_utf8(std::mem::take(&mut *sent.borrow_mut())).unwrap();
        assert!(xtversion.contains("tai"), "XTVERSION {xtversion:?}");
        terminal.vt_write(b"\x1b[18t");
        let size = String::from_utf8(std::mem::take(&mut *sent.borrow_mut())).unwrap();
        assert_eq!(size, "\x1b[8;24;80t");
    }
}
