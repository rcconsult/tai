//! Creating a terminal, and its answers to programs' queries (ADR 0014,
//! step 1). tai and tai-gui answered differently before: only tai answered
//! device attributes and the colour-scheme query, and sent the mode 2048
//! size report when a program set the mode.

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

/// Mode 2048, in-band size reports. libghostty-vt sends a report on each
/// resize while the mode is on, but not when a program sets it; Ghostty's
/// app sends one then (src/termio/stream_handler.zig, at a887df42), and
/// Textual waits for it before it reads SGR-Pixels mouse reports as pixels.
#[derive(Default)]
pub struct InBandResize {
    on: bool,
}

impl InBandResize {
    /// After the program's output was parsed: the report to send, if the
    /// program has just set the mode.
    pub fn report(
        &mut self,
        terminal: &Terminal<'_, '_>,
        grid: (u16, u16),
        cell_pixels: (u32, u32),
    ) -> Result<Option<String>> {
        let on = terminal.mode(Mode::IN_BAND_RESIZE)?;
        let report = (on && !self.on).then(|| size_report(grid, cell_pixels));
        self.on = on;
        Ok(report)
    }
}

/// The mode 2048 size report, ESC [ 48 ; rows ; cols ; height ; width t,
/// encoded like Ghostty's src/terminal/size_report.zig (at a887df42).
fn size_report((cols, rows): (u16, u16), (cell_w, cell_h): (u32, u32)) -> String {
    let (height, width) = (
        u64::from(rows) * u64::from(cell_h),
        u64::from(cols) * u64::from(cell_w),
    );
    format!("\x1b[48;{rows};{cols};{height};{width}t")
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::{InBandResize, answer_queries, new_terminal, size_report};

    #[test]
    fn the_in_band_size_report_matches_libghosttys_on_resize() {
        let mut terminal = new_terminal(80, 24, (10, 20)).unwrap();
        let sent = Rc::new(RefCell::new(Vec::new()));
        let s = sent.clone();
        terminal
            .on_pty_write(move |_t, data| s.borrow_mut().extend_from_slice(data))
            .unwrap();
        terminal.vt_write(b"\x1b[?2048h");
        // Setting the mode sends nothing; tai has to.
        assert!(sent.borrow().is_empty());
        // libghostty-vt's own report on resize is what tai's must match.
        terminal.resize(100, 30, 9, 18).unwrap();
        let expected = size_report((100, 30), (9, 18));
        assert_eq!(expected, "\x1b[48;30;100;540;900t");
        assert_eq!(*sent.borrow(), expected.as_bytes());
    }

    #[test]
    fn a_size_report_goes_once_when_a_program_sets_the_mode() {
        let mut terminal = new_terminal(80, 24, (10, 20)).unwrap();
        let mut in_band = InBandResize::default();
        assert_eq!(in_band.report(&terminal, (80, 24), (10, 20)).unwrap(), None);
        terminal.vt_write(b"\x1b[?2048h");
        let first = in_band.report(&terminal, (80, 24), (10, 20)).unwrap();
        assert_eq!(first.as_deref(), Some("\x1b[48;24;80;480;800t"));
        assert_eq!(in_band.report(&terminal, (80, 24), (10, 20)).unwrap(), None);
        // Off and on again: another report.
        terminal.vt_write(b"\x1b[?2048l");
        assert_eq!(in_band.report(&terminal, (80, 24), (10, 20)).unwrap(), None);
        terminal.vt_write(b"\x1b[?2048h");
        assert!(
            in_band
                .report(&terminal, (80, 24), (10, 20))
                .unwrap()
                .is_some()
        );
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
