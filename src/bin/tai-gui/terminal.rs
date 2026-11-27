//! A terminal pane: a libghostty-vt terminal on tai's pty, drawn by the
//! wgpu cell pass (`gpu.rs`), typed into through egui's events.
//!
//! Text, colours, the cursor, typing, paste, scrollback with the wheel,
//! resizing with the pane, mouse selection with copy and the corner badge,
//! and mouse reporting to programs that ask for it. Images stay in the
//! macroquad build for now.

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
    Terminal,
    color::Rgb,
    effect::SemanticPromptKind,
    key::{self, Key},
    mode::Mode,
    mouse,
    point::{Coordinate, Point},
    render::{CellIterator, Dirty, RenderState, RowIterator},
    report::SizeReportSize,
    screen::{CellWide, RowSemanticPrompt, Screen},
    selection::gesture::{
        Autoscroll, AutoscrollTickEvent, DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent,
    },
    terminal::ScrollViewport,
};
use macroquad::color::Color;

use crate::{
    cells::{GlyphAtlas, Instance},
    fonts::{Coverage, Face, SymbolSpan},
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

/// How often a selection drag past the pane's top or bottom scrolls one
/// row, as Ghostty's `selection_scroll_ms` (src/termio/Thread.zig at
/// 108cacb1).
const AUTOSCROLL_EVERY: Duration = Duration::from_millis(15);

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

/// Where the shell is, from its OSC 133 prompt marks (ADR 0013 decision
/// 7). tai's shell integration makes zsh and bash send them; other shells
/// may (PowerShell with a prompt theme that does), or never do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prompt {
    /// No mark yet: the shell doesn't send them, or hasn't drawn a prompt.
    Unknown,
    /// Drawing its prompt (OSC 133 A), or a command has ended (D).
    Drawing,
    /// At its prompt, reading a command line (B).
    Input,
    /// Running a command (C).
    Running,
}

pub struct TerminalPane {
    terminal: Terminal<'static, 'static>,
    pty: Rc<Pty>,
    child: Child,
    prompt: Rc<Cell<Prompt>>,
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
    /// This pane's GPU objects, its own as its atlas is (gpu.rs).
    gpu: crate::gpu::PaneId,
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
    autoscroll: AutoscrollTickEvent<'static>,
    /// When the last autoscroll tick ran, while a drag past the pane's
    /// top or bottom scrolls it.
    last_autoscroll: Option<Instant>,
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
    /// A key or typed text queued input this frame (paste doesn't count).
    typed: bool,
    /// Mouse reporting to the program, as the macroquad build's
    /// `handle_mouse_input` does.
    mouse_encoder: mouse::Encoder<'static>,
    mouse_event: mouse::Event<'static>,
    /// The buttons (left, right, middle) whose press went to the program,
    /// so their release goes there too, wherever the pointer is then.
    reported: [bool; 3],
    /// Wheel travel not yet a whole row, in points, as Ghostty's
    /// `pending_scroll_y` (src/Surface.zig at 108cacb1, scrollCallback).
    pending_scroll: f32,
    /// The mouse modes and size the encoder was last given (`sync_encoder`).
    encoder_for: Option<([bool; MOUSE_MODES.len()], mouse::EncoderSize)>,
    /// Take the focus next frame: a command was typed for the user.
    want_focus: bool,
    /// A command to type once a new shell has drawn its prompt, and
    /// whether to run it.
    pending_command: Option<(String, bool)>,
    /// When output last arrived, to tell a prompt that has settled.
    last_output: Option<Instant>,
    ctx: egui::Context,
}

/// How long a new shell's output must have stopped before a command is
/// typed at its prompt: startup files print first, and a shell may drop
/// input that comes before it reads the line.
const PROMPT_SETTLE: Duration = Duration::from_millis(300);

/// A command as typed at a prompt: one line, without control characters,
/// so typing it never runs it or sends a key. The user presses Enter.
fn typed_command(command: &str) -> String {
    command
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

impl TerminalPane {
    /// A shell in `cwd`, or in tai-gui's own folder.
    pub fn new(
        coverage: Coverage,
        ctx: egui::Context,
        cwd: Option<&std::path::Path>,
    ) -> Result<Self> {
        let dpi_scale = ctx.pixels_per_point();
        let font_size = shortcuts::DEFAULT_FONT_SIZE;
        let dims = Dimensions::new(&coverage, font_size, dpi_scale, egui::vec2(800.0, 600.0));
        let (cols, rows) = dims.grid_size();
        let (pty, child) = Pty::new(dims, cwd)?;
        let repaint = ctx.clone();
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
        let prompt = Rc::new(Cell::new(Prompt::Unknown));
        let grid_size = Rc::new(Cell::new((cols, rows)));
        let cell_pixels = Rc::new(Cell::new(dims.cell_pixels()));

        let mut terminal = Terminal::new(cols, rows)?;
        terminal.set_scrollback_max_lines(Some(1000))?;
        // Grapheme clustering, mode 2027, only when asked for (grapheme.rs).
        if crate::grapheme::clustering() {
            terminal.set_mode(Mode::GRAPHEME_CLUSTER, true)?;
        }
        // ConPTY keeps its own screen and can't take scrollback back, so on
        // Windows a resize mustn't pull scrollback into the active screen
        // (docs/lessons/from-libghostty-projects.md, section 1).
        #[cfg(windows)]
        terminal.set_resize_pull_scrollback(Some(false))?;
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
            .on_xtversion(|_t| Some("tai"))?
            .on_semantic_prompt({
                let prompt = Rc::clone(&prompt);
                // A panic here aborts, so a kind these bindings don't know
                // is ignored.
                move |_t, event| match event.kind() {
                    Ok(SemanticPromptKind::PromptStart | SemanticPromptKind::CommandEnd) => {
                        prompt.set(Prompt::Drawing)
                    }
                    Ok(SemanticPromptKind::InputStart) => prompt.set(Prompt::Input),
                    Ok(SemanticPromptKind::OutputStart) => prompt.set(Prompt::Running),
                    _ => {}
                }
            })?;

        Ok(Self {
            terminal,
            pty,
            child,
            prompt,
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
            gpu: crate::gpu::PaneId::new(),
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
            autoscroll: AutoscrollTickEvent::new()?,
            last_autoscroll: None,
            epoch: Instant::now(),
            selecting: false,
            selection_changed: false,
            badge: None,
            copy_requested: false,
            typed: false,
            mouse_encoder: mouse::Encoder::new()?,
            mouse_event: mouse::Event::new()?,
            reported: [false; 3],
            pending_scroll: 0.0,
            encoder_for: None,
            want_focus: false,
            pending_command: None,
            last_output: None,
            ctx: repaint,
        })
    }

    /// Type `command` at the shell's prompt and take the focus; it runs
    /// only when `run` is set, for a command tai-gui built itself. A shell that sends prompt marks gets it only at its
    /// prompt; for one that doesn't, not while a full-screen program has
    /// the terminal, where the keys would go to it (ADR 0013 decision 7).
    /// Returns false when it wasn't typed. A shell that hasn't drawn its
    /// prompt yet gets it once it has.
    pub fn type_command(&mut self, command: &str, run: bool) -> Result<bool> {
        let mut line = typed_command(command);
        if line.is_empty() || self.exited() {
            return Ok(false);
        }
        // A command tai-gui runs itself (`/edit`'s editor) starts with a
        // space, which keeps it out of the history of a shell told to
        // (bash HISTCONTROL=ignorespace, zsh HIST_IGNORE_SPACE).
        if run && !line.starts_with(' ') {
            line.insert(0, ' ');
        }
        let prompt = self.prompt.get();
        if (self.last_output.is_none() && prompt == Prompt::Unknown)
            || self.pending_command.is_some()
        {
            self.pending_command = Some((line, run));
            self.want_focus = true;
            self.ctx.request_repaint_after(PROMPT_SETTLE);
            return Ok(true);
        }
        let at_prompt = match prompt {
            Prompt::Unknown => self.terminal.active_screen()? != Screen::Alternate,
            known => known == Prompt::Input,
        };
        if !at_prompt {
            return Ok(false);
        }
        let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE)?;
        let bytes = shortcuts::encode_paste(&line, bracketed)?;
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
        self.pty.write(&bytes);
        if run {
            // Enter, after the paste has ended.
            self.pty.write(b"\r");
        }
        self.want_focus = true;
        Ok(true)
    }

    /// The folder the shell last reported (OSC 7, which tai's shell
    /// integration makes zsh and bash send), if it has.
    pub fn folder(&self) -> Option<std::path::PathBuf> {
        self.terminal
            .pwd()
            .ok()
            .and_then(crate::shell_integration::reported_dir)
    }

    /// Take the focus next frame.
    pub fn focus(&mut self) {
        self.want_focus = true;
    }

    pub fn exited(&self) -> bool {
        !matches!(self.child, Child::Active(_))
    }

    /// Whether closing would stop something the user may want (ADR 0013
    /// decision 2): a command or full-screen program is running, or the
    /// shell sends no prompt marks, so tai can't tell.
    pub fn busy(&self) -> bool {
        if self.exited() {
            return false;
        }
        match self.prompt.get() {
            Prompt::Unknown | Prompt::Running => true,
            Prompt::Drawing | Prompt::Input => {
                self.terminal.active_screen().ok() == Some(Screen::Alternate)
            }
        }
    }

    /// End the shell, off the UI thread: on Unix, dropping a running child
    /// sends SIGHUP and waits up to a second for it (`pty_unix.rs`).
    pub fn close(mut self) {
        // What's left in the pane's place: a child already reaped, which
        // dropping leaves alone.
        #[cfg(unix)]
        let reaped = Child::Reaped(nix::sys::wait::WaitStatus::StillAlive);
        #[cfg(windows)]
        let reaped = Child::Reaped(portable_pty::ExitStatus::with_exit_code(0));
        let child = std::mem::replace(&mut self.child, reaped);
        drop(self);
        std::thread::Builder::new()
            .name("tai-close-terminal".into())
            .spawn(move || drop(child))
            .map_err(|e| tracing::warn!("ending the shell on a thread: {e}"))
            .ok();
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
        let now = Instant::now();
        if got {
            self.last_output = Some(now);
        }
        if self.pending_command.is_some() {
            match self.last_output {
                Some(at) if now >= at + PROMPT_SETTLE => {
                    if let Some((line, run)) = self.pending_command.take()
                        && let Err(e) = self.type_command(&line, run)
                    {
                        tracing::warn!("typing a command: {e}");
                    }
                }
                _ => self.ctx.request_repaint_after(PROMPT_SETTLE),
            }
        }
        got
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
        // Under a dialog (an egui Modal: the close question, the rename
        // box), the terminal neither takes the keyboard nor reads keys:
        // they'd reach the shell unseen. A one-line text box gives up the
        // focus on Enter, which would otherwise hand it to the terminal.
        let under_dialog = !ui.ctx().memory(|m| m.allows_interaction(ui.layer_id()));
        if !under_dialog
            && (std::mem::take(&mut self.want_focus)
                || response.clicked()
                || !ui.ctx().memory(|m| m.focused().is_some()))
        {
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

        if let Err(e) = self.report_pointer(ui, rect) {
            tracing::warn!("mouse: {e}");
        }
        if let Err(e) = self.handle_pointer(ui, rect) {
            tracing::warn!("selection: {e}");
        }
        if response.hovered() {
            // An arrow while the program has the mouse, as Ghostty shows.
            let program = self.terminal.is_mouse_tracking().unwrap_or(false)
                && !ui.input(|i| i.modifiers.shift);
            ui.ctx().set_cursor_icon(if program {
                egui::CursorIcon::Default
            } else {
                egui::CursorIcon::Text
            });
        }

        if response.has_focus() && !under_dialog && !self.exited() {
            let (events, held) = ui.input(|i| (i.events.clone(), i.modifiers));
            for event in &events {
                if let Err(e) = self.handle_event(event, held) {
                    tracing::warn!("input: {e}");
                }
            }
            // A key that sends input clears the selection, as Ghostty's
            // default selection-clear-on-typing does (src/Surface.zig at
            // 108cacb1, keyCallback). Paste doesn't.
            if std::mem::take(&mut self.typed)
                && !self.response.is_empty()
                && self.terminal.selection().is_ok_and(|s| s.is_some())
            {
                if let Err(e) = self.terminal.set_selection(None) {
                    tracing::warn!("selection: {e}");
                }
                self.selection_changed = true;
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
            match self.wheel_to_program(ui, rect) {
                Ok(true) => {}
                Ok(false) => {
                    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                    let lines = (scroll / self.dims.cell_height).round() as isize;
                    if lines != 0 {
                        self.terminal.scroll_viewport(ScrollViewport::Delta(-lines));
                        // A drag in progress follows the scroll: the cell
                        // under the resting pointer is a different one now.
                        if self.selecting
                            && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
                        {
                            let held = ui.input(|i| i.modifiers);
                            if let Err(e) = self.drag_to(pos - rect.min, held) {
                                tracing::warn!("selection: {e}");
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!("wheel: {e}"),
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
                    pane: self.gpu.0,
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

    /// Send the buttons and motion to the program while it tracks the
    /// mouse, as the macroquad build's `handle_mouse_input` does. Shift
    /// keeps a press for selection instead. The encoder follows the
    /// program's tracking mode and format, and drops motion within a cell.
    fn report_pointer(&mut self, ui: &egui::Ui, rect: egui::Rect) -> Result<()> {
        if !self.terminal.is_mouse_tracking()? {
            self.reported = [false; 3];
            return Ok(());
        }
        // The button events in the order they came: a fast click, or a
        // touchpad tap, presses and releases within one frame, and both
        // must reach the program (popos-tai's test of gui-mouse).
        let (buttons, pos, held, downs, moved) = ui.input(|i| {
            let buttons: Vec<_> = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::PointerButton {
                        pos,
                        button,
                        pressed,
                        modifiers,
                    } => {
                        let index = BUTTONS.iter().position(|(b, _)| b == button)?;
                        Some((index, *pos, *pressed, *modifiers))
                    }
                    _ => None,
                })
                .collect();
            (
                buttons,
                i.pointer.latest_pos(),
                i.modifiers,
                BUTTONS.map(|(b, _)| i.pointer.button_down(b)),
                i.pointer.delta() != egui::Vec2::ZERO,
            )
        });
        let mut out = Vec::new();
        self.sync_encoder()?;
        for (index, at, pressed, mods) in buttons {
            let action = if pressed {
                if !rect.contains(at) || mods.shift || self.selecting {
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
            let position = self.surface_position(at - rect.min);
            self.mouse_encoder
                .set_any_button_pressed(self.reported.iter().any(|&r| r));
            self.mouse_event
                .set_mods(mods_from(mods))
                .set_position(position)
                .set_action(action)
                .set_button(Some(BUTTONS[index].1));
            self.mouse_encoder
                .encode_to_vec(&self.mouse_event, &mut out)?;
        }
        let dragging = self.reported.iter().any(|&r| r);
        if let Some(pos) = pos
            && moved
            && !self.selecting
            && (dragging || rect.contains(pos))
        {
            let button = BUTTONS
                .iter()
                .zip(downs)
                .find(|&(_, down)| down)
                .map(|(&(_, b), _)| b);
            let position = self.surface_position(pos - rect.min);
            self.mouse_encoder.set_any_button_pressed(dragging);
            self.mouse_event
                .set_mods(mods_from(held))
                .set_position(position)
                .set_action(mouse::Action::Motion)
                .set_button(button);
            self.mouse_encoder
                .encode_to_vec(&self.mouse_event, &mut out)?;
        }
        if !out.is_empty() {
            self.pty.write(&out);
        }
        Ok(())
    }

    /// Give the encoder the program's mouse modes and the pane's size, but
    /// only when they changed: setting either makes it forget the last
    /// reported cell (src/terminal/c/mouse_encode.zig at 108cacb1), and
    /// every small move within a cell was then reported (popos-tai).
    fn sync_encoder(&mut self) -> Result<()> {
        let mut modes = [false; MOUSE_MODES.len()];
        for (on, mode) in modes.iter_mut().zip(MOUSE_MODES) {
            *on = self.terminal.mode(mode)?;
        }
        let key = (modes, encoder_size(self.dims));
        if self.encoder_for != Some(key) {
            self.mouse_encoder
                .set_options_from_terminal(&self.terminal)
                .set_size(key.1)?
                .set_track_last_cell(true);
            self.encoder_for = Some(key);
        }
        Ok(())
    }

    /// Send the wheel to the program, as Ghostty's scrollCallback does
    /// (src/Surface.zig at 108cacb1): while it tracks the mouse, as button
    /// 4 and 5 presses, one per row of travel; on the alternate screen with
    /// alternate scroll (mode 1007) and no tracking, as arrow keys, which
    /// is how `less` and `man` scroll. Returns false when the wheel isn't
    /// the program's, so it scrolls the scrollback.
    fn wheel_to_program(&mut self, ui: &egui::Ui, rect: egui::Rect) -> Result<bool> {
        let tracking = self.terminal.is_mouse_tracking()?;
        let alternate = !tracking
            && self.terminal.active_screen()? == Screen::Alternate
            && self.terminal.mode(Mode::ALT_SCROLL)?;
        if !tracking && !alternate {
            self.pending_scroll = 0.0;
            return Ok(false);
        }
        let cell = self.dims.cell_height;
        let page = cell * f32::from(self.terminal.rows()?);
        let (travel, pos, held) = ui.input(|i| {
            let travel: f32 = i
                .events
                .iter()
                .filter_map(|e| match e {
                    // Ctrl and Cmd with the wheel zoom in egui.
                    egui::Event::MouseWheel {
                        unit,
                        delta,
                        modifiers,
                        ..
                    } if !modifiers.command && !modifiers.ctrl => {
                        Some(wheel_points(*unit, delta.y, cell, page))
                    }
                    _ => None,
                })
                .sum();
            (travel, i.pointer.latest_pos(), i.modifiers)
        });
        let rows = wheel_rows(&mut self.pending_scroll, travel, cell);
        if rows == 0 {
            return Ok(true);
        }
        let mut out = Vec::new();
        if tracking {
            let Some(pos) = pos else {
                return Ok(true);
            };
            // The selection a Shift drag made goes, as in Ghostty.
            if self.terminal.selection()?.is_some() {
                self.terminal.set_selection(None)?;
                self.selection_changed = true;
            }
            let at = self.surface_position(pos - rect.min);
            self.mouse_event
                .set_mods(mods_from(held))
                .set_position(at)
                .set_action(mouse::Action::Press)
                .set_button(Some(if rows > 0 {
                    mouse::Button::Four
                } else {
                    mouse::Button::Five
                }));
            self.sync_encoder()?;
            self.mouse_encoder
                .set_any_button_pressed(self.reported.iter().any(|&r| r))
                // Every row is a report, even within one cell.
                .set_track_last_cell(false);
            for _ in 0..rows.unsigned_abs() {
                self.mouse_encoder
                    .encode_to_vec(&self.mouse_event, &mut out)?;
            }
            self.mouse_encoder.set_track_last_cell(true);
        } else {
            let seq = arrow_seq(rows > 0, self.terminal.mode(Mode::DECCKM)?);
            for _ in 0..rows.unsigned_abs() {
                out.extend_from_slice(seq);
            }
        }
        self.pty.write(&out);
        Ok(true)
    }

    /// A point in the pane as the encoder takes it: surface pixels, which
    /// are physical pixels (include/ghostty/vt/mouse/encoder.h). The
    /// macroquad build scales macroquad's points the same way, so SGR-Pixels
    /// reports (mode 1016) share units with the size tai reports.
    fn surface_position(&self, local: egui::Vec2) -> mouse::Position {
        mouse::Position {
            x: local.x * self.dims.dpi_scale,
            y: local.y * self.dims.dpi_scale,
        }
    }

    /// Start, extend and end a selection from the primary button, as the
    /// macroquad build does. While a program tracks the mouse the buttons
    /// are its (`report_pointer`), except with Shift held.
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
            self.drag_to(local, held)?;
        }
        if self.selecting && down {
            self.autoscroll_tick(ui, local, held)?;
        } else {
            self.last_autoscroll = None;
        }
        // Not an else: a fast click presses and releases in one frame, and
        // a selection left open would keep the next presses from the
        // program (popos-tai's test of gui-mouse).
        if self.selecting && released {
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

    /// The pane as the selection gesture measures it.
    fn geometry(&self) -> Result<Geometry> {
        Ok(Geometry {
            columns: self.terminal.cols()?.into(),
            cell_width: self.dims.cell_width as u32,
            padding_left: PADDING as u32,
            screen_height: self.dims.height as u32,
        })
    }

    /// Extend the selection drag to `local`. Past the pane's top or bottom
    /// the gesture asks to autoscroll (`autoscroll_tick`).
    fn drag_to(&mut self, local: egui::Vec2, held: egui::Modifiers) -> Result<()> {
        let grid_ref = self.terminal.grid_ref(self.viewport_point(local)?)?;
        let geometry = self.geometry()?;
        let selection = self
            .drag
            .set_rectangle(rectangle_mods(mods_from(held)))?
            .set_position(local.x.into(), local.y.into())?
            .apply(&mut self.gesture, &self.terminal, grid_ref, geometry)?;
        self.terminal.set_selection(selection.as_ref())?;
        self.selection_changed = true;
        Ok(())
    }

    /// While a selection drag is past the pane's top or bottom, scroll one
    /// row and extend the selection every AUTOSCROLL_EVERY, as Ghostty's
    /// selectionScrollTick does (src/Surface.zig at 108cacb1); the gesture
    /// has no timer of its own (include/ghostty/vt/selection.h).
    fn autoscroll_tick(
        &mut self,
        ui: &egui::Ui,
        local: egui::Vec2,
        held: egui::Modifiers,
    ) -> Result<()> {
        if matches!(self.gesture.autoscroll(&self.terminal)?, Autoscroll::None) {
            self.last_autoscroll = None;
            return Ok(());
        }
        let now = Instant::now();
        if self
            .last_autoscroll
            .is_none_or(|last| now.duration_since(last) >= AUTOSCROLL_EVERY)
        {
            self.last_autoscroll = Some(now);
            let at = self.viewport_coordinate(local)?;
            let geometry = self.geometry()?;
            let selection = self
                .autoscroll
                .set_rectangle(rectangle_mods(mods_from(held)))?
                .set_position(local.x.into(), local.y.into())?
                .apply(&mut self.gesture, &self.terminal, at, geometry)?;
            // None: nothing to scroll, or the pressed cell left the screen
            // and the gesture reset; the selection stays as it is.
            if let Some(selection) = selection {
                self.terminal.set_selection(Some(&selection))?;
                self.selection_changed = true;
            }
        }
        // Keep ticking while the pointer rests past the edge.
        ui.ctx().request_repaint_after(AUTOSCROLL_EVERY);
        Ok(())
    }

    /// The viewport cell under a point in the pane, clamped to the grid as
    /// the macroquad build's `mouse_viewport_point` does: grid_ref rejects
    /// a point one past the last row or column.
    fn viewport_point(&self, local: egui::Vec2) -> Result<Point> {
        Ok(Point::Viewport(self.viewport_coordinate(local)?))
    }

    fn viewport_coordinate(&self, local: egui::Vec2) -> Result<Coordinate> {
        let cols = self.terminal.cols()?;
        let rows = self.terminal.rows()?;
        let cell = |v: f32, size: f32, count: u32| {
            if v > PADDING {
                ((v - PADDING) / size).clamp(0.0, count.saturating_sub(1) as f32) as u32
            } else {
                0
            }
        };
        Ok(Coordinate {
            x: cell(local.x, self.dims.cell_width, u32::from(cols)) as u16,
            y: cell(local.y, self.dims.cell_height, u32::from(rows)),
        })
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
                cached.edge.clear();
                let prompt = !matches!(
                    row.raw_row()?.semantic_prompt(),
                    Ok(RowSemanticPrompt::None) | Err(_)
                );
                let mut extend_cells = Vec::new();
                let mut cell_it = self.cell_it.update(row)?;
                let mut x = PADDING;
                let mut col: u16 = 0;
                let mut span = SymbolSpan::default();
                let mut held: Option<(Glyph, String)> = None;
                while let Some(cell) = cell_it.next() {
                    let graphemes = cell.graphemes_len()?;
                    if graphemes > 0 {
                        cell.graphemes_utf8(&mut text)?;
                    }
                    let grow = span.next((graphemes > 0).then(|| text.chars().next()).flatten());
                    if let Some((mut glyph, held_text)) = held.take() {
                        if grow {
                            glyph.cells = 2;
                        }
                        glyph.push(
                            &mut self.atlas,
                            &self.coverage,
                            &mut cached.text,
                            &held_text,
                            dims,
                        );
                    }
                    let selected = cell.is_selected()?;
                    let mut fg = cell.fg_color()?.unwrap_or(colors.foreground);
                    let bg = cell.bg_color()?;
                    extend_cells.push((bg, (graphemes > 0).then(|| text.chars().next()).flatten()));
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
                    cached.edge.push(rgba(bg));
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
                        // One face for the whole cluster, trimmed to what
                        // tai draws (see walk_row in src/main.rs).
                        let face = self.coverage.cluster_face(&mut text);
                        if face != Face::Blank && !text.is_empty() {
                            // A wide character is drawn two cells wide, as
                            // Ghostty's constraintWidth does (src/renderer/
                            // cell.zig at 521cf2c5). A narrow one from a
                            // fallback face waits for the next cell, which
                            // it may take too (`SymbolSpan`).
                            let wide = cell.raw_cell()?.wide()? == CellWide::Wide;
                            let glyph = Glyph {
                                face,
                                x,
                                y,
                                fg: rgba(fg),
                                bold,
                                cells: if wide { 2 } else { 1 },
                            };
                            if wide || face == Face::Primary {
                                glyph.push(
                                    &mut self.atlas,
                                    &self.coverage,
                                    &mut cached.text,
                                    &text,
                                    dims,
                                );
                            } else {
                                held = Some((glyph, text.clone()));
                            }
                        }
                    }
                    x += dims.cell_width;
                    col += 1;
                }
                if let Some((glyph, text)) = held {
                    glyph.push(
                        &mut self.atlas,
                        &self.coverage,
                        &mut cached.text,
                        &text,
                        dims,
                    );
                }
                cached.extend = extends(prompt, &extend_cells, colors.background);
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
        out.extend(
            padding_rects(&self.rows, dims, rgba(colors.background))
                .into_iter()
                .map(|(rect, color)| Instance::solid(rect, color)),
        );
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
            egui::Event::Text(text) => {
                self.response.extend_from_slice(text.as_bytes());
                self.typed = true;
            }
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
        self.typed = true;
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
    /// Each cell's background, the default one included, for the padding.
    edge: Vec<[u8; 4]>,
    /// Whether the padding above or below may take this row's colours.
    extend: bool,
}

/// The padding around the grid in the colours of the nearest cells, as
/// Ghostty's `window-padding-color = extend` draws it (src/renderer/
/// generic.zig and shaders.metal at 108cacb1): a full-screen app with its
/// own background, such as Helix, then fills the pane without a frame in
/// the terminal's default colour. The sides always take the edge cells'
/// colours; the top and bottom only when the nearest row allows it
/// (`extends`). Colours equal to the default `background` are skipped, as
/// the pane is already that colour.
fn padding_rects(
    rows: &[RowInstances],
    dims: Dimensions,
    background: [u8; 4],
) -> Vec<([f32; 4], [u8; 4])> {
    let (cw, ch) = (dims.cell_width, dims.cell_height);
    let mut out = Vec::new();
    // One strip of the padding above or below a row, in runs of one colour.
    let strip = |edge: &[[u8; 4]], y: f32, h: f32, out: &mut Vec<([f32; 4], [u8; 4])>| {
        let mut run: Option<(f32, [u8; 4])> = None;
        for (c, &color) in edge.iter().enumerate() {
            let start = if c == 0 { 0.0 } else { PADDING + c as f32 * cw };
            if let Some((x, held)) = run
                && held != color
            {
                out.push(([x, y, start - x, h], held));
                run = None;
            }
            run.get_or_insert((start, color));
        }
        if let Some((x, held)) = run {
            out.push(([x, y, dims.width - x, h], held));
        }
    };
    let mut y = PADDING;
    for row in rows {
        if let (Some(&first), Some(&last)) = (row.edge.first(), row.edge.last()) {
            out.push(([0.0, y, PADDING, ch], first));
            let right = PADDING + row.edge.len() as f32 * cw;
            out.push(([right, y, dims.width - right, ch], last));
        }
        y += ch;
    }
    if let Some(top) = rows.first().filter(|r| r.extend) {
        strip(&top.edge, 0.0, PADDING, &mut out);
    }
    if let Some(bottom) = rows.last().filter(|r| r.extend) {
        strip(&bottom.edge, y, dims.height - y, &mut out);
    }
    out.retain(|(rect, color)| *color != background && rect[2] > 0.0 && rect[3] > 0.0);
    out
}

/// Whether the padding may take a row's colours (Ghostty's
/// `neverExtendBg`, src/renderer/row.zig): not a prompt row, and no cell
/// with the default background or a powerline glyph, which is drawn to fit
/// its cell exactly.
fn extends(prompt: bool, cells: &[(Option<Rgb>, Option<char>)], background: Rgb) -> bool {
    !prompt
        && cells.iter().all(|&(bg, first)| {
            bg.is_some_and(|bg| bg != background) && !first.is_some_and(is_powerline)
        })
}

fn is_powerline(c: char) -> bool {
    matches!(c as u32, 0xE0B0..=0xE0C8 | 0xE0CA | 0xE0CC..=0xE0D2 | 0xE0D4)
}

/// Where and how one cell's glyph is drawn.
struct Glyph {
    face: Face,
    x: f32,
    y: f32,
    fg: [u8; 4],
    bold: bool,
    /// How many cells it spans: 2 for a wide cell, or a narrow symbol that
    /// takes the next cell (`SymbolSpan`).
    cells: u8,
}

impl Glyph {
    fn push(
        &self,
        atlas: &mut GlyphAtlas,
        coverage: &Coverage,
        out: &mut Vec<Instance>,
        text: &str,
        dims: Dimensions,
    ) {
        let place = coverage.place(self.face, dims.font_size, self.cells);
        let fg = self.fg;
        atlas.push_text(
            out,
            coverage.font(self.face),
            self.face,
            text,
            self.x,
            self.y + dims.baseline - place.rise,
            place.font_size,
            dims.dpi_scale,
            Color::from_rgba(fg[0], fg[1], fg[2], fg[3]),
            self.bold,
        );
    }
}

/// The buttons reported to programs, with egui's names for them.
const BUTTONS: [(egui::PointerButton, mouse::Button); 3] = [
    (egui::PointerButton::Primary, mouse::Button::Left),
    (egui::PointerButton::Secondary, mouse::Button::Right),
    (egui::PointerButton::Middle, mouse::Button::Middle),
];

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

/// The pane's geometry in surface pixels, as the encoder needs it to find
/// the cell under a point.
fn encoder_size(dims: Dimensions) -> mouse::EncoderSize {
    let (cell_width, cell_height) = dims.cell_pixels();
    let padding = (PADDING * dims.dpi_scale) as u32;
    mouse::EncoderSize {
        screen_width: (dims.width * dims.dpi_scale) as u32,
        screen_height: (dims.height * dims.dpi_scale) as u32,
        cell_width: cell_width.max(1),
        cell_height: cell_height.max(1),
        padding_top: padding,
        padding_bottom: padding,
        padding_left: padding,
        padding_right: padding,
    }
}

/// One wheel event's travel in points, up positive. A line is a row; on
/// macOS a slow wheel click comes as a tenth of a line, so a line event
/// counts as at least one, as Ghostty does.
fn wheel_points(unit: egui::MouseWheelUnit, y: f32, cell: f32, page: f32) -> f32 {
    match unit {
        egui::MouseWheelUnit::Point => y,
        egui::MouseWheelUnit::Line if cfg!(target_os = "macos") && y != 0.0 => {
            y.signum() * y.abs().max(1.0) * cell
        }
        egui::MouseWheelUnit::Line => y * cell,
        egui::MouseWheelUnit::Page => y * page,
    }
}

/// Add `travel` to the pending travel and take whole rows from it, toward
/// zero, keeping the rest for later events (Ghostty's scrollCallback).
fn wheel_rows(pending: &mut f32, travel: f32, cell: f32) -> i32 {
    *pending += travel;
    let rows = (*pending / cell).trunc();
    *pending -= rows * cell;
    rows as i32
}

/// The arrow key the wheel sends under alternate scroll, in the cursor
/// key mode (DECCKM) the program set.
fn arrow_seq(up: bool, application: bool) -> &'static [u8] {
    match (up, application) {
        (true, true) => b"\x1bOA",
        (false, true) => b"\x1bOB",
        (true, false) => b"\x1b[A",
        (false, false) => b"\x1b[B",
    }
}

fn rgba(c: Rgb) -> [u8; 4] {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A full-screen app's background reaches the pane's edges (Ghostty's
    /// `window-padding-color = extend`); a shell's default one doesn't
    /// draw, and a row with a default cell keeps the top or bottom plain.
    #[test]
    fn the_padding_takes_the_edge_cells_colours() {
        let dims = Dimensions {
            width: 2.0 * PADDING + 30.0 + 4.0,
            height: 2.0 * PADDING + 40.0 + 7.0,
            cell_width: 10.0,
            cell_height: 20.0,
            baseline: 15.0,
            font_size: 13,
            dpi_scale: 1.0,
        };
        let (bg, a, b) = ([0, 0, 0, 255], [1, 1, 1, 255], [2, 2, 2, 255]);
        let row = |edge: [[u8; 4]; 3], extend| RowInstances {
            edge: edge.to_vec(),
            extend,
            ..Default::default()
        };
        let p = PADDING;
        let right = p + 30.0;
        // Top: a, a, b in two runs; bottom row has a default cell.
        let rows = [row([a, a, b], true), row([b, a, bg], false)];
        assert_eq!(
            padding_rects(&rows, dims, bg),
            vec![
                ([0.0, p, p, 20.0], a),
                ([right, p, p + 4.0, 20.0], b),
                ([0.0, p + 20.0, p, 20.0], b),
                ([0.0, 0.0, p + 20.0, p], a),
                ([p + 20.0, 0.0, dims.width - p - 20.0, p], b),
            ]
        );
        let shell = [row([bg, bg, bg], false), row([bg, bg, bg], false)];
        assert!(padding_rects(&shell, dims, bg).is_empty());

        let black = Rgb { r: 0, g: 0, b: 0 };
        let blue = Some(Rgb { r: 0, g: 0, b: 9 });
        assert!(extends(false, &[(blue, None), (blue, Some('x'))], black));
        assert!(!extends(true, &[(blue, None)], black), "a prompt row");
        assert!(!extends(false, &[(blue, None), (None, None)], black));
        assert!(!extends(false, &[(Some(black), None)], black));
        assert!(!extends(false, &[(blue, Some('\u{e0b0}'))], black));
    }

    /// tai's shell integration reports the folder with OSC 7, which the
    /// terminal keeps for `folder`. Unix only, like `reported_dir`'s test.
    #[cfg(unix)]
    #[test]
    fn a_reported_folder_is_kept() {
        let mut terminal = Terminal::new(80, 24).unwrap();
        terminal.vt_write(b"\x1b]7;kitty-shell-cwd://host/usr/local\x07");
        assert_eq!(
            terminal
                .pwd()
                .ok()
                .and_then(crate::shell_integration::reported_dir),
            Some(std::path::PathBuf::from("/usr/local")),
            "{:?}",
            terminal.pwd()
        );
    }

    /// A command from ppxai's `run_shell` is typed, never run: no newline,
    /// no escape sequence, no other control key gets through.
    #[test]
    fn a_typed_command_is_one_line_of_text() {
        assert_eq!(typed_command("ls -la\n"), "ls -la");
        assert_eq!(typed_command("a\rb\x1b[201~c\x03"), "a b [201~c");
        assert_eq!(typed_command("echo \u{9b}x\ty"), "echo  x y");
        assert_eq!(typed_command("\n\n"), "");
    }

    #[test]
    fn wheel_travel_becomes_whole_rows() {
        let mut pending = 0.0;
        // Touchpad points add up until they make a row.
        assert_eq!(wheel_rows(&mut pending, 10.0, 24.0), 0);
        assert_eq!(wheel_rows(&mut pending, 10.0, 24.0), 0);
        assert_eq!(wheel_rows(&mut pending, 10.0, 24.0), 1);
        assert!((pending - 6.0).abs() < 1e-4);
        // Turning back undoes what is pending first.
        assert_eq!(wheel_rows(&mut pending, -6.0, 24.0), 0);
        assert_eq!(wheel_rows(&mut pending, -50.0, 24.0), -2);
        // A line is a row; a page is the screen.
        assert_eq!(
            wheel_points(egui::MouseWheelUnit::Line, 3.0, 24.0, 480.0),
            72.0
        );
        assert_eq!(
            wheel_points(egui::MouseWheelUnit::Page, -1.0, 24.0, 480.0),
            -480.0
        );
        let tenth = wheel_points(egui::MouseWheelUnit::Line, 0.1, 24.0, 480.0);
        if cfg!(target_os = "macos") {
            assert_eq!(tenth, 24.0);
        } else {
            assert!((tenth - 2.4).abs() < 1e-4);
        }
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
    fn a_press_reports_the_cell_under_the_pointer() -> Result<()> {
        for scale in [1.0, 2.0] {
            let dims = Dimensions {
                width: 800.0,
                height: 600.0,
                cell_width: 10.0,
                cell_height: 20.0,
                baseline: 15.0,
                font_size: 13,
                dpi_scale: scale,
            };
            let (cols, rows) = dims.grid_size();
            let (cw, ch) = dims.cell_pixels();
            let mut terminal = Terminal::new(cols, rows)?;
            terminal.resize(cols, rows, cw, ch)?;
            // Normal tracking, SGR format.
            terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
            let mut encoder = mouse::Encoder::new()?;
            encoder
                .set_options_from_terminal(&terminal)
                .set_size(encoder_size(dims))?;
            let mut event = mouse::Event::new()?;
            // Column 3 and row 1 from zero: past the padding, mid-cell.
            let local = egui::vec2(PADDING + 3.5 * 10.0, PADDING + 1.5 * 20.0);
            event
                .set_action(mouse::Action::Press)
                .set_button(Some(mouse::Button::Left))
                .set_position(mouse::Position {
                    x: local.x * scale,
                    y: local.y * scale,
                });
            let mut out = Vec::new();
            encoder.encode_to_vec(&event, &mut out)?;
            assert_eq!(out, b"\x1b[<0;4;2M", "at {scale}x");
        }
        Ok(())
    }
}
