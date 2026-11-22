//! tai-gui: the egui prototype of ADR 0011.
//!
//! One eframe window on wgpu with two tab stacks (egui_dock): chat beside a
//! terminal and preview stack. The terminal is a libghostty-vt pane on
//! tai's pty (step 2); chat holds a conversation with the engine; preview
//! shows the runs `/run` starts (`runs.rs`). Build and run with
//! `scripts/cargo.sh run --features gui --bin tai-gui`.
//!
//! wgpu's validation: egui-wgpu creates the instance with
//! `InstanceFlags::from_build_config().with_env()` (egui-wgpu 0.36.2,
//! `src/setup.rs`), so `VALIDATION_INDIRECT_CALL` is on and
//! `WGPU_VALIDATION_INDIRECT_CALL=0` turns it off. On Intel Macs tai turns
//! it off unless that variable is set (ADR 0011 decision 3). Debug builds
//! also turn on full validation, so measure in release.

use eframe::egui;
use egui_dock::{DockArea, DockState, NodeIndex, Style, TabViewer, tab_viewer::OnCloseResponse};

// Modules shared with the macroquad build. Each is compiled into both
// binaries; tai-gui uses only part of some.
#[path = "../../cells.rs"]
#[allow(dead_code)]
mod cells;
#[path = "../../discovery.rs"]
mod discovery;
#[path = "../../fonts.rs"]
#[allow(dead_code)]
mod fonts;
#[path = "../../identity.rs"]
mod identity;
#[cfg(unix)]
#[path = "../../locale.rs"]
mod locale;
#[cfg(windows)]
#[path = "../../pty_windows.rs"]
#[allow(dead_code)]
mod pty;
#[cfg(unix)]
#[path = "../../pty_unix.rs"]
#[allow(dead_code)]
mod pty;
#[path = "../../pty_input.rs"]
mod pty_input;
#[path = "../../selection.rs"]
mod selection;
#[path = "../../shortcuts.rs"]
#[allow(dead_code)]
mod shortcuts;

mod chat;
mod engine;
mod gpu;
mod grammar;
mod images;
mod ppxai;
mod runs;
mod terminal;

// The pty modules size the pseudo terminal from `crate::Dimensions`.
use terminal::{Dimensions, TerminalPane};

/// The UI's proportional font (fonts/README.md). Only tai-gui uses it.
const INTER_BYTES: &[u8] = include_bytes!("../../../fonts/Inter-Regular.ttf");

/// Whether wgpu's indirect-call validation starts off: on Intel Macs, when
/// WGPU_VALIDATION_INDIRECT_CALL isn't set (ADR 0011 decision 3). An x86_64
/// build under Rosetta also counts; the check it skips is a safety net, not
/// something tai relies on.
fn indirect_validation_off_by_default() -> bool {
    cfg!(all(target_os = "macos", target_arch = "x86_64"))
        && std::env::var_os("WGPU_VALIDATION_INDIRECT_CALL").is_none()
}

/// What a tab shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Tab {
    Chat,
    Terminal,
    Preview,
    /// An image opened from an answer or by `/show`, by its index in
    /// `App::images`.
    Image(usize),
}

impl Tab {
    fn title(self) -> &'static str {
        match self {
            Tab::Chat => "Chat",
            Tab::Terminal => "Terminal",
            Tab::Preview => "Preview",
            Tab::Image(_) => "Image",
        }
    }
}

/// The starting layout: chat on the left; terminal and preview stacked on
/// the right, so rotating between them can be tried.
fn initial_layout() -> DockState<Tab> {
    let mut dock = DockState::new(vec![Tab::Chat]);
    dock.main_surface_mut()
        .split_right(NodeIndex::root(), 0.45, vec![Tab::Terminal, Tab::Preview]);
    dock
}

struct Panes<'a> {
    terminal: Option<&'a mut TerminalPane>,
    chat: &'a mut chat::ChatPane,
    runs: &'a mut runs::RunsPane,
    engine: &'a engine::Engine,
    images: &'a [std::path::PathBuf],
    /// Tabs closed this frame; the app keeps them to show again.
    closed: Vec<Tab>,
}

impl TabViewer for Panes<'_> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        egui::Id::new(*tab)
    }

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        match tab {
            Tab::Image(i) => self
                .images
                .get(*i)
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Image".into())
                .into(),
            tab => tab.title().into(),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        match (tab, self.terminal.as_deref_mut()) {
            (Tab::Terminal, Some(terminal)) => terminal.ui(ui),
            (Tab::Terminal, None) => {
                ui.label("The terminal didn't start; see the log.");
            }
            (Tab::Chat, _) => self.chat.ui(ui, self.engine),
            (Tab::Preview, _) => self.runs.ui(ui),
            (Tab::Image(i), _) => match self.images.get(*i) {
                Some(path) => images::show_file(ui, path),
                None => {
                    ui.label("No image.");
                }
            },
        }
    }

    // The terminal draws to its own edges.
    fn clear_background(&self, tab: &Tab) -> bool {
        *tab != Tab::Terminal
    }

    // Every pane scrolls itself.
    fn scroll_bars(&self, _tab: &Tab) -> [bool; 2] {
        [false, false]
    }

    // Closing a tab hides it: its pane lives on in the app (a terminal's
    // shell keeps running) and View > Show puts it back.
    fn is_closeable(&self, _tab: &Tab) -> bool {
        true
    }

    fn on_close(&mut self, tab: &mut Tab) -> OnCloseResponse {
        // An image tab just closes; answers and /show open it again.
        if !matches!(tab, Tab::Image(_)) {
            self.closed.push(*tab);
        }
        OnCloseResponse::Close
    }
}

/// The chord that zooms the focused pane to the whole window and back, as
/// in WezTerm (Ctrl+Shift+Z) and tmux (prefix z). Ctrl on every platform,
/// as WezTerm does on macOS, since Cmd+Shift+Z is Redo there.
fn zoom_chord(i: &mut egui::InputState) -> bool {
    let mods = egui::Modifiers {
        ctrl: true,
        shift: true,
        ..Default::default()
    };
    i.consume_key(mods, egui::Key::Z)
}

/// Frames per second and what asked for them, logged when
/// `TAI_FRAME_STATS=1`, for ADR 0011's measurements.
struct FrameStats {
    since: std::time::Instant,
    frames: u32,
    busy: std::time::Duration,
    max: std::time::Duration,
    causes: std::collections::BTreeMap<String, u32>,
}

impl FrameStats {
    fn new() -> Option<Self> {
        (std::env::var("TAI_FRAME_STATS").as_deref() == Ok("1")).then(|| Self {
            since: std::time::Instant::now(),
            frames: 0,
            busy: std::time::Duration::ZERO,
            max: std::time::Duration::ZERO,
            causes: Default::default(),
        })
    }

    fn record(&mut self, ctx: &egui::Context, busy: std::time::Duration) {
        self.frames += 1;
        self.busy += busy;
        self.max = self.max.max(busy);
        for cause in ctx.repaint_causes() {
            *self
                .causes
                .entry(format!("{}:{}", cause.file, cause.line))
                .or_default() += 1;
        }
        if self.since.elapsed() >= std::time::Duration::from_secs(1) {
            tracing::info!(
                "frames: {} in {:.2} s, ui time avg {:.2} max {:.2} ms; repaint causes: {:?}",
                self.frames,
                self.since.elapsed().as_secs_f32(),
                self.busy.as_secs_f32() * 1000.0 / self.frames as f32,
                self.max.as_secs_f32() * 1000.0,
                self.causes
            );
            *self = Self {
                since: std::time::Instant::now(),
                frames: 0,
                busy: std::time::Duration::ZERO,
                max: std::time::Duration::ZERO,
                causes: Default::default(),
            };
        }
    }
}

struct App {
    stats: Option<FrameStats>,
    dock: DockState<Tab>,
    /// Tabs the user closed, in the order they closed them.
    hidden: Vec<Tab>,
    /// Where each tab was last seen in the dock, so a hidden tab comes
    /// back to its own stack.
    last_place: std::collections::HashMap<Tab, egui_dock::NodePath>,
    /// The pane shown over the whole window, if one is zoomed.
    zoomed: Option<Tab>,
    terminal: Option<TerminalPane>,
    chat: chat::ChatPane,
    runs: runs::RunsPane,
    engine: engine::Engine,
    /// The images opened in tabs, which `Tab::Image` indexes.
    images: Vec<std::path::PathBuf>,
}

impl App {
    /// `/terminal`, or a command to type into a terminal (ppxai's
    /// `open_terminal` and `run_shell`). A running shell is brought
    /// forward where it is; one that has exited, or never started, is
    /// started again in the folder the request names.
    fn open_terminal(&mut self, request: chat::TerminalRequest, ctx: &egui::Context) {
        if self.terminal.as_ref().is_none_or(TerminalPane::exited) {
            let cwd = request.cwd.as_deref().map(std::path::Path::new);
            match fonts::Coverage::new()
                .map_err(Into::into)
                .and_then(|coverage| TerminalPane::new(coverage, ctx.clone(), cwd))
            {
                Ok(terminal) => self.terminal = Some(terminal),
                Err(e) => {
                    tracing::error!("the terminal didn't start: {e}");
                    self.chat.note(format!("The terminal didn't start: {e}"));
                    return;
                }
            }
        }
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        match request.command {
            Some(command) => match terminal.type_command(&command) {
                Ok(true) => {}
                Ok(false) => self.chat.note(format!(
                    "A program is running in the terminal, so this wasn't typed there: `{command}`"
                )),
                Err(e) => {
                    tracing::warn!("typing a command: {e}");
                    self.chat.note(format!("Run in the terminal: `{command}`"));
                }
            },
            None => terminal.focus(),
        }
        self.reveal(Tab::Terminal);
        if let Some(path) = self.dock.find_tab(&Tab::Terminal) {
            self.dock.set_focused_node_and_surface(egui_dock::NodePath {
                surface: path.surface,
                node: path.node,
            });
        }
    }

    /// Show an image in the Preview stack, in its own tab, or bring its tab
    /// forward if it's open already.
    fn open_image(&mut self, path: std::path::PathBuf) {
        let i = match self.images.iter().position(|p| *p == path) {
            Some(i) => i,
            None => {
                self.images.push(path);
                self.images.len() - 1
            }
        };
        let tab = Tab::Image(i);
        if self.dock.find_tab(&tab).is_none() {
            let preview = self
                .dock
                .find_tab(&Tab::Preview)
                .map(|p| egui_dock::NodePath {
                    surface: p.surface,
                    node: p.node,
                })
                .or_else(|| self.last_place.get(&Tab::Preview).copied());
            match preview.and_then(|path| self.dock.leaf_mut(path).ok()) {
                Some(leaf) => leaf.append_tab(tab),
                None => self.dock.push_to_first_leaf(tab),
            }
        }
        if let Some(path) = self.dock.find_tab(&tab) {
            _ = self.dock.set_active_tab(path);
        }
        self.zoomed = None;
    }

    /// Zoom the focused pane, or the first tab when none has focus yet;
    /// zoomed already, go back to the layout.
    fn toggle_zoom(&mut self) {
        self.zoomed = match self.zoomed {
            Some(_) => None,
            None => self
                .dock
                .find_active_focused()
                .map(|(_, tab)| *tab)
                .or_else(|| self.dock.iter_all_tabs().next().map(|(_, tab)| *tab)),
        };
    }

    /// Put a hidden tab back in the stack it was closed from, as its
    /// active tab, or in the first stack if that one has gone (closing a
    /// stack's last tab removes the stack).
    fn show(&mut self, tab: Tab) {
        let place = self.last_place.get(&tab).copied();
        match place.and_then(|path| self.dock.leaf_mut(path).ok()) {
            Some(leaf) => leaf.append_tab(tab),
            None => self.dock.push_to_first_leaf(tab),
        }
        if let Some(path) = self.dock.find_tab(&tab) {
            _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(egui_dock::NodePath {
                surface: path.surface,
                node: path.node,
            });
        }
    }

    /// Bring a tab to the front, hidden or not, without taking the focus
    /// from the pane that has it.
    fn reveal(&mut self, tab: Tab) {
        if let Some(i) = self.hidden.iter().position(|t| *t == tab) {
            self.hidden.remove(i);
            let focused = self.dock.focused_leaf();
            self.show(tab);
            if let Some(path) = focused {
                self.dock.set_focused_node_and_surface(path);
            }
        } else if let Some(path) = self.dock.find_tab(&tab) {
            _ = self.dock.set_active_tab(path);
        }
        if self.zoomed.is_some_and(|z| z != tab) {
            self.zoomed = None;
        }
    }

    /// The View menu: zoom, and the hidden tabs to show again.
    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("menu-bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("View", |ui| {
                    let zoom = if self.zoomed.is_some() {
                        "Restore layout"
                    } else {
                        "Zoom pane"
                    };
                    let button = egui::Button::new(zoom).shortcut_text("Ctrl+Shift+Z");
                    if ui.add(button).clicked() {
                        self.toggle_zoom();
                        ui.close();
                    }
                    ui.separator();
                    if self.hidden.is_empty() {
                        ui.weak("No hidden panes");
                    }
                    let mut shown = None;
                    for (i, tab) in self.hidden.iter().enumerate() {
                        if ui.button(format!("Show {}", tab.title())).clicked() {
                            shown = Some(i);
                            ui.close();
                        }
                    }
                    if let Some(i) = shown {
                        let tab = self.hidden.remove(i);
                        self.zoomed = None;
                        self.show(tab);
                    }
                });
                if let Some(tab) = self.zoomed {
                    ui.weak(format!(
                        "{} zoomed: Ctrl+Shift+Z restores the layout",
                        tab.title()
                    ));
                }
            });
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let started = std::time::Instant::now();
        // Read the shell's output even while its tab is hidden.
        if let Some(terminal) = &mut self.terminal {
            // The pty's reader wakes egui when output arrives. A frame
            // reads only so much of it (terminal.rs, PARSE_BUDGET), so
            // after a read, come back for the rest.
            if terminal.pump() {
                ui.ctx().request_repaint();
            }
        }
        // Taken before any pane sees the keys, so the shell doesn't get it.
        if ui.input_mut(zoom_chord) {
            self.toggle_zoom();
        }
        self.menu_bar(ui);

        for (path, tab) in self.dock.iter_all_tabs() {
            self.last_place.insert(
                *tab,
                egui_dock::NodePath {
                    surface: path.surface,
                    node: path.node,
                },
            );
        }
        let mut panes = Panes {
            terminal: self.terminal.as_mut(),
            chat: &mut self.chat,
            runs: &mut self.runs,
            engine: &self.engine,
            images: &self.images,
            closed: Vec::new(),
        };
        if let Some(mut tab) = self.zoomed {
            // The dock keeps its layout; the zoomed pane is drawn alone.
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| panes.ui(ui, &mut tab));
        } else {
            let style = Style::from_egui(ui.style().as_ref());
            DockArea::new(&mut self.dock)
                .style(style)
                .show_leaf_close_all_buttons(false)
                .show_inside(ui, &mut panes);
        }
        let closed = std::mem::take(&mut panes.closed);
        self.hidden.extend(closed);
        // Runs started in chat open in the preview, and what the run pane
        // did goes into the transcript.
        let opened = self.chat.take_opened();
        if !opened.is_empty() {
            for open in opened {
                self.runs.open(open, ui.ctx());
            }
            self.reveal(Tab::Preview);
        }
        for note in self.runs.take_notes() {
            self.chat.note(note);
        }
        for request in self.chat.take_terminals() {
            self.open_terminal(request, ui.ctx());
        }
        // Answers are model output, and a Markdown link can name any
        // scheme: file:, or a Windows protocol handler. Only web links open
        // (in the browser); an answer's "open in Preview" link opens its
        // image here. The hover still shows the full target.
        let mut open = images::take_open();
        ui.ctx().output_mut(|o| {
            o.commands.retain(|command| match command {
                egui::OutputCommand::OpenUrl(link) => {
                    if let Some(path) = link.url.strip_prefix(images::OPEN_SCHEME) {
                        open.push(path.into());
                        false
                    } else if chat::is_web_link(&link.url) {
                        true
                    } else {
                        tracing::warn!("not opening a non-web link: {}", link.url);
                        false
                    }
                }
                _ => true,
            })
        });
        for path in open {
            self.open_image(path);
        }
        if let Some(stats) = &mut self.stats {
            stats.record(ui.ctx(), started.elapsed());
        }
    }

    fn on_exit(&mut self) {
        self.runs.stop();
        tracing::info!("exit: stopping the engine");
        self.engine.stop();
        tracing::info!("exit: closing the terminal");
        drop(self.terminal.take());
        tracing::info!("exit: done");
    }
}

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // egui_commonmark asks egui_extras for image loaders, which
                // tai-gui builds without: its own are in images.rs.
                "info,egui_extras::loaders=error".into()
            }),
        )
        .init();

    let mut options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title("tai")
            .with_inner_size([1200.0, 760.0]),
        ..Default::default()
    };
    // egui-wgpu prefers the high-performance GPU, which on a laptop with two
    // wakes the discrete one for a mostly idle window. A terminal doesn't
    // need it, so prefer the integrated GPU; WGPU_POWER_PREF=high overrides.
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
        setup.power_preference = eframe::wgpu::PowerPreference::from_env()
            .unwrap_or(eframe::wgpu::PowerPreference::LowPower);
        // ADR 0011: on Windows, GL by default. Vulkan cost about 300 MB
        // more on the AMD host (531 against 224 MB) and DX12 didn't start
        // there (DxcCreateInstance fails). WGPU_BACKEND overrides.
        if cfg!(windows) && eframe::wgpu::Backends::from_env().is_none() {
            setup.instance_descriptor.backends = eframe::wgpu::Backends::GL;
        }
        // ADR 0011 decision 3: indirect-call validation's compute pipeline
        // crashes the Metal shader compiler on the Intel Iris Pro Mac, and
        // the device is lost. Off on Intel Macs unless
        // WGPU_VALIDATION_INDIRECT_CALL says otherwise (=1 keeps it on).
        if indirect_validation_off_by_default() {
            setup
                .instance_descriptor
                .flags
                .remove(eframe::wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL);
        }
    }
    let result = eframe::run_native(
        "tai",
        options,
        Box::new(|cc| {
            if let Some(render) = &cc.wgpu_render_state {
                let info = render.adapter.get_info();
                tracing::info!(
                    "GPU: {} ({:?}, {:?})",
                    info.name,
                    info.backend,
                    info.device_type
                );
            }
            if let Some(render) = &cc.wgpu_render_state {
                gpu::CellPass::install(render);
            }
            // A blinking text cursor in a focused text box kept egui
            // drawing about 30 frames a second (6-12% of a core on the
            // Windows host), so the chat input's cursor holds still.
            cc.egui_ctx
                .all_styles_mut(|style| style.visuals.text_cursor.blink = false);
            // Chat links come from model output: show where one goes before
            // it's clicked. egui shows a link's URL on hover only with this
            // set (egui 0.36.2 `widgets/hyperlink.rs`); it's off by default.
            cc.egui_ctx
                .all_styles_mut(|style| style.url_in_tooltip = true);
            // Code in chat answers (Markdown code blocks and inline code)
            // uses egui's monospace family. Put the terminal's font first in
            // it, so code reads the same in both panes.
            cc.egui_ctx.add_font(egui::epaint::text::FontInsert::new(
                "JetBrainsMono-Medium",
                egui::FontData::from_static(fonts::PRIMARY_BYTES),
                vec![egui::epaint::text::InsertFontFamily {
                    family: egui::FontFamily::Monospace,
                    priority: egui::epaint::text::FontPriority::Highest,
                }],
            ));
            // Prose (chat, menus, tabs) in Inter Regular. egui's own
            // proportional font is Ubuntu Light, thin to read at length.
            // Bundled like the terminal fonts (fonts/README.md).
            cc.egui_ctx.add_font(egui::epaint::text::FontInsert::new(
                "Inter-Regular",
                egui::FontData::from_static(INTER_BYTES),
                vec![egui::epaint::text::InsertFontFamily {
                    family: egui::FontFamily::Proportional,
                    priority: egui::epaint::text::FontPriority::Highest,
                }],
            ));
            images::install(&cc.egui_ctx);
            let terminal = fonts::Coverage::new()
                .map_err(Into::into)
                .and_then(|coverage| TerminalPane::new(coverage, cc.egui_ctx.clone(), None))
                .map_err(|e| tracing::error!("the terminal didn't start: {e}"))
                .ok();
            Ok(Box::new(App {
                dock: initial_layout(),
                terminal,
                chat: chat::ChatPane::new(),
                runs: runs::RunsPane::default(),
                hidden: Vec::new(),
                last_place: Default::default(),
                zoomed: None,
                stats: FrameStats::new(),
                engine: engine::Engine::start(cc.egui_ctx.clone()),
                images: Vec::new(),
            }))
        }),
    );
    // ADR 0011 decision 3: name the opt-out when the GPU device didn't
    // start. wgpu's indirect-draw validation crashes the shader compiler
    // on some Intel Macs (Iris Pro, macOS 12), and the device is lost.
    if let Err(eframe::Error::Wgpu(e)) = &result
        && std::env::var("WGPU_VALIDATION_INDIRECT_CALL").as_deref() != Ok("0")
        && !indirect_validation_off_by_default()
    {
        tracing::error!(
            "the GPU device didn't start ({e}). If the log above shows a shader \
             compiler error, try again with WGPU_VALIDATION_INDIRECT_CALL=0"
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{Tab, initial_layout};

    #[test]
    fn layout_has_chat_beside_a_terminal_and_preview_stack() {
        let dock = initial_layout();
        let stacks: Vec<Vec<Tab>> = dock
            .main_surface()
            .iter()
            .filter_map(|node| node.tabs().map(<[Tab]>::to_vec))
            .collect();
        assert_eq!(
            stacks,
            vec![vec![Tab::Chat], vec![Tab::Terminal, Tab::Preview]]
        );
    }
}
