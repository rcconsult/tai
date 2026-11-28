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
    kitty::graphics::PlacementIterator,
    mode::Mode,
    render::{CellIterator, Dirty, RenderState, RowIterator},
    screen::Screen,
    terminal::ScrollViewport,
};

use crate::{
    cells::{GlyphAtlas, Instance},
    fonts::{Coverage, Face},
    gpu::{AtlasUpload, CellFrame, ImageStage, ImageUpload, ImagesHeld},
    pty::{Child, Pty, PtyError},
    selection, shortcuts,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// Space between the pane's edge and the grid: the terminal core's, shared
// with tai (src/term/mod.rs).
use crate::term::PADDING;

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
    /// Keys to the program, as tai sends them (src/term/keys.rs).
    keys: crate::term::keys::Keys,
    /// The key pressed last, waiting for its text.
    pending_key: Option<PendingKey>,
    atlas: GlyphAtlas,
    /// This pane's GPU objects, its own as its atlas is (gpu.rs).
    gpu: crate::gpu::PaneId,
    /// The last frame's instances, reused while nothing changed.
    instances: Arc<Vec<Instance>>,
    /// Where the cell backgrounds and the glyphs start in `instances`, so
    /// images can draw between them (gpu.rs, `ImageStage`).
    bg_start: u32,
    text_start: u32,
    /// Kitty images (src/term/images.rs): placements, and each drawn
    /// image's pixels, copied once per generation for the GPU side.
    placement_it: PlacementIterator<'static>,
    images_held: ImagesHeld,
    /// Translates Sixel in the pty's output into Kitty images (ADR 0006).
    sixel: crate::sixel::Translator,
    built_for: Option<(Dimensions, Option<(u16, u16)>)>,
    /// The preedit the instances were built with (ADR 0015).
    built_preedit: Option<String>,
    /// Each viewport row's instances as last built, so a frame rebuilds
    /// only the rows that changed, as Ghostty's `rebuildRow` does
    /// (src/renderer/generic.zig at a887df42) and the macroquad build's
    /// cell renderer does (ADR 0008).
    rows: Vec<RowInstances>,
    /// Typed and pasted bytes waiting to be written.
    response: Vec<u8>,
    /// Set by the pty's reader when it has woken egui, cleared by pump.
    wake_pending: Arc<AtomicBool>,
    /// The mouse: reports, selection and wheel, as tai handles it
    /// (src/term/pointer.rs).
    pointer: crate::term::pointer::Pointer,
    /// The selection changed this frame, so the cells must be redrawn: the
    /// render state's dirty flags track contents, not the selection.
    selection_changed: bool,
    /// The selection badge's text, while there is a selection.
    badge: Option<String>,
    /// Copy was asked for; done in `ui`, which has the egui context.
    copy_requested: bool,
    /// A key or typed text queued input this frame (paste doesn't count).
    typed: bool,
    /// Take the focus next frame: a command was typed for the user.
    want_focus: bool,
    /// A command to type once a new shell has drawn its prompt, and
    /// whether to run it.
    pending_command: Option<(String, bool)>,
    /// When output last arrived, to tell a prompt that has settled.
    last_output: Option<Instant>,
    ctx: egui::Context,
}

/// A key pressed, with the text egui sent for it after it.
struct PendingKey {
    action: key::Action,
    key: Key,
    ucp: char,
    mods: key::Mods,
    text: String,
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

        // The options and the answers to programs' queries tai shares
        // (src/term/setup.rs).
        let mut terminal = crate::term::setup::new_terminal(cols, rows, dims.cell_pixels())?;
        crate::term::setup::answer_queries(
            &mut terminal,
            {
                let pty = Rc::clone(&pty);
                move |data| pty.reply(data)
            },
            {
                let (gs, cp) = (Rc::clone(&grid_size), Rc::clone(&cell_pixels));
                move || (gs.get(), cp.get())
            },
        )?;
        // Kitty images, as tai accepts them (src/term/images.rs).
        crate::term::images::enable(&mut terminal)?;
        terminal.on_semantic_prompt({
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
            keys: crate::term::keys::Keys::new()?,
            pending_key: None,
            atlas: GlyphAtlas::new(crate::cells::ATLAS_MAX_HEIGHT),
            gpu: crate::gpu::PaneId::new(),
            instances: Arc::new(Vec::new()),
            bg_start: 0,
            text_start: 0,
            placement_it: PlacementIterator::new()?,
            images_held: Default::default(),
            sixel: crate::sixel::Translator::new(),
            built_for: None,
            built_preedit: None,
            rows: Vec::new(),
            response: Vec::new(),
            wake_pending,
            pointer: crate::term::pointer::Pointer::new()?,
            selection_changed: false,
            badge: None,
            copy_requested: false,
            typed: false,
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
        let sixel = &mut self.sixel;
        // Sixel images become Kitty images on the way in (ADR 0006).
        let cell_height = self.cell_pixels.get().1;
        let mut sink = |bytes: &[u8]| {
            got = true;
            sixel.feed(bytes, cell_height, &mut |out| terminal.vt_write(out));
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

        // The mouse, through the terminal core tai shares
        // (src/term/pointer.rs).
        let input = self.pointer_input(ui, rect, response.hovered());
        let pane = self.pointer_pane();
        let mut out = Vec::new();
        match self
            .pointer
            .frame(&mut self.terminal, pane, &input, &mut out)
        {
            Ok(outcome) => {
                if outcome.redraw {
                    self.selection_changed = true;
                }
                // Keep ticking while a drag rests past the edge.
                if let Some(within) = outcome.tick_within {
                    ui.ctx().request_repaint_after(within);
                }
            }
            Err(e) => tracing::warn!("mouse: {e}"),
        }
        if !out.is_empty() {
            self.pty.write(&out);
        }
        if response.hovered() {
            use crate::term::pointer::Shape;
            let shape = self
                .pointer
                .shape(
                    &self.terminal,
                    self.pointer_pane(),
                    input.pointer,
                    input.mods,
                )
                .unwrap_or(Shape::Text);
            ui.ctx().set_cursor_icon(match shape {
                Shape::Default => egui::CursorIcon::Default,
                Shape::Text => egui::CursorIcon::Text,
                Shape::Crosshair => egui::CursorIcon::Crosshair,
            });
        }

        if response.has_focus() && !under_dialog && !self.exited() {
            let (events, held) = ui.input(|i| (i.events.clone(), i.modifiers));
            for event in &events {
                if let Err(e) = self.handle_event(event, held) {
                    tracing::warn!("input: {e}");
                }
            }
            if let Err(e) = self.flush_key() {
                tracing::warn!("input: {e}");
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
        let (images, image_uploads) = self.image_frame().unwrap_or_else(|e| {
            tracing::warn!("images: {e}");
            Default::default()
        });
        ui.painter()
            .add(eframe::egui_wgpu::Callback::new_paint_callback(
                rect,
                CellFrame {
                    pane: self.gpu.0,
                    view: rect.size(),
                    instances: Arc::clone(&self.instances),
                    atlas,
                    bg_start: self.bg_start,
                    text_start: self.text_start,
                    images,
                    image_uploads,
                    images_held: Arc::clone(&self.images_held),
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

    /// The pane as the terminal core's mouse handling measures it.
    fn pointer_pane(&self) -> crate::term::pointer::Pane {
        crate::term::pointer::Pane {
            width: self.dims.width,
            height: self.dims.height,
            cell_width: self.dims.cell_width,
            cell_height: self.dims.cell_height,
            dpi_scale: self.dims.dpi_scale,
            cell_pixels: self.dims.cell_pixels(),
        }
    }

    /// This frame's mouse, from egui, for the terminal core: the button
    /// events in the order they came (a fast click presses and releases in
    /// one frame), the pointer, the buttons held, and the wheel's rows
    /// while the pointer is over the pane.
    fn pointer_input(
        &self,
        ui: &egui::Ui,
        rect: egui::Rect,
        hovered: bool,
    ) -> crate::term::pointer::Input {
        use crate::term::pointer::{Button, Click, Input, Pos};
        let local = |p: egui::Pos2| Pos {
            x: p.x - rect.min.x,
            y: p.y - rect.min.y,
        };
        let cell = self.dims.cell_height;
        let page = cell * f32::from(self.terminal.rows().unwrap_or(1));
        ui.input(|i| Input {
            clicks: i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::PointerButton {
                        pos,
                        button,
                        pressed,
                        modifiers,
                    } => Some(Click {
                        button: match button {
                            egui::PointerButton::Primary => Button::Left,
                            egui::PointerButton::Secondary => Button::Right,
                            egui::PointerButton::Middle => Button::Middle,
                            _ => return None,
                        },
                        pressed: *pressed,
                        pos: local(*pos),
                        mods: mods_from(*modifiers),
                    }),
                    _ => None,
                })
                .collect(),
            pointer: i.pointer.latest_pos().map(local),
            moved: i.pointer.delta() != egui::Vec2::ZERO,
            down: [
                i.pointer.button_down(egui::PointerButton::Primary),
                i.pointer.button_down(egui::PointerButton::Secondary),
                i.pointer.button_down(egui::PointerButton::Middle),
            ],
            mods: mods_from(i.modifiers),
            wheel_rows: if hovered {
                wheel_travel(i, cell, page) / cell
            } else {
                0.0
            },
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
        let preedit = self.keys.preedit();
        if !force
            && dirty == Dirty::Clean
            && self.built_for == key
            && self.built_preedit.as_deref() == preedit
        {
            return Ok(false);
        }
        self.built_preedit = preedit.map(str::to_owned);
        let cols = snapshot.cols()?;
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
                // The row, read by the terminal core tai shares
                // (src/term/rows.rs), into this renderer's quads.
                let on_row = cursor.filter(|&(_, cy)| cy == row_index).map(|(col, _)| {
                    crate::term::rows::Cursor {
                        col,
                        color: cursor_color,
                    }
                });
                // The preedit, laid out on the cursor's row (src/term/ime.rs).
                let preedit_cells = match (on_row, self.built_preedit.as_deref()) {
                    (Some(c), Some(p)) => crate::term::ime::layout(p, c.col, cols),
                    _ => Vec::new(),
                };
                cached.read = crate::term::rows::read_row(
                    row,
                    &mut self.cell_it,
                    &colors,
                    &self.coverage,
                    on_row,
                    &preedit_cells,
                    &mut text,
                )?;
                cached.bg.clear();
                cached.text.clear();
                let mut x = PADDING;
                for bg in &cached.read.bg {
                    if let Some(bg) = bg {
                        cached.bg.push(Instance::solid(
                            [x, y, dims.cell_width, dims.cell_height],
                            rgba(*bg),
                        ));
                    }
                    x += dims.cell_width;
                }
                crate::term::rows::push_glyphs(
                    &mut self.atlas,
                    &self.coverage,
                    &mut cached.text,
                    &cached.read,
                    y,
                    dims.cell_width,
                    dims.baseline,
                    dims.font_size,
                    dims.dpi_scale,
                );
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
            crate::term::rows::padding_rects(
                &self.rows,
                dims.width,
                dims.height,
                dims.cell_width,
                dims.cell_height,
                colors.background,
            )
            .into_iter()
            .map(|(rect, color)| Instance::solid(rect, rgba(color))),
        );
        self.bg_start = out.len() as u32;
        for row in &self.rows {
            out.extend_from_slice(&row.bg);
        }
        // Lines under cells: the preedit's (src/term/rows.rs).
        out.extend(
            crate::term::rows::underline_rects(
                &self.rows,
                dims.cell_width,
                dims.cell_height,
                dims.baseline,
            )
            .into_iter()
            .map(|(rect, color)| Instance::solid(rect, rgba(color))),
        );
        self.text_start = out.len() as u32;
        for row in &self.rows {
            out.extend_from_slice(&row.text);
        }
        self.instances = Arc::new(out);
        Ok(true)
    }

    /// This frame's Kitty images in drawing order (src/term/images.rs), and
    /// the pixels of those the GPU side holds no texture for.
    #[allow(clippy::type_complexity)]
    fn image_frame(&mut self) -> Result<(Vec<(ImageStage, u32, u64, Instance)>, Vec<ImageUpload>)> {
        let Some(graphics) = self.terminal.kitty_graphics()? else {
            return Ok(Default::default());
        };
        let geometry = crate::term::images::Geometry {
            cell_width: self.dims.cell_width,
            cell_height: self.dims.cell_height,
            dpi_scale: self.dims.dpi_scale,
        };
        let runs: Vec<(u16, crate::placeholders::Placement)> = self
            .rows
            .iter()
            .enumerate()
            .flat_map(|(r, row)| {
                row.read
                    .virtual_runs
                    .iter()
                    .map(move |run| (r as u16, *run))
            })
            .collect();
        let staged = crate::term::images::frame_draws(
            &self.terminal,
            &mut self.placement_it,
            &graphics,
            &runs,
            geometry,
        )?;
        let held = self
            .images_held
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default();
        let mut images = Vec::with_capacity(staged.len());
        let mut uploads: Vec<ImageUpload> = Vec::new();
        for (stage, draw) in staged {
            let Some(pixels) = crate::term::images::pixels(&graphics, draw.image_id)? else {
                continue;
            };
            if held.get(&draw.image_id) != Some(&pixels.generation)
                && !uploads.iter().any(|u| u.id == draw.image_id)
            {
                uploads.push(ImageUpload {
                    id: draw.image_id,
                    generation: pixels.generation,
                    size: (pixels.width, pixels.height),
                    rgba: pixels.data.to_vec(),
                });
            }
            images.push((
                stage,
                draw.image_id,
                pixels.generation,
                Instance::textured(
                    [draw.dest.x, draw.dest.y, draw.dest.w, draw.dest.h],
                    [draw.source.x, draw.source.y, draw.source.w, draw.source.h],
                ),
            ));
        }
        Ok((images, uploads))
    }

    fn handle_event(&mut self, event: &egui::Event, held: egui::Modifiers) -> Result<()> {
        let macos = cfg!(target_os = "macos");
        // A pending key's text comes just after it; anything else ends it.
        if !matches!(event, egui::Event::Text(_)) {
            self.flush_key()?;
        }
        match event {
            // Alt+key also arrives as its plain text; the key event sends
            // the chord (ESC + key).
            // egui-winit sends a key's text just after its key event
            // (on_keyboard_input); it belongs to that key, and the encoder
            // gets both, as in tai (src/main.rs, flush_pending_key).
            egui::Event::Text(text) => match &mut self.pending_key {
                Some(pending) => pending.text.push_str(text),
                // Text with no key: an unmapped key, or IME. As it is.
                None => {
                    self.response.extend_from_slice(text.as_bytes());
                    self.typed = true;
                }
            },
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
            egui::Event::Copy => {
                self.encode_key(key::Action::Press, Key::C, 'c', key::Mods::CTRL, None)?
            }
            egui::Event::Cut if !macos => {
                self.encode_key(key::Action::Press, Key::X, 'x', key::Mods::CTRL, None)?
            }
            egui::Event::Key {
                key,
                pressed,
                repeat,
                modifiers,
                ..
            } => {
                let Some((k, ucp)) = key_for(*key) else {
                    return Ok(());
                };
                let mods = mods_from(*modifiers);
                // An app shortcut's key is the app's, as in tai: egui zooms
                // for Cmd+= / Ctrl+= at the end of the frame, after this
                // pane has read the key. Its release then has no press and
                // keys.rs drops it.
                if *pressed
                    && shortcut_key(*key)
                        .is_some_and(|kc| shortcuts::action_for(kc, mods, macos).is_some())
                {
                    return Ok(());
                }
                if !pressed {
                    return self.encode_key(key::Action::Release, k, ucp, mods, None);
                }
                // Encoded once its text, if any, has come (flush_key).
                self.pending_key = Some(PendingKey {
                    action: if *repeat {
                        key::Action::Repeat
                    } else {
                        key::Action::Press
                    },
                    key: k,
                    ucp,
                    mods,
                    text: String::new(),
                });
            }
            _ => {}
        }
        Ok(())
    }

    /// Encode the key pressed last, with the text that followed it.
    fn flush_key(&mut self) -> Result<()> {
        let Some(p) = self.pending_key.take() else {
            return Ok(());
        };
        let text = (!p.text.is_empty()).then_some(p.text.as_str());
        self.encode_key(p.action, p.key, p.ucp, p.mods, text)
    }

    /// A key and its text to the program, through the terminal core tai
    /// shares (src/term/keys.rs).
    fn encode_key(
        &mut self,
        action: key::Action,
        k: Key,
        ucp: char,
        mods: key::Mods,
        text: Option<&str>,
    ) -> Result<()> {
        if action != key::Action::Release {
            self.typed = true;
        }
        let k = crate::term::keys::KeyIn {
            action,
            key: k,
            ucp,
            mods,
            text,
            composing: false,
        };
        self.keys.encode(&self.terminal, k, &mut self.response)
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
    /// The row as the terminal core read it (src/term/rows.rs).
    read: crate::term::rows::Row,
    bg: Vec<Instance>,
    text: Vec<Instance>,
}

impl AsRef<crate::term::rows::Row> for RowInstances {
    fn as_ref(&self) -> &crate::term::rows::Row {
        &self.read
    }
}

/// One wheel event's travel in points, up positive. A line is WHEEL_ROWS
/// rows; on macOS a slow wheel click comes as a tenth of a line, so a line
/// event counts as at least one, as Ghostty does.
fn wheel_points(unit: egui::MouseWheelUnit, y: f32, cell: f32, page: f32) -> f32 {
    match unit {
        egui::MouseWheelUnit::Point => y,
        egui::MouseWheelUnit::Line if cfg!(target_os = "macos") && y != 0.0 => {
            y.signum() * y.abs().max(1.0) * cell * WHEEL_ROWS
        }
        egui::MouseWheelUnit::Line => y * cell * WHEEL_ROWS,
        egui::MouseWheelUnit::Page => y * page,
    }
}

// Rows one line of a mouse wheel (a notch) scrolls: the terminal core's
// (src/term/pointer.rs). A trackpad's points stay one to one.
use crate::term::pointer::WHEEL_ROWS;

/// This frame's wheel travel in points, up positive. Ctrl and Cmd with the
/// wheel zoom in egui, so they don't count.
fn wheel_travel(input: &egui::InputState, cell: f32, page: f32) -> f32 {
    input
        .events
        .iter()
        .filter_map(|e| match e {
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
        .sum()
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

/// The key tai's shortcut table (shortcuts.rs) names for an egui key, for
/// the keys it binds. egui reports Shift+= as Plus.
fn shortcut_key(k: egui::Key) -> Option<macroquad::input::KeyCode> {
    use egui::Key as E;
    use macroquad::input::KeyCode as K;
    Some(match k {
        E::C => K::C,
        E::V => K::V,
        E::Equals | E::Plus => K::Equal,
        E::Minus => K::Minus,
        E::Num0 => K::Key0,
        _ => return None,
    })
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

    /// tai's shortcuts hold in tai-gui too: their keys never reach the
    /// program, and plain Ctrl+C still does off macOS.
    #[test]
    fn app_shortcuts_are_tais() {
        use libghostty_vt::key::Mods;
        let is_shortcut = |k: egui::Key, mods: Mods, macos: bool| {
            super::shortcut_key(k)
                .is_some_and(|kc| crate::shortcuts::action_for(kc, mods, macos).is_some())
        };
        assert!(is_shortcut(egui::Key::Equals, Mods::SUPER, true));
        assert!(is_shortcut(
            egui::Key::Plus,
            Mods::SUPER | Mods::SHIFT,
            true
        ));
        assert!(is_shortcut(egui::Key::C, Mods::SUPER, true));
        assert!(is_shortcut(egui::Key::Minus, Mods::CTRL, false));
        assert!(is_shortcut(egui::Key::C, Mods::CTRL | Mods::SHIFT, false));
        assert!(!is_shortcut(egui::Key::C, Mods::CTRL, false));
        assert!(!is_shortcut(egui::Key::A, Mods::SUPER, true));
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
    fn wheel_units_become_points() {
        // A line (a notch) is WHEEL_ROWS rows, as in Ghostty; a page is
        // the screen.
        assert_eq!(
            wheel_points(egui::MouseWheelUnit::Line, 3.0, 24.0, 480.0),
            216.0
        );
        assert_eq!(
            wheel_points(egui::MouseWheelUnit::Page, -1.0, 24.0, 480.0),
            -480.0
        );
        let tenth = wheel_points(egui::MouseWheelUnit::Line, 0.1, 24.0, 480.0);
        if cfg!(target_os = "macos") {
            assert_eq!(tenth, 72.0);
        } else {
            assert!((tenth - 7.2).abs() < 1e-4);
        }
    }
}
