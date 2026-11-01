//! A terminal pane: a libghostty-vt terminal on tai's pty, drawn by the
//! wgpu cell pass (`gpu.rs`), typed into through egui's events.
//!
//! Text, colours, the cursor, typing, paste, scrollback with the wheel,
//! resizing with the pane, and mouse selection with copy and the corner
//! badge. Images and mouse reporting stay in the macroquad build for now.

use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use eframe::egui;
use libghostty_vt::{
    Terminal, TerminalOptions,
    key::{self, Key},
    render::{CellIterator, Dirty, RenderState, RowIterator},
    screen::CellWide,
    selection::gesture::{DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent},
    style::RgbColor,
    terminal::{Mode, Point, PointCoordinate, ScrollViewport, SizeReportSize},
};
use macroquad::color::Color;

use crate::{
    cells::{GlyphAtlas, Instance},
    fonts::{Coverage, Face},
    gpu::{AtlasUpload, CellFrame},
    pty::{Child, Pty, PtyError},
    selection, shortcuts,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Space between the pane's edge and the grid, as in the macroquad build.
const PADDING: f32 = 6.0;
/// Space added to the glyph height for each row, as in the macroquad build.
const ROW_GAP: f32 = 12.0;
/// How long one frame may spend feeding pty output to the terminal; it
/// stops earlier once the queued output runs out. The macroquad build's
/// PARSE_DEADLINE: 8 ms halved flood throughput there on Linux, and here
/// tai-gui's flood throughput tracked its frame rate (popos-tai).
const PARSE_BUDGET: Duration = Duration::from_millis(14);
/// How long to wait for the shell's echo after typing, so it's drawn in
/// the same frame, as the macroquad build's ECHO_WAIT does. Without it a
/// keystroke took two frames (popos-tai's typing-lag probe).
const ECHO_WAIT: Duration = Duration::from_millis(4);

/// The pane's size and cell metrics. The pty modules size the pseudo
/// terminal from it (`crate::Dimensions`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dimensions {
    pub width: f32,
    pub height: f32,
    pub cell_width: f32,
    pub cell_height: f32,
    /// Offset from a cell's top edge to the text baseline.
    pub baseline: f32,
    pub font_size: u16,
    pub dpi_scale: f32,
}

impl Dimensions {
    /// Cell metrics measured on the primary font's 'M', as the macroquad
    /// build measures them, for a pane `size` points big.
    pub fn new(coverage: &Coverage, font_size: u16, dpi_scale: f32, size: egui::Vec2) -> Self {
        let px = (f32::from(font_size) * dpi_scale).ceil();
        let m = coverage.font(Face::Primary).metrics('M', px);
        let glyph_height = (m.height as f32 / dpi_scale).max(1.0);
        let ascent = (m.height as f32 + m.ymin as f32) / dpi_scale;
        Self {
            width: size.x,
            height: size.y,
            cell_width: (m.advance_width / dpi_scale).max(1.0),
            cell_height: glyph_height + ROW_GAP,
            baseline: ROW_GAP / 2.0 + ascent,
            font_size,
            dpi_scale,
        }
    }

    pub fn grid_size(self) -> (u16, u16) {
        let cols = ((self.width - 2.0 * PADDING) / self.cell_width).max(1.0) as u16;
        let rows = ((self.height - 2.0 * PADDING) / self.cell_height).max(1.0) as u16;
        (cols, rows)
    }

    pub fn cell_pixels(self) -> (u32, u32) {
        (
            (self.cell_width * self.dpi_scale) as u32,
            (self.cell_height * self.dpi_scale) as u32,
        )
    }

    /// Whole cells only, as the macroquad build reports it.
    pub fn grid_pixels(self) -> (u16, u16) {
        let (cols, rows) = self.grid_size();
        let (cell_w, cell_h) = self.cell_pixels();
        let px = |cells: u16, cell: u32| (u32::from(cells) * cell).min(u32::from(u16::MAX)) as u16;
        (px(cols, cell_w), px(rows, cell_h))
    }

    #[cfg(unix)]
    pub fn to_winsize(self) -> nix::pty::Winsize {
        let (cols, rows) = self.grid_size();
        let (xpixel, ypixel) = self.grid_pixels();
        nix::pty::Winsize {
            ws_col: cols,
            ws_row: rows,
            ws_xpixel: xpixel,
            ws_ypixel: ypixel,
        }
    }
}

pub struct TerminalPane {
    terminal: Terminal<'static, 'static>,
    pty: Rc<Pty>,
    child: Child,
    grid_size: Rc<Cell<(u16, u16)>>,
    cell_pixels: Rc<Cell<(u32, u32)>>,
    dims: Dimensions,
    coverage: Coverage,
    render_state: RenderState<'static>,
    row_it: RowIterator<'static>,
    cell_it: CellIterator<'static>,
    key_encoder: key::Encoder<'static>,
    key_event: key::Event<'static>,
    atlas: GlyphAtlas,
    /// The last frame's instances, reused while nothing changed.
    instances: Arc<Vec<Instance>>,
    built_for: Option<(Dimensions, Option<(u16, u16)>)>,
    /// Each viewport row's instances as last built, so a frame rebuilds
    /// only the rows that changed, as Ghostty's `rebuildRow` does
    /// (src/renderer/generic.zig at a887df42) and the macroquad build's
    /// cell renderer does (ADR 0008).
    rows: Vec<RowInstances>,
    /// Typed and pasted bytes waiting to be written.
    response: Vec<u8>,
    /// Set by the pty's reader when it has woken egui, cleared by pump.
    wake_pending: Arc<AtomicBool>,
    /// Mouse selection, through libghostty-vt's gesture API as in the
    /// macroquad build (`SelectionState` in src/main.rs).
    gesture: Gesture<'static>,
    press: PressEvent<'static>,
    release: ReleaseEvent<'static>,
    drag: DragEvent<'static>,
    /// Gesture times count from here.
    epoch: Instant,
    /// Whether a press in the pane started a selection drag.
    selecting: bool,
    /// The selection changed this frame, so the cells must be redrawn: the
    /// render state's dirty flags track contents, not the selection.
    selection_changed: bool,
    /// The selection badge's text, while there is a selection.
    badge: Option<String>,
    /// Copy was asked for; done in `ui`, which has the egui context.
    copy_requested: bool,
}

impl TerminalPane {
    pub fn new(coverage: Coverage, ctx: egui::Context) -> Result<Self> {
        let dpi_scale = ctx.pixels_per_point();
        let font_size = shortcuts::DEFAULT_FONT_SIZE;
        let dims = Dimensions::new(&coverage, font_size, dpi_scale, egui::vec2(800.0, 600.0));
        let (cols, rows) = dims.grid_size();
        let (pty, child) = Pty::new(dims)?;
        // egui sleeps until something happens; output is something.
        // One wake per frame at most: the reader calls this for every
        // chunk, about 40,000 times a second during a flood on Linux.
        let wake_pending = Arc::new(AtomicBool::new(false));
        let pending = Arc::clone(&wake_pending);
        pty.wake_on_output(move || {
            if !pending.swap(true, Ordering::AcqRel) {
                ctx.request_repaint();
            }
        });
        let pty = Rc::new(pty);
        let grid_size = Rc::new(Cell::new((cols, rows)));
        let cell_pixels = Rc::new(Cell::new(dims.cell_pixels()));

        let mut terminal = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: 1000,
        })?;
        let (cell_w, cell_h) = dims.cell_pixels();
        terminal.resize(cols, rows, cell_w, cell_h)?;
        terminal
            .on_pty_write({
                let pty = Rc::clone(&pty);
                move |_t, data| pty.reply(data)
            })?
            .on_size({
                let (gs, cp) = (Rc::clone(&grid_size), Rc::clone(&cell_pixels));
                move |_t| {
                    let (columns, rows) = gs.get();
                    let (cell_width, cell_height) = cp.get();
                    Some(SizeReportSize {
                        rows,
                        columns,
                        cell_width,
                        cell_height,
                    })
                }
            })?
            .on_xtversion(|_t| Some("tai"))?;

        Ok(Self {
            terminal,
            pty,
            child,
            grid_size,
            cell_pixels,
            dims,
            coverage,
            render_state: RenderState::new()?,
            row_it: RowIterator::new()?,
            cell_it: CellIterator::new()?,
            key_encoder: key::Encoder::new()?,
            key_event: key::Event::new()?,
            atlas: GlyphAtlas::new(crate::cells::ATLAS_MAX_HEIGHT),
            instances: Arc::new(Vec::new()),
            built_for: None,
            rows: Vec::new(),
            response: Vec::new(),
            wake_pending,
            gesture: Gesture::new()?,
            press: {
                let mut press = PressEvent::new()?;
                press.set_repeat_interval(Duration::from_millis(500))?;
                press
            },
            release: ReleaseEvent::new()?,
            drag: DragEvent::new()?,
            epoch: Instant::now(),
            selecting: false,
            selection_changed: false,
            badge: None,
            copy_requested: false,
        })
    }

    pub fn exited(&self) -> bool {
        !matches!(self.child, Child::Active(_))
    }

    /// Feed the child's output to the terminal. Runs every frame, whether
    /// or not the pane is visible, so a hidden shell never blocks on a full
    /// pty. Returns whether anything arrived.
    pub fn pump(&mut self) -> bool {
        // Clear before reading, so output that arrives after the read
        // wakes the next frame.
        self.wake_pending.store(false, Ordering::Release);
        if self.exited() {
            self.child.try_reap();
            return false;
        }
        let mut got = false;
        let terminal = &mut self.terminal;
        let mut sink = |bytes: &[u8]| {
            got = true;
            terminal.vt_write(bytes);
        };
        match self.pty.read(&mut sink, Instant::now() + PARSE_BUDGET) {
            Ok(()) => {}
            Err(PtyError::EndOfStream) => self.child.mark_exited(),
            Err(PtyError::OtherError(e)) => {
                tracing::error!("failed to read from pty: {e}");
                self.child.mark_exited();
            }
        }
        got
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
        if response.clicked() || !ui.ctx().memory(|m| m.focused().is_some()) {
            response.request_focus();
        }
        // Keep Tab, arrows and Escape for the shell instead of egui's focus
        // navigation.
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            )
        });

        let dpi_scale = ui.ctx().pixels_per_point();
        if self.dims.dpi_scale != dpi_scale
            || self.dims.width != rect.width()
            || self.dims.height != rect.height()
        {
            let dims = Dimensions::new(&self.coverage, self.dims.font_size, dpi_scale, rect.size());
            if dims.dpi_scale != self.dims.dpi_scale {
                self.atlas.clear();
            }
            self.resize(dims);
        }

        if let Err(e) = self.handle_pointer(ui, rect) {
            tracing::warn!("selection: {e}");
        }
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
        }

        if response.has_focus() && !self.exited() {
            let (events, held) = ui.input(|i| (i.events.clone(), i.modifiers));
            for event in &events {
                if let Err(e) = self.handle_event(event, held) {
                    tracing::warn!("input: {e}");
                }
            }
            if !self.response.is_empty() {
                // Typing returns to the live screen, as in most terminals.
                self.terminal.scroll_viewport(ScrollViewport::Bottom);
                self.pty.write(&self.response);
                self.response.clear();
                self.pty.wait_readable(ECHO_WAIT);
                self.pump();
            }
        }
        if std::mem::take(&mut self.copy_requested) {
            match selection::text(&self.terminal) {
                Ok(Some(text)) => ui.ctx().copy_text(text),
                Ok(None) => {}
                Err(e) => tracing::warn!("copy: {e}"),
            }
        }
        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            let lines = (scroll / self.dims.cell_height).round() as isize;
            if lines != 0 {
                self.terminal.scroll_viewport(ScrollViewport::Delta(-lines));
            }
        }

        let selection_changed = std::mem::take(&mut self.selection_changed);
        match self.build(selection_changed) {
            Ok(rebuilt) => {
                if rebuilt {
                    // Output can move or clear the selection too.
                    self.badge = selection::text(&self.terminal)
                        .ok()
                        .flatten()
                        .map(|text| selection::summary(&text));
                }
            }
            Err(e) => tracing::warn!("render: {e}"),
        }
        let atlas = self.atlas.take_dirty_rows().map(|(start, end)| {
            let (w, h) = self.atlas.size();
            let row = (w * 4) as usize;
            AtlasUpload {
                size: (w, h),
                first: start,
                pixels: self.atlas.pixels()[start as usize * row..end.min(h) as usize * row]
                    .to_vec(),
            }
        });
        ui.painter()
            .add(eframe::egui_wgpu::Callback::new_paint_callback(
                rect,
                CellFrame {
                    view: rect.size(),
                    instances: Arc::clone(&self.instances),
                    atlas,
                },
            ));
        if let Some(badge) = &self.badge {
            draw_badge(ui, rect, badge);
        }
        if self.exited() {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "[process exited]",
                egui::FontId::proportional(16.0),
                egui::Color32::GRAY,
            );
        }
    }

    fn resize(&mut self, dims: Dimensions) {
        self.dims = dims;
        let (cols, rows) = dims.grid_size();
        let (cell_w, cell_h) = dims.cell_pixels();
        if (cols, rows) == self.grid_size.get() && (cell_w, cell_h) == self.cell_pixels.get() {
            return;
        }
        self.grid_size.set((cols, rows));
        self.cell_pixels.set((cell_w, cell_h));
        if let Err(e) = self.terminal.resize(cols, rows, cell_w, cell_h) {
            tracing::warn!("resize: {e}");
        }
        self.pty.resize(dims);
    }

    /// Start, extend and end a selection from the primary button, as the
    /// macroquad build does. While a program tracks the mouse the buttons
    /// are its, except with Shift held; this pane doesn't report the mouse
    /// to programs yet, so without Shift nothing happens there.
    fn handle_pointer(&mut self, ui: &egui::Ui, rect: egui::Rect) -> Result<()> {
        let (pos, pressed, down, released, moved, held) = ui.input(|i| {
            (
                i.pointer.interact_pos(),
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.primary_released(),
                i.pointer.delta() != egui::Vec2::ZERO,
                i.modifiers,
            )
        });
        let Some(pos) = pos else {
            return Ok(());
        };
        let local = pos - rect.min;
        if pressed && rect.contains(pos) {
            if self.terminal.is_mouse_tracking()? && !held.shift {
                return Ok(());
            }
            let grid_ref = self.terminal.grid_ref(self.viewport_point(local)?)?;
            let selection = self
                .press
                .set_repeat_distance(self.dims.cell_width.into())?
                .set_time(self.epoch.elapsed())?
                .set_position(local.x.into(), local.y.into())?
                .apply(&mut self.gesture, &self.terminal, grid_ref)?;
            self.terminal.set_selection(selection.as_ref())?;
            self.selecting = true;
            self.selection_changed = true;
        } else if self.selecting && down && moved {
            let grid_ref = self.terminal.grid_ref(self.viewport_point(local)?)?;
            let geometry = Geometry {
                columns: self.terminal.cols()?.into(),
                cell_width: self.dims.cell_width as u32,
                padding_left: PADDING as u32,
                screen_height: self.dims.height as u32,
            };
            let selection = self
                .drag
                .set_rectangle(rectangle_mods(mods_from(held)))?
                .set_position(local.x.into(), local.y.into())?
                .apply(&mut self.gesture, &self.terminal, grid_ref, geometry)?;
            self.terminal.set_selection(selection.as_ref())?;
            self.selection_changed = true;
        } else if self.selecting && released {
            // Released outside the grid is fine: no cell, no change.
            let grid_ref = self
                .viewport_point(local)
                .ok()
                .and_then(|p| self.terminal.grid_ref(p).ok());
            self.release
                .apply(&mut self.gesture, &self.terminal, grid_ref)?;
            self.selecting = false;
            self.selection_changed = true;
        }
        Ok(())
    }

    /// The viewport cell under a point in the pane, clamped to the grid as
    /// the macroquad build's `mouse_viewport_point` does: grid_ref rejects
    /// a point one past the last row or column.
    fn viewport_point(&self, local: egui::Vec2) -> Result<Point> {
        let cols = self.terminal.cols()?;
        let rows = self.terminal.rows()?;
        let cell = |v: f32, size: f32, count: u32| {
            if v > PADDING {
                ((v - PADDING) / size).clamp(0.0, count.saturating_sub(1) as f32) as u32
            } else {
                0
            }
        };
        Ok(Point::Viewport(PointCoordinate {
            x: cell(local.x, self.dims.cell_width, u32::from(cols)) as u16,
            y: cell(local.y, self.dims.cell_height, u32::from(rows)),
        }))
    }

    /// Rebuild the instances when the screen, the pane, the cursor or the
    /// selection changed. Returns whether it rebuilt. Only dirty rows and
    /// the rows the cursor left and entered are rebuilt, unless everything
    /// must be: a new size, a full redraw from the terminal, or a changed
    /// selection (the dirty flags track contents, not the selection).
    fn build(&mut self, force: bool) -> Result<bool> {
        let snapshot = self.render_state.update(&self.terminal)?;
        let cursor = if snapshot.cursor_visible()? {
            snapshot.cursor_viewport()?.map(|vp| (vp.x, vp.y))
        } else {
            None
        };
        let dirty = snapshot.dirty()?;
        let key = Some((self.dims, cursor));
        if !force && dirty == Dirty::Clean && self.built_for == key {
            return Ok(false);
        }
        let old_cursor_row = self.built_for.and_then(|(_, c)| c).map(|(_, y)| y);
        let full = force
            || dirty == Dirty::Full
            || self.built_for.is_none_or(|(dims, _)| dims != self.dims);
        self.built_for = key;
        let dims = self.dims;
        let colors = snapshot.colors()?;
        let cursor_color = colors.cursor.unwrap_or(colors.foreground);
        let mut text = String::with_capacity(16);

        let mut row_it = self.row_it.update(&snapshot)?;
        let mut y = PADDING;
        let mut row_index: u16 = 0;
        while let Some(row) = row_it.next() {
            let index = usize::from(row_index);
            if self.rows.len() <= index {
                self.rows.resize_with(index + 1, RowInstances::default);
            }
            let cursor_row = cursor.map(|(_, cy)| cy);
            let rebuild = full
                || row.dirty()?
                || old_cursor_row == Some(row_index)
                || cursor_row == Some(row_index);
            if rebuild {
                let cached = &mut self.rows[index];
                cached.bg.clear();
                cached.text.clear();
                let mut cell_it = self.cell_it.update(row)?;
                let mut x = PADDING;
                let mut col: u16 = 0;
                while let Some(cell) = cell_it.next() {
                    let graphemes = cell.graphemes_len()?;
                    let selected = cell.is_selected()?;
                    let mut fg = cell.fg_color()?.unwrap_or(colors.foreground);
                    let bg = cell.bg_color()?;
                    let mut has_bg = bg.is_some();
                    let mut bg = bg.unwrap_or(colors.background);
                    let mut bold = false;
                    if graphemes > 0 && cell.has_styling()? {
                        let style = cell.style()?;
                        if style.inverse {
                            std::mem::swap(&mut fg, &mut bg);
                            has_bg = true;
                        }
                        bold = style.bold;
                    }
                    // Selected cells are drawn in reverse video, as in the
                    // macroquad build (walk_row in src/main.rs).
                    if selected {
                        std::mem::swap(&mut fg, &mut bg);
                        has_bg = true;
                    }
                    let cell_rect = [x, y, dims.cell_width, dims.cell_height];
                    if cursor == Some((col, row_index)) {
                        cached
                            .bg
                            .push(Instance::solid(cell_rect, rgba(cursor_color)));
                        fg = colors.background;
                    } else if has_bg {
                        cached.bg.push(Instance::solid(cell_rect, rgba(bg)));
                    }
                    if graphemes > 0 {
                        cell.graphemes_utf8(&mut text)?;
                        // VS15/VS16 have no glyph (see walk_row in src/main.rs).
                        text.retain(|ch| !matches!(ch, '\u{fe0e}' | '\u{fe0f}'));
                        let face = text
                            .chars()
                            .next()
                            .map_or(Face::Primary, |ch| self.coverage.face_for(ch));
                        if face != Face::Blank && !text.is_empty() {
                            // A wide character is drawn two cells wide, as
                            // Ghostty's constraintWidth does for grid width 2
                            // (src/renderer/cell.zig at a887df42).
                            let wide = cell.raw_cell()?.wide()? == CellWide::Wide;
                            let place =
                                self.coverage
                                    .place(face, dims.font_size, if wide { 2 } else { 1 });
                            let fg = rgba(fg);
                            self.atlas.push_text(
                                &mut cached.text,
                                self.coverage.font(face),
                                face,
                                &text,
                                x,
                                y + dims.baseline - place.rise,
                                place.font_size,
                                dims.dpi_scale,
                                Color::from_rgba(fg[0], fg[1], fg[2], fg[3]),
                                bold,
                            );
                        }
                    }
                    x += dims.cell_width;
                    col += 1;
                }
                row.set_dirty(false)?;
            }
            y += dims.cell_height;
            row_index += 1;
        }
        self.rows.truncate(usize::from(row_index));
        snapshot.set_dirty(Dirty::Clean)?;

        // The background, every row's cell backgrounds, then every row's
        // text, so no background covers a glyph that overhangs its cell.
        let total = 1 + self
            .rows
            .iter()
            .map(|r| r.bg.len() + r.text.len())
            .sum::<usize>();
        let mut out = Vec::with_capacity(total);
        out.push(Instance::solid(
            [0.0, 0.0, dims.width, dims.height],
            rgba(colors.background),
        ));
        for row in &self.rows {
            out.extend_from_slice(&row.bg);
        }
        for row in &self.rows {
            out.extend_from_slice(&row.text);
        }
        self.instances = Arc::new(out);
        Ok(true)
    }

    fn handle_event(&mut self, event: &egui::Event, held: egui::Modifiers) -> Result<()> {
        let macos = cfg!(target_os = "macos");
        match event {
            // Alt+key also arrives as its plain text; the key event sends
            // the chord (ESC + key).
            egui::Event::Text(_) if held.alt && !held.ctrl => {}
            egui::Event::Text(text) => self.response.extend_from_slice(text.as_bytes()),
            egui::Event::Paste(text) => {
                let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE)?;
                self.response
                    .extend(shortcuts::encode_paste(text, bracketed)?);
            }
            // egui-winit turns Ctrl+C/X (Cmd on macOS) into Copy and Cut
            // events with no key event. Off macOS, Ctrl+C belongs to the
            // shell (shortcuts.rs), so send it on. Copying needs a
            // selection, which this prototype doesn't have yet.
            // Copy is Ctrl+Shift+C off macOS and Cmd+C on it (shortcuts.rs);
            // egui reports both, and plain Ctrl+C, as Copy.
            egui::Event::Copy if macos || held.shift => self.copy_requested = true,
            egui::Event::Copy => self.encode_key(Key::C, 'c', key::Mods::CTRL, None)?,
            egui::Event::Cut if !macos => self.encode_key(Key::X, 'x', key::Mods::CTRL, None)?,
            egui::Event::Key {
                key,
                pressed: true,
                repeat: _,
                modifiers,
                ..
            } => {
                let mods = mods_from(*modifiers);
                if let Some((k, ucp)) = key_for(*key) {
                    // Plain printable keys arrive as Text too; send only
                    // chords and keys without text from here.
                    let chord =
                        mods.intersects(key::Mods::CTRL | key::Mods::ALT | key::Mods::SUPER);
                    if chord
                        || ucp == '\0'
                        || matches!(
                            k,
                            Key::Enter | Key::Tab | Key::Space | Key::Backspace | Key::Escape
                        )
                    {
                        if matches!(k, Key::Space) && !chord {
                            return Ok(());
                        }
                        self.encode_key(k, ucp, mods, None)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn encode_key(&mut self, k: Key, ucp: char, mods: key::Mods, text: Option<&str>) -> Result<()> {
        let mut consumed = key::Mods::empty();
        if ucp != '\0' && mods.contains(key::Mods::SHIFT) {
            consumed |= key::Mods::SHIFT;
        }
        self.key_event
            .set_action(key::Action::Press)
            .set_key(k)
            .set_mods(mods)
            .set_consumed_mods(consumed)
            .set_unshifted_codepoint(ucp)
            .set_utf8(text);
        let before = self.response.len();
        // libghostty-vt 0.2.2's encode_to_vec retries a too-small buffer
        // with reserve(required - remaining), and reserve counts from the
        // length, so with bytes already queued the retry fails too and the
        // key is lost ("out of space, 16 bytes required"). Leave room for
        // any key sequence up front.
        self.response.reserve(64);
        self.key_encoder
            .set_options_from_terminal(&self.terminal)
            .encode_to_vec(&self.key_event, &mut self.response)?;
        // The encoder sends nothing for Ctrl+[ ; send the classic C0 byte,
        // as the macroquad build does.
        if self.response.len() == before && mods == key::Mods::CTRL {
            let upper = ucp.to_ascii_uppercase();
            if ('@'..='_').contains(&upper) {
                self.response.push(upper as u8 & 0x1F);
            }
        }
        Ok(())
    }
}

/// Whether a drag selects a rectangle: Alt on macOS, Ctrl+Alt or
/// Super+Alt elsewhere, as in the macroquad build.
fn rectangle_mods(mods: key::Mods) -> bool {
    let alt = mods.contains(key::Mods::ALT);
    if cfg!(target_os = "macos") {
        alt
    } else {
        alt && mods.intersects(key::Mods::CTRL | key::Mods::SUPER)
    }
}

/// The selection badge in the pane's bottom-right corner: a dark box with
/// a grey outline, as in the macroquad build.
fn draw_badge(ui: &egui::Ui, rect: egui::Rect, text: &str) {
    let painter = ui.painter_at(rect);
    let font = egui::FontId::monospace(12.0);
    let galley = painter.layout_no_wrap(text.to_string(), font, egui::Color32::WHITE);
    let pad = egui::vec2(8.0, 4.0);
    let size = galley.size() + 2.0 * pad;
    let min = rect.max - size - egui::vec2(PADDING + 4.0, PADDING + 4.0);
    let badge = egui::Rect::from_min_size(min, size);
    painter.rect(
        badge,
        3.0,
        egui::Color32::from_rgba_unmultiplied(20, 20, 20, 230),
        egui::Stroke::new(1.0, egui::Color32::GRAY),
        egui::StrokeKind::Inside,
    );
    painter.galley(badge.min + pad, galley, egui::Color32::WHITE);
}

/// One viewport row's instances: cell backgrounds (and the cursor) and
/// glyphs, kept apart so all backgrounds draw before any text.
#[derive(Default)]
struct RowInstances {
    bg: Vec<Instance>,
    text: Vec<Instance>,
}

fn rgba(c: RgbColor) -> [u8; 4] {
    [c.r, c.g, c.b, 255]
}

fn mods_from(m: egui::Modifiers) -> key::Mods {
    let mut mods = key::Mods::empty();
    if m.shift {
        mods |= key::Mods::SHIFT;
    }
    if m.alt {
        mods |= key::Mods::ALT;
    }
    if m.ctrl {
        mods |= key::Mods::CTRL;
    }
    if m.mac_cmd {
        mods |= key::Mods::SUPER;
    }
    mods
}

/// The libghostty key and unshifted codepoint for an egui key.
fn key_for(k: egui::Key) -> Option<(Key, char)> {
    use egui::Key as E;
    Some(match k {
        E::ArrowDown => (Key::ArrowDown, '\0'),
        E::ArrowLeft => (Key::ArrowLeft, '\0'),
        E::ArrowRight => (Key::ArrowRight, '\0'),
        E::ArrowUp => (Key::ArrowUp, '\0'),
        E::Escape => (Key::Escape, '\0'),
        E::Tab => (Key::Tab, '\0'),
        E::Backspace => (Key::Backspace, '\0'),
        E::Enter => (Key::Enter, '\0'),
        E::Space => (Key::Space, ' '),
        E::Insert => (Key::Insert, '\0'),
        E::Delete => (Key::Delete, '\0'),
        E::Home => (Key::Home, '\0'),
        E::End => (Key::End, '\0'),
        E::PageUp => (Key::PageUp, '\0'),
        E::PageDown => (Key::PageDown, '\0'),
        E::F1 => (Key::F1, '\0'),
        E::F2 => (Key::F2, '\0'),
        E::F3 => (Key::F3, '\0'),
        E::F4 => (Key::F4, '\0'),
        E::F5 => (Key::F5, '\0'),
        E::F6 => (Key::F6, '\0'),
        E::F7 => (Key::F7, '\0'),
        E::F8 => (Key::F8, '\0'),
        E::F9 => (Key::F9, '\0'),
        E::F10 => (Key::F10, '\0'),
        E::F11 => (Key::F11, '\0'),
        E::F12 => (Key::F12, '\0'),
        E::Minus => (Key::Minus, '-'),
        E::Equals => (Key::Equal, '='),
        E::Comma => (Key::Comma, ','),
        E::Period => (Key::Period, '.'),
        E::Slash => (Key::Slash, '/'),
        E::Backslash => (Key::Backslash, '\\'),
        E::Semicolon => (Key::Semicolon, ';'),
        E::Quote => (Key::Quote, '\''),
        E::Backtick => (Key::Backquote, '`'),
        E::OpenBracket => (Key::BracketLeft, '['),
        E::CloseBracket => (Key::BracketRight, ']'),
        E::Num0 => (Key::Digit0, '0'),
        E::Num1 => (Key::Digit1, '1'),
        E::Num2 => (Key::Digit2, '2'),
        E::Num3 => (Key::Digit3, '3'),
        E::Num4 => (Key::Digit4, '4'),
        E::Num5 => (Key::Digit5, '5'),
        E::Num6 => (Key::Digit6, '6'),
        E::Num7 => (Key::Digit7, '7'),
        E::Num8 => (Key::Digit8, '8'),
        E::Num9 => (Key::Digit9, '9'),
        E::A => (Key::A, 'a'),
        E::B => (Key::B, 'b'),
        E::C => (Key::C, 'c'),
        E::D => (Key::D, 'd'),
        E::E => (Key::E, 'e'),
        E::F => (Key::F, 'f'),
        E::G => (Key::G, 'g'),
        E::H => (Key::H, 'h'),
        E::I => (Key::I, 'i'),
        E::J => (Key::J, 'j'),
        E::K => (Key::K, 'k'),
        E::L => (Key::L, 'l'),
        E::M => (Key::M, 'm'),
        E::N => (Key::N, 'n'),
        E::O => (Key::O, 'o'),
        E::P => (Key::P, 'p'),
        E::Q => (Key::Q, 'q'),
        E::R => (Key::R, 'r'),
        E::S => (Key::S, 's'),
        E::T => (Key::T, 't'),
        E::U => (Key::U, 'u'),
        E::V => (Key::V, 'v'),
        E::W => (Key::W, 'w'),
        E::X => (Key::X, 'x'),
        E::Y => (Key::Y, 'y'),
        E::Z => (Key::Z, 'z'),
        _ => return None,
    })
}
