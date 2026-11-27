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
    screen::{CellWide, RowSemanticPrompt, Screen},
    terminal::ScrollViewport,
};
use macroquad::color::Color;

use crate::{
    cells::{GlyphAtlas, Instance},
    fonts::{Coverage, Face, SymbolSpan},
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
    key_encoder: key::Encoder<'static>,
    key_event: key::Event<'static>,
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
            key_encoder: key::Encoder::new()?,
            key_event: key::Event::new()?,
            atlas: GlyphAtlas::new(crate::cells::ATLAS_MAX_HEIGHT),
            gpu: crate::gpu::PaneId::new(),
            instances: Arc::new(Vec::new()),
            bg_start: 0,
            text_start: 0,
            placement_it: PlacementIterator::new()?,
            images_held: Default::default(),
            sixel: crate::sixel::Translator::new(),
            built_for: None,
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
                // A row with Kitty Unicode placeholders: each cell's
                // placeholder, or None (src/term/images.rs).
                let placeholder_row = row.raw_row()?.has_kitty_virtual_placeholder()?;
                let mut placeholder_cells = Vec::new();
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
                    // A placeholder cell shows part of an image, drawn with
                    // the images; its own character has no glyph.
                    let placeholder = if placeholder_row {
                        let cell_placeholder = crate::term::images::placeholder_cell(cell)?;
                        let is = cell_placeholder.is_some();
                        placeholder_cells.push(cell_placeholder);
                        is
                    } else {
                        false
                    };
                    if graphemes > 0 && !placeholder {
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
                cached.virtual_runs = if placeholder_row {
                    crate::placeholders::row_placements(placeholder_cells)
                } else {
                    Vec::new()
                };
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
        self.bg_start = out.len() as u32;
        for row in &self.rows {
            out.extend_from_slice(&row.bg);
        }
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
            .flat_map(|(r, row)| row.virtual_runs.iter().map(move |run| (r as u16, *run)))
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
    /// The row's Kitty Unicode placeholder runs (src/placeholders.rs).
    virtual_runs: Vec<crate::placeholders::Placement>,
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
