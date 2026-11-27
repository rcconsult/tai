//! The mouse in a terminal, for tai and tai-gui alike (ADR 0014, step 3):
//! reports to a program that tracks the mouse, the selection (press, drag,
//! release, autoscroll past the edge, Shift-click extend), and the wheel
//! (reports, alternate scroll, scrollback). Each front end turns its
//! toolkit's input into an `Input` once a frame and calls
//! `Pointer::frame`; it keeps only that translation, its wheel units, the
//! cursor icon call, and anything its toolkit needs beyond (tai's Windows
//! mouse capture).
//!
//! The two front ends used to do this each their own way, and differed:
//! releases, presses outside the grid, the wheel to a tracking program,
//! alternate scroll and the selection, motion with Shift, the autoscroll
//! edge and the pointer shape. The rules here are one set, Ghostty's
//! where it has one (src/Surface.zig at 108cacb1).

use std::time::{Duration, Instant};

use libghostty_vt::{
    Terminal, key,
    mode::Mode,
    mouse,
    point::{Coordinate, Point},
    screen::Screen,
    selection::gesture::{
        Autoscroll, AutoscrollTickEvent, DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent,
    },
    terminal::ScrollViewport,
};

use crate::term::PADDING;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Presses closer together than this are a double or triple click; a
/// Shift-click further from the last press extends the selection.
pub const CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// How often a selection drag past the pane's top or bottom scrolls one
/// row, as Ghostty's `selection_scroll_ms` (src/termio/Thread.zig at
/// 108cacb1).
pub const AUTOSCROLL_EVERY: Duration = Duration::from_millis(15);

/// Rows one notch of a mouse wheel scrolls: Ghostty's default discrete
/// mouse-scroll-multiplier (src/config/Config.zig at 108cacb1). Each front
/// end turns its toolkit's wheel units into rows with it.
pub const WHEEL_ROWS: f32 = 3.0;

/// The modes that set the encoder's tracking mode and format.
const MOUSE_MODES: [Mode; 8] = [
    Mode::X10_MOUSE,
    Mode::NORMAL_MOUSE,
    Mode::BUTTON_MOUSE,
    Mode::ANY_MOUSE,
    Mode::UTF8_MOUSE,
    Mode::SGR_MOUSE,
    Mode::URXVT_MOUSE,
    Mode::SGR_PIXELS_MOUSE,
];

/// The buttons a program can get, in `Input::down`'s order.
const BUTTONS: [mouse::Button; 3] = [
    mouse::Button::Left,
    mouse::Button::Right,
    mouse::Button::Middle,
];

/// A button, as `Input::down` and `Click` count them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left = 0,
    Right = 1,
    Middle = 2,
}

/// A point in the pane, in logical units from its top-left corner. It may
/// be outside the pane, during a drag.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pos {
    pub x: f32,
    pub y: f32,
}

/// A button going down or up, where, and with which modifiers, in the
/// order they happened: a fast click or a tap can press and release in
/// one frame, and both count.
#[derive(Clone, Copy, Debug)]
pub struct Click {
    pub button: Button,
    pub pressed: bool,
    pub pos: Pos,
    pub mods: key::Mods,
}

/// One frame of the mouse, from the front end's toolkit.
#[derive(Clone, Debug)]
pub struct Input {
    pub clicks: Vec<Click>,
    /// Where the pointer is now, if the toolkit knows.
    pub pointer: Option<Pos>,
    /// Whether it moved this frame.
    pub moved: bool,
    /// The buttons held now, by `Button`.
    pub down: [bool; 3],
    /// The modifiers held now.
    pub mods: key::Mods,
    /// This frame's wheel travel in rows, up positive. Zero when the
    /// wheel isn't the pane's (tai-gui: the pointer isn't over it).
    pub wheel_rows: f32,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            clicks: Vec::new(),
            pointer: None,
            moved: false,
            down: [false; 3],
            mods: key::Mods::empty(),
            wheel_rows: 0.0,
        }
    }
}

/// The pane: its size and its cells', in logical units, and the scale to
/// surface pixels, which the protocol and the encoder use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pane {
    pub width: f32,
    pub height: f32,
    pub cell_width: f32,
    pub cell_height: f32,
    pub dpi_scale: f32,
    /// The cell size in surface pixels, as the terminal was told.
    pub cell_pixels: (u32, u32),
}

/// What a frame did that the front end acts on.
#[derive(Clone, Copy, Debug, Default)]
pub struct Outcome {
    /// The selection or the viewport changed, so the cells need drawing.
    pub redraw: bool,
    /// Call `frame` again within this, even without input: a drag rests
    /// past the edge and autoscrolls.
    pub tick_within: Option<Duration>,
}

/// The pointer's shape over the pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// The program has the mouse, or the pointer is off the grid.
    Default,
    /// Text can be selected.
    Text,
    /// A rectangle can be selected.
    Crosshair,
}

/// The mouse's state for one terminal.
pub struct Pointer {
    gesture: Gesture<'static>,
    press: PressEvent<'static>,
    release: ReleaseEvent<'static>,
    drag: DragEvent<'static>,
    autoscroll: AutoscrollTickEvent<'static>,
    encoder: mouse::Encoder<'static>,
    event: mouse::Event<'static>,
    /// The mouse modes and size the encoder was last given; setting either
    /// makes it forget the last reported cell, and every move within a
    /// cell was then reported (src/terminal/c/mouse_encode.zig at
    /// 108cacb1; popos-tai).
    encoder_for: Option<([bool; MOUSE_MODES.len()], mouse::EncoderSize)>,
    /// The buttons whose press went to the program, so their release goes
    /// there too, wherever the pointer is then.
    reported: [bool; 3],
    /// A left press started a selection that its release hasn't ended.
    selecting: bool,
    /// When the left button last pressed for a selection, for Shift-click.
    last_press: Option<Instant>,
    /// When the last autoscroll tick ran.
    last_autoscroll: Option<Instant>,
    /// Wheel travel not yet a whole row.
    pending_rows: f32,
    /// Gesture times count from here.
    epoch: Instant,
}

impl Pointer {
    pub fn new() -> Result<Self> {
        let mut press = PressEvent::new()?;
        press.set_repeat_interval(CLICK_INTERVAL)?;
        Ok(Self {
            gesture: Gesture::new()?,
            press,
            release: ReleaseEvent::new()?,
            drag: DragEvent::new()?,
            autoscroll: AutoscrollTickEvent::new()?,
            encoder: mouse::Encoder::new()?,
            event: mouse::Event::new()?,
            encoder_for: None,
            reported: [false; 3],
            selecting: false,
            last_press: None,
            last_autoscroll: None,
            pending_rows: 0.0,
            epoch: Instant::now(),
        })
    }

    /// One frame of the mouse: reports for the program go to `out`, the
    /// selection and the viewport change on `terminal`.
    pub fn frame(
        &mut self,
        terminal: &mut Terminal<'_, '_>,
        pane: Pane,
        input: &Input,
        out: &mut Vec<u8>,
    ) -> Result<Outcome> {
        let mut outcome = Outcome::default();
        let tracking = terminal.is_mouse_tracking()?;
        if !tracking {
            self.reported = [false; 3];
        }
        self.sync_encoder(terminal, pane)?;

        // Buttons, in the order they came.
        for click in &input.clicks {
            let index = click.button as usize;
            let selection_press = click.button == Button::Left
                && click.pressed
                && in_pane(pane, click.pos)
                && (!tracking || click.mods.contains(key::Mods::SHIFT));
            if selection_press {
                self.select_press(terminal, pane, click.pos, click.mods)?;
                outcome.redraw = true;
                continue;
            }
            if click.button == Button::Left && !click.pressed && self.selecting {
                // The frame's motion came before its release: drag to where
                // the button went up first, or a drag released in the same
                // frame it last moved would lose its end.
                if input.moved {
                    self.drag_to(terminal, pane, click.pos, click.mods)?;
                }
                self.select_release(terminal, pane, click.pos)?;
                outcome.redraw = true;
                continue;
            }
            // The rest is the program's, while it tracks the mouse: a press
            // in the pane without Shift (with Shift the mouse is the
            // terminal's), and the release of a press it got.
            let action = if click.pressed {
                if !tracking
                    || self.selecting
                    || !in_pane(pane, click.pos)
                    || click.mods.contains(key::Mods::SHIFT)
                {
                    continue;
                }
                self.reported[index] = true;
                mouse::Action::Press
            } else if self.reported[index] {
                self.reported[index] = false;
                mouse::Action::Release
            } else {
                continue;
            };
            // A button the program gets clears the terminal's selection,
            // and the gesture forgets its click, as Ghostty's
            // mouseButtonCallback does: the program handles the mouse now.
            if terminal.selection()?.is_some() {
                terminal.set_selection(None)?;
                outcome.redraw = true;
            }
            self.gesture.reset(terminal);
            self.encoder
                .set_any_button_pressed(self.reported.iter().any(|&r| r));
            self.event
                .set_mods(click.mods)
                .set_position(surface(pane, click.pos))
                .set_action(action)
                .set_button(Some(BUTTONS[index]));
            self.encode(out)?;
        }

        // Motion: the selection drag's, or the program's while it tracks
        // the mouse, without Shift, in the pane or during its drag.
        if let Some(pos) = input.pointer
            && input.moved
        {
            if self.selecting && input.down[Button::Left as usize] {
                self.drag_to(terminal, pane, pos, input.mods)?;
                outcome.redraw = true;
            } else {
                let dragging = self.reported.iter().any(|&r| r);
                if tracking
                    && !self.selecting
                    && !input.mods.contains(key::Mods::SHIFT)
                    && (dragging || in_pane(pane, pos))
                {
                    let button = BUTTONS
                        .iter()
                        .zip(input.down)
                        .find(|&(_, down)| down)
                        .map(|(&b, _)| b);
                    self.encoder.set_any_button_pressed(dragging);
                    self.event
                        .set_mods(input.mods)
                        .set_position(surface(pane, pos))
                        .set_action(mouse::Action::Motion)
                        .set_button(button);
                    self.encode(out)?;
                }
            }
        }

        // The wheel.
        let rows = take_rows(&mut self.pending_rows, input.wheel_rows);
        if rows != 0 {
            self.wheel(terminal, pane, input, rows, out, &mut outcome)?;
        }

        // Autoscroll while a selection drag rests past the edge.
        if self.selecting
            && input.down[Button::Left as usize]
            && let Some(pos) = input.pointer
        {
            if self.autoscroll_tick(terminal, pane, pos, input.mods)? {
                outcome.redraw = true;
            }
            if !matches!(self.gesture.autoscroll(terminal)?, Autoscroll::None) {
                outcome.tick_within = Some(AUTOSCROLL_EVERY);
            }
        } else {
            self.last_autoscroll = None;
        }
        Ok(outcome)
    }

    /// The pointer's shape at `pos` with `mods` held.
    pub fn shape(
        &self,
        terminal: &Terminal<'_, '_>,
        pane: Pane,
        pos: Option<Pos>,
        mods: key::Mods,
    ) -> Result<Shape> {
        let rectangle = rectangle_mods(mods);
        let shape = if rectangle {
            Shape::Crosshair
        } else {
            Shape::Text
        };
        if self.selecting {
            return Ok(shape);
        }
        let program = terminal.is_mouse_tracking()? && !mods.contains(key::Mods::SHIFT);
        let on_grid = pos.is_some_and(|pos| on_grid(terminal, pane, pos).unwrap_or(false));
        Ok(if program || !on_grid {
            Shape::Default
        } else {
            shape
        })
    }

    /// The wheel's rows: button 4 and 5 presses while the program tracks
    /// the mouse, arrow keys on the alternate screen under alternate scroll
    /// (mode 1007; how less and man scroll), else the scrollback, as
    /// Ghostty's scrollCallback does.
    fn wheel(
        &mut self,
        terminal: &mut Terminal<'_, '_>,
        pane: Pane,
        input: &Input,
        rows: isize,
        out: &mut Vec<u8>,
        outcome: &mut Outcome,
    ) -> Result<()> {
        let tracking = terminal.is_mouse_tracking()?;
        let alternate = !tracking
            && terminal.active_screen()? == Screen::Alternate
            && terminal.mode(Mode::ALT_SCROLL)?;
        if tracking || alternate {
            // The program scrolls; a selection, from a Shift-drag, goes.
            if terminal.selection()?.is_some() {
                terminal.set_selection(None)?;
                outcome.redraw = true;
            }
        }
        if tracking {
            let Some(pos) = input.pointer else {
                return Ok(());
            };
            self.event
                .set_mods(input.mods)
                .set_position(surface(pane, pos))
                .set_action(mouse::Action::Press)
                .set_button(Some(if rows > 0 {
                    mouse::Button::Four
                } else {
                    mouse::Button::Five
                }));
            // One press per row and no release, as Ghostty reports the
            // wheel: hx counts an SGR release of button 4 or 5 as another
            // scroll (popos-tai). Every row is a report, even within one
            // cell.
            self.encoder
                .set_any_button_pressed(self.reported.iter().any(|&r| r))
                .set_track_last_cell(false);
            for _ in 0..rows.unsigned_abs() {
                self.encode(out)?;
            }
            self.encoder.set_track_last_cell(true);
        } else if alternate {
            let seq = arrow_seq(rows > 0, terminal.mode(Mode::DECCKM)?);
            for _ in 0..rows.unsigned_abs() {
                out.extend_from_slice(seq);
            }
        } else {
            // The scrollback: negative is up, into history.
            terminal.scroll_viewport(ScrollViewport::Delta(-rows));
            outcome.redraw = true;
            // A drag in progress follows the scroll: the cell under the
            // resting pointer is a different one now.
            if self.selecting
                && let Some(pos) = input.pointer
            {
                self.drag_to(terminal, pane, pos, input.mods)?;
            }
        }
        Ok(())
    }

    /// Encode the event and add it to `out`. The encoder writes into a
    /// fresh buffer: libghostty-rs's encode_to_vec can fail when the buffer
    /// already holds bytes, because its retry reserves from the length
    /// ("out of space, 16 bytes required", seen in tai-gui).
    fn encode(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let mut buf = Vec::with_capacity(64);
        self.encoder.encode_to_vec(&self.event, &mut buf)?;
        out.extend_from_slice(&buf);
        Ok(())
    }

    /// Give the encoder the program's mouse modes and the pane's size, but
    /// only when they changed.
    fn sync_encoder(&mut self, terminal: &Terminal<'_, '_>, pane: Pane) -> Result<()> {
        let mut modes = [false; MOUSE_MODES.len()];
        for (on, mode) in modes.iter_mut().zip(MOUSE_MODES) {
            *on = terminal.mode(mode)?;
        }
        let key = (modes, encoder_size(pane));
        if self.encoder_for != Some(key) {
            self.encoder
                .set_options_from_terminal(terminal)
                .set_size(key.1)?
                .set_track_last_cell(true);
            self.encoder_for = Some(key);
        }
        Ok(())
    }

    /// A left press for the selection. Shift-click extends the selection
    /// instead of starting a new one, as Ghostty's mouseButtonCallback
    /// does: with a selection, a click the gesture remembers, and that
    /// click longer ago than a double click, the press continues the old
    /// click's drag.
    fn select_press(
        &mut self,
        terminal: &Terminal<'_, '_>,
        pane: Pane,
        pos: Pos,
        mods: key::Mods,
    ) -> Result<()> {
        let since_press = self.last_press.map(|at| at.elapsed());
        self.last_press = Some(Instant::now());
        self.selecting = true;
        if mods.contains(key::Mods::SHIFT)
            && terminal.selection()?.is_some()
            && self.gesture.click_count(terminal)? > 0
            && since_press.is_some_and(|since| since > CLICK_INTERVAL)
        {
            return self.drag_to(terminal, pane, pos, mods);
        }
        let grid_ref = terminal.grid_ref(Point::Viewport(viewport_cell(terminal, pane, pos)?))?;
        let selection = self
            .press
            .set_repeat_distance(pane.cell_width.into())?
            .set_time(self.epoch.elapsed())?
            .set_position(pos.x.into(), pos.y.into())?
            .apply(&mut self.gesture, terminal, grid_ref)?;
        terminal.set_selection(selection.as_ref())?;
        Ok(())
    }

    /// The selection's left release. Released off the grid is fine: no
    /// cell, no change.
    fn select_release(&mut self, terminal: &Terminal<'_, '_>, pane: Pane, pos: Pos) -> Result<()> {
        let grid_ref = viewport_cell(terminal, pane, pos)
            .ok()
            .and_then(|at| terminal.grid_ref(Point::Viewport(at)).ok());
        self.release.apply(&mut self.gesture, terminal, grid_ref)?;
        self.selecting = false;
        self.last_autoscroll = None;
        Ok(())
    }

    /// Extend the selection drag to `pos`. Past the pane's top or bottom
    /// the gesture asks to autoscroll.
    fn drag_to(
        &mut self,
        terminal: &Terminal<'_, '_>,
        pane: Pane,
        pos: Pos,
        mods: key::Mods,
    ) -> Result<()> {
        let grid_ref = terminal.grid_ref(Point::Viewport(viewport_cell(terminal, pane, pos)?))?;
        let selection = self
            .drag
            .set_rectangle(rectangle_mods(mods))?
            .set_position(pos.x.into(), pos.y.into())?
            .apply(
                &mut self.gesture,
                terminal,
                grid_ref,
                geometry(terminal, pane)?,
            )?;
        terminal.set_selection(selection.as_ref())?;
        Ok(())
    }

    /// While a drag is past the pane's top or bottom, scroll one row and
    /// extend the selection every AUTOSCROLL_EVERY, as Ghostty's
    /// selectionScrollTick does; the gesture has no timer of its own
    /// (include/ghostty/vt/selection.h). Returns whether it ticked.
    fn autoscroll_tick(
        &mut self,
        terminal: &Terminal<'_, '_>,
        pane: Pane,
        pos: Pos,
        mods: key::Mods,
    ) -> Result<bool> {
        if matches!(self.gesture.autoscroll(terminal)?, Autoscroll::None) {
            self.last_autoscroll = None;
            return Ok(false);
        }
        let now = Instant::now();
        if self
            .last_autoscroll
            .is_some_and(|last| now.duration_since(last) < AUTOSCROLL_EVERY)
        {
            return Ok(false);
        }
        self.last_autoscroll = Some(now);
        let selection = self
            .autoscroll
            .set_rectangle(rectangle_mods(mods))?
            .set_position(pos.x.into(), pos.y.into())?
            .apply(
                &mut self.gesture,
                terminal,
                viewport_cell(terminal, pane, pos)?,
                geometry(terminal, pane)?,
            )?;
        // None: nothing to scroll, or the pressed cell left the screen and
        // the gesture reset; the selection stays as it is.
        if let Some(selection) = selection {
            terminal.set_selection(Some(&selection))?;
        }
        Ok(true)
    }
}

/// Whether a drag selects a rectangle: Alt on macOS, Ctrl+Alt or Super+Alt
/// elsewhere, as in Ghostty.
pub fn rectangle_mods(mods: key::Mods) -> bool {
    let alt = mods.contains(key::Mods::ALT);
    if cfg!(target_os = "macos") {
        alt
    } else {
        alt && mods.intersects(key::Mods::CTRL | key::Mods::SUPER)
    }
}

/// Add `rows` of wheel travel to `pending` and take the whole rows out,
/// keeping the fraction for the next frame, as Ghostty's pending_scroll_y.
/// A change of direction drops the fraction left from the other way: a
/// trackpad's leftover made the next notch back scroll 2 rows, not 3
/// (macos-tai).
fn take_rows(pending: &mut f32, rows: f32) -> isize {
    if rows == 0.0 {
        return 0;
    }
    if pending.signum() != rows.signum() {
        *pending = 0.0;
    }
    *pending += rows;
    let whole = pending.trunc();
    *pending -= whole;
    whole as isize
}

/// The arrow key the wheel sends under alternate scroll, in the cursor key
/// mode (DECCKM) the program set.
fn arrow_seq(up: bool, application: bool) -> &'static [u8] {
    match (up, application) {
        (true, true) => b"\x1bOA",
        (false, true) => b"\x1bOB",
        (true, false) => b"\x1b[A",
        (false, false) => b"\x1b[B",
    }
}

fn in_pane(pane: Pane, pos: Pos) -> bool {
    (0.0..pane.width).contains(&pos.x) && (0.0..pane.height).contains(&pos.y)
}

/// Whether `pos` is over a cell, not the padding around the grid.
fn on_grid(terminal: &Terminal<'_, '_>, pane: Pane, pos: Pos) -> Result<bool> {
    let right = PADDING + f32::from(terminal.cols()?) * pane.cell_width;
    let bottom = PADDING + f32::from(terminal.rows()?) * pane.cell_height;
    Ok((PADDING..right).contains(&pos.x) && (PADDING..bottom).contains(&pos.y))
}

/// A point as the encoder takes it: surface pixels, which are physical
/// pixels (include/ghostty/vt/mouse/encoder.h), so SGR-Pixels reports (mode
/// 1016) share units with the size the terminal reports, as Ghostty's
/// mouse_encode.zig posToPixels does.
fn surface(pane: Pane, pos: Pos) -> mouse::Position {
    mouse::Position {
        x: pos.x * pane.dpi_scale,
        y: pos.y * pane.dpi_scale,
    }
}

/// The pane's geometry in surface pixels, as the encoder needs it to find
/// the cell under a point.
fn encoder_size(pane: Pane) -> mouse::EncoderSize {
    let (cell_width, cell_height) = pane.cell_pixels;
    let padding = (PADDING * pane.dpi_scale) as u32;
    mouse::EncoderSize {
        screen_width: (pane.width * pane.dpi_scale) as u32,
        screen_height: (pane.height * pane.dpi_scale) as u32,
        cell_width: cell_width.max(1),
        cell_height: cell_height.max(1),
        padding_top: padding,
        padding_bottom: padding,
        padding_left: padding,
        padding_right: padding,
    }
}

/// The pane as the selection gesture measures it. The gesture autoscrolls
/// up at y <= 1 and down only past `screen_height - 1`
/// (src/terminal/SelectionGesture.zig at 108cacb1). Giving it a height one
/// less makes the pane's last pixel row count for down as its first does
/// for up, which tai needs where its toolkit reports nothing past the
/// window (X11 under Xvfb, popos-tai). screen_height feeds only that test.
fn geometry(terminal: &Terminal<'_, '_>, pane: Pane) -> Result<Geometry> {
    Ok(Geometry {
        columns: terminal.cols()?.into(),
        cell_width: pane.cell_width as u32,
        padding_left: PADDING as u32,
        screen_height: (pane.height as u32).saturating_sub(1),
    })
}

/// The viewport cell under `pos`, clamped to the grid: grid_ref rejects a
/// point one past the last row or column, and a drag goes past it.
fn viewport_cell(terminal: &Terminal<'_, '_>, pane: Pane, pos: Pos) -> Result<Coordinate> {
    let cols = terminal.cols()?;
    let rows = terminal.rows()?;
    let cell = |v: f32, size: f32, count: u32| {
        if v > PADDING {
            ((v - PADDING) / size).clamp(0.0, count.saturating_sub(1) as f32) as u32
        } else {
            0
        }
    };
    Ok(Coordinate {
        x: cell(pos.x, pane.cell_width, u32::from(cols)) as u16,
        y: cell(pos.y, pane.cell_height, u32::from(rows)),
    })
}

#[cfg(test)]
mod tests {
    use libghostty_vt::{Terminal, key, mouse};

    use super::{Button, Click, Input, Pane, Pointer, Pos, arrow_seq, encoder_size, take_rows};
    use crate::term::PADDING;

    /// An 80x24 pane of 10x20 cells at 1x, and a terminal to match.
    fn pane_and_terminal() -> (Pane, Terminal<'static, 'static>) {
        let pane = Pane {
            width: 2.0 * PADDING + 800.0,
            height: 2.0 * PADDING + 480.0,
            cell_width: 10.0,
            cell_height: 20.0,
            dpi_scale: 1.0,
            cell_pixels: (10, 20),
        };
        let mut terminal = Terminal::new(80, 24).unwrap();
        terminal.set_scrollback_max_lines(Some(100)).unwrap();
        terminal.resize(80, 24, 10, 20).unwrap();
        (pane, terminal)
    }

    /// The middle of cell (col, row).
    fn at(col: f32, row: f32) -> Pos {
        Pos {
            x: PADDING + (col + 0.5) * 10.0,
            y: PADDING + (row + 0.5) * 20.0,
        }
    }

    fn click(button: Button, pressed: bool, pos: Pos, mods: key::Mods) -> Click {
        Click {
            button,
            pressed,
            pos,
            mods,
        }
    }

    #[test]
    fn a_tracking_program_gets_the_press_and_its_release_and_the_selection_goes() {
        let (pane, mut terminal) = pane_and_terminal();
        terminal.vt_write(b"hello\x1b[?1000h\x1b[?1006h");
        let mut pointer = Pointer::new().unwrap();
        let none = key::Mods::empty();
        // A Shift-drag selects, even with the program tracking. It ends
        // past the middle of the o's cell, which takes the cell.
        let end_of_o = Pos {
            x: PADDING + 4.8 * 10.0,
            y: PADDING + 10.0,
        };

        let mut out = Vec::new();
        let shift = key::Mods::SHIFT;
        let input = Input {
            clicks: vec![click(Button::Left, true, at(0.0, 0.0), shift)],
            pointer: Some(at(0.0, 0.0)),
            down: [true, false, false],
            mods: shift,
            ..Default::default()
        };
        pointer
            .frame(&mut terminal, pane, &input, &mut out)
            .unwrap();
        let input = Input {
            clicks: vec![click(Button::Left, false, end_of_o, shift)],
            pointer: Some(end_of_o),
            moved: true,
            down: [true, false, false],
            mods: shift,
            ..Default::default()
        };
        pointer
            .frame(&mut terminal, pane, &input, &mut out)
            .unwrap();
        assert!(out.is_empty(), "nothing went to the program: {out:?}");
        assert_eq!(
            crate::selection::text(&terminal).unwrap().as_deref(),
            Some("hello")
        );
        // A plain click goes to the program, press and release, and the
        // selection goes.
        let input = Input {
            clicks: vec![
                click(Button::Left, true, at(3.0, 1.0), none),
                click(Button::Left, false, at(3.0, 1.0), none),
            ],
            pointer: Some(at(3.0, 1.0)),
            ..Default::default()
        };
        pointer
            .frame(&mut terminal, pane, &input, &mut out)
            .unwrap();
        assert_eq!(out, b"\x1b[<0;4;2M\x1b[<0;4;2m");
        assert!(terminal.selection().unwrap().is_none());
        // A press off the pane isn't the program's, nor is its release.
        out.clear();
        let off = Pos { x: -5.0, y: 10.0 };
        let input = Input {
            clicks: vec![
                click(Button::Left, true, off, none),
                click(Button::Left, false, off, none),
            ],
            pointer: Some(off),
            ..Default::default()
        };
        pointer
            .frame(&mut terminal, pane, &input, &mut out)
            .unwrap();
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn the_wheel_is_presses_for_a_tracking_program_and_arrows_under_alternate_scroll() {
        let (pane, mut terminal) = pane_and_terminal();
        let mut pointer = Pointer::new().unwrap();
        let wheel = |rows: f32| Input {
            pointer: Some(at(3.0, 1.0)),
            wheel_rows: rows,
            ..Default::default()
        };
        // Tracking: one press per row, no release.
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        let mut out = Vec::new();
        pointer
            .frame(&mut terminal, pane, &wheel(3.0), &mut out)
            .unwrap();
        assert_eq!(out, b"\x1b[<64;4;2M".repeat(3));
        // The alternate screen without tracking: arrow keys.
        terminal.vt_write(b"\x1b[?1000l\x1b[?1049h");
        out.clear();
        pointer
            .frame(&mut terminal, pane, &wheel(-2.0), &mut out)
            .unwrap();
        assert_eq!(out, b"\x1b[B".repeat(2));
    }

    #[test]
    fn fractions_of_a_row_carry_until_the_direction_changes() {
        let mut pending = 0.0;
        // A trackpad's small steps add up instead of rounding to nothing.
        let steps: isize = (0..10).map(|_| take_rows(&mut pending, 0.25)).sum();
        assert_eq!(steps, 2);
        assert!((pending - 0.5).abs() < 1e-6);
        // Turning around drops the half row left from going up.
        assert_eq!(take_rows(&mut pending, -0.75), 0);
        assert!((pending + 0.75).abs() < 1e-6);
        assert_eq!(take_rows(&mut pending, 0.0), 0);
        // Three notches' rows at once all count.
        assert_eq!(take_rows(&mut pending, 9.0), 9);
    }

    #[test]
    fn alternate_scroll_sends_the_arrow_for_the_cursor_key_mode() {
        assert_eq!(arrow_seq(true, false), b"\x1b[A");
        assert_eq!(arrow_seq(false, false), b"\x1b[B");
        assert_eq!(arrow_seq(true, true), b"\x1bOA");
        assert_eq!(arrow_seq(false, true), b"\x1bOB");
    }

    /// The encoder finds the cell under a point from `encoder_size`, at 1x
    /// and on a 2x display, where points are scaled to surface pixels.
    #[test]
    fn a_press_reports_the_cell_under_the_pointer() {
        for scale in [1.0_f32, 2.0] {
            let (cols, rows) = (78, 29);
            let cell_pixels = ((10.0 * scale) as u32, (20.0 * scale) as u32);
            let pane = Pane {
                width: 800.0,
                height: 600.0,
                cell_width: 10.0,
                cell_height: 20.0,
                dpi_scale: scale,
                cell_pixels,
            };
            let mut terminal = Terminal::new(cols, rows).unwrap();
            terminal
                .resize(cols, rows, cell_pixels.0, cell_pixels.1)
                .unwrap();
            // Normal tracking, SGR format.
            terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
            let mut encoder = mouse::Encoder::new().unwrap();
            encoder
                .set_options_from_terminal(&terminal)
                .set_size(encoder_size(pane))
                .unwrap();
            let mut event = mouse::Event::new().unwrap();
            // Column 3 and row 1 from zero: past the padding, mid-cell.
            let (x, y) = (PADDING + 3.5 * 10.0, PADDING + 1.5 * 20.0);
            event
                .set_action(mouse::Action::Press)
                .set_button(Some(mouse::Button::Left))
                .set_position(mouse::Position {
                    x: x * scale,
                    y: y * scale,
                });
            let mut out = Vec::new();
            encoder.encode_to_vec(&event, &mut out).unwrap();
            assert_eq!(out, b"\x1b[<0;4;2M", "at {scale}x");
        }
    }
}
