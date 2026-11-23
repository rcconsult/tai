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
#[path = "../../shell_integration.rs"]
#[cfg_attr(windows, allow(dead_code))]
mod shell_integration;
#[path = "../../shortcuts.rs"]
#[allow(dead_code)]
mod shortcuts;

mod aliases;
mod chat;
mod editor;
mod engine;
mod gpu;
mod grammar;
mod images;
#[cfg(target_os = "macos")]
mod macos_quit;
mod ppxai;
mod runs;
mod terminal;

use aliases::{Alias, TermId};
// The pty modules size the pseudo terminal from `crate::Dimensions`.
use terminal::{Dimensions, TerminalPane};

type Terminals = aliases::Terminals<TerminalPane>;

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
    /// A terminal, by its internal ID (ADR 0013).
    Terminal(TermId),
    Preview,
    /// An image opened from an answer or by `/show`, by its index in
    /// `App::images`.
    Image(usize),
}

impl Tab {
    /// The tab's title; a terminal's comes from its aliases.
    fn title(self, terminals: &Terminals) -> String {
        match self {
            Tab::Chat => "Chat".into(),
            Tab::Terminal(id) => terminals.title(id),
            Tab::Preview => "Preview".into(),
            Tab::Image(_) => "Image".into(),
        }
    }
}

/// The starting layout: chat on the left; the first terminal and preview
/// stacked on the right, so rotating between them can be tried.
fn initial_layout(first: TermId) -> DockState<Tab> {
    let mut dock = DockState::new(vec![Tab::Chat]);
    dock.main_surface_mut().split_right(
        NodeIndex::root(),
        0.45,
        vec![Tab::Terminal(first), Tab::Preview],
    );
    dock
}

/// What a terminal tab's right-click menu asked for. egui_dock's own
/// menu items follow tai's: Eject, and Close, which goes through
/// `on_close` as the tab's close button does.
enum TabAction {
    Rename(TermId),
}

struct Panes<'a> {
    terminals: &'a mut Terminals,
    chat: &'a mut chat::ChatPane,
    runs: &'a mut runs::RunsPane,
    engine: &'a engine::Engine,
    images: &'a [std::path::PathBuf],
    /// Tabs closed this frame; the app keeps them to show again.
    closed: Vec<Tab>,
    /// Terminals closed this frame, whose shells end.
    ended: Vec<TermId>,
    /// A terminal whose tab was closed while it was busy: the app asks
    /// first.
    confirm: Option<TermId>,
    action: Option<TabAction>,
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
            tab => tab.title(self.terminals).into(),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        match tab {
            Tab::Terminal(id) => match self.terminals.pane_mut(*id) {
                Some(terminal) => terminal.ui(ui),
                None => {
                    ui.label("The terminal didn't start; see the log.");
                }
            },
            Tab::Chat => self.chat.ui(ui, self.engine),
            Tab::Preview => self.runs.ui(ui),
            Tab::Image(i) => match self.images.get(*i) {
                Some(path) => images::show_file(ui, path),
                None => {
                    ui.label("No image.");
                }
            },
        }
    }

    // A terminal's tab shows its shell's folder on hover.
    fn on_tab_button(&mut self, tab: &mut Tab, response: &egui::Response) {
        if let Tab::Terminal(id) = *tab
            && let Some(terminal) = self.terminals.pane(id)
            && let Some(folder) = terminal.folder()
        {
            response.clone().on_hover_text(folder.to_string_lossy());
        }
    }

    // A terminal's tab is renamed from its right-click menu.
    fn context_menu(&mut self, ui: &mut egui::Ui, tab: &mut Tab, _path: egui_dock::NodePath) {
        if let Tab::Terminal(id) = *tab
            && ui.button("Rename…").clicked()
        {
            self.action = Some(TabAction::Rename(id));
            ui.close();
        }
    }

    // The terminal draws to its own edges.
    fn clear_background(&self, tab: &Tab) -> bool {
        !matches!(tab, Tab::Terminal(_))
    }

    // Every pane scrolls itself.
    fn scroll_bars(&self, _tab: &Tab) -> [bool; 2] {
        [false, false]
    }

    // Closing chat's or preview's tab hides it: its pane lives on in the
    // app and View > Show puts it back. Closing a terminal's tab ends its
    // shell (ADR 0013 decision 2).
    fn is_closeable(&self, _tab: &Tab) -> bool {
        true
    }

    fn on_close(&mut self, tab: &mut Tab) -> OnCloseResponse {
        match *tab {
            // An image tab just closes; answers and /show open it again.
            Tab::Image(_) => {}
            // A busy terminal asks first, as Ghostty's
            // confirm-close-surface does.
            Tab::Terminal(id) if self.terminals.pane(id).is_some_and(TerminalPane::busy) => {
                self.confirm = Some(id);
                return OnCloseResponse::Ignore;
            }
            Tab::Terminal(id) => self.ended.push(id),
            tab => self.closed.push(tab),
        }
        OnCloseResponse::Close
    }
}

/// Quit: Cmd+Q on macOS and Ctrl+Shift+Q elsewhere, as in Ghostty
/// (src/config/Config.zig at 108cacb1). egui's own quit shortcut,
/// Command+Q, is Ctrl+Q off macOS, which belongs to the terminal (vim's
/// block selection, emacs' quoted insert, micro's quit), and egui checks it
/// only after the panes have sent it on; so it's turned off at startup and
/// this one is taken before any pane sees the keys (popos-tai).
const QUIT: egui::KeyboardShortcut = egui::KeyboardShortcut::new(
    if cfg!(target_os = "macos") {
        egui::Modifiers::COMMAND
    } else {
        egui::Modifiers::CTRL.plus(egui::Modifiers::SHIFT)
    },
    egui::Key::Q,
);

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

/// A terminal tab's rename box.
struct Rename {
    id: TermId,
    text: String,
    error: Option<String>,
    /// The text box has taken the focus once.
    focused: bool,
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
    /// The terminals by ID, with their aliases. A pane is None when its
    /// shell didn't start.
    terminals: Terminals,
    /// The terminal that last had the focus, where `/terminal` goes.
    last_terminal: Option<TermId>,
    /// Where a terminal tab was last seen, so a new one goes there when
    /// every terminal has closed.
    terminal_place: Option<egui_dock::NodePath>,
    /// A busy terminal whose tab was closed, waiting for the user's yes.
    confirm_close: Option<TermId>,
    /// The rename box a terminal tab's menu opened.
    renaming: Option<Rename>,
    /// Terminals opened for `/edit`'s editor, which close once their shell
    /// exits (editor.rs types `&& exit` after the editor).
    editors: Vec<TermId>,
    /// The busy terminals' titles, while the question before quitting is
    /// up.
    confirm_quit: Option<Vec<String>>,
    /// The user said quit: the next close request goes through.
    quit_confirmed: bool,
    /// macOS: the app menu's Quit now closes the window (macos_quit.rs).
    #[cfg(target_os = "macos")]
    quit_rerouted: bool,
    chat: chat::ChatPane,
    runs: runs::RunsPane,
    engine: engine::Engine,
    /// The images opened in tabs, which `Tab::Image` indexes.
    images: Vec<std::path::PathBuf>,
}

impl App {
    /// `/terminal [alias]`, or a command to type into a terminal (ppxai's
    /// `open_terminal` and `run_shell`), for the terminal the alias names,
    /// or the one that last had the focus (ADR 0013 decision 3). A running
    /// shell is brought forward where it is; one that has exited, or never
    /// started, is started again in the folder the request names; an
    /// alias no terminal holds gets a new terminal.
    fn open_terminal(&mut self, request: chat::TerminalRequest, ctx: &egui::Context) {
        let (id, new) = match request.name.as_deref().map(Alias::parse) {
            _ if request.new => (self.terminals.add(None, None, None), true),
            Some(Err(why)) => {
                self.chat.note(format!("No terminal opened: {why}."));
                return;
            }
            Some(Ok(alias)) => match self.terminals.find(&alias) {
                Some(id) => (id, false),
                None => (self.terminals.add_for(&alias, None), true),
            },
            None => match self
                .last_terminal
                .filter(|id| self.terminals.contains(*id))
                .or_else(|| self.terminals.first())
            {
                Some(id) => (id, false),
                None => (self.terminals.add(None, None, None), true),
            },
        };
        self.last_terminal = Some(id);
        if request.run && new {
            self.editors.push(id);
        }
        let running = self.terminals.pane(id).is_some_and(|t| !t.exited());
        if !running && !self.start_terminal(id, request.cwd.as_deref(), ctx) {
            // A new terminal that didn't start doesn't keep its alias.
            if new {
                self.terminals.remove(id);
            }
            return;
        }
        let title = self.terminals.title(id);
        let Some(terminal) = self.terminals.pane_mut(id) else {
            return;
        };
        match request.command {
            Some(command) => match terminal.type_command(&command, request.run) {
                Ok(true) => {}
                Ok(false) => self.chat.note(format!(
                    "{title} isn't at its prompt, so this wasn't typed there: `{command}`"
                )),
                Err(e) => {
                    tracing::warn!("typing a command: {e}");
                    self.chat.note(format!("Run in {title}: `{command}`"));
                }
            },
            None => terminal.focus(),
        }
        self.focus_tab(Tab::Terminal(id));
    }

    /// View > New terminal: a shell in the conversation's folder, in a tab
    /// beside the terminal that last had the focus.
    fn new_terminal(&mut self, ctx: &egui::Context) {
        let id = self.terminals.add(None, None, None);
        let folder = self.chat.folder().map(str::to_string);
        if !self.start_terminal(id, folder.as_deref(), ctx) {
            self.terminals.remove(id);
            return;
        }
        if let Some(terminal) = self.terminals.pane_mut(id) {
            terminal.focus();
        }
        self.focus_tab(Tab::Terminal(id));
    }

    /// Start a shell for terminal `id`, in `cwd` or tai-gui's own folder.
    /// Says so in the chat when it fails.
    fn start_terminal(&mut self, id: TermId, cwd: Option<&str>, ctx: &egui::Context) -> bool {
        let cwd = cwd.map(std::path::Path::new);
        match fonts::Coverage::new()
            .map_err(Into::into)
            .and_then(|coverage| TerminalPane::new(coverage, ctx.clone(), cwd))
        {
            Ok(terminal) => {
                // A restarted shell's old pane, exited, goes with it.
                if let Some(old) = self.terminals.remove_pane(id) {
                    old.close();
                }
                self.terminals.set_pane(id, terminal);
                true
            }
            Err(e) => {
                tracing::error!("the terminal didn't start: {e}");
                self.chat.note(format!("The terminal didn't start: {e}"));
                false
            }
        }
    }

    /// Bring a tab to the front with the focus: shown again when hidden,
    /// put beside the last terminal (or the first stack) when it isn't in
    /// the dock at all.
    fn focus_tab(&mut self, tab: Tab) {
        if self.dock.find_tab(&tab).is_none() && !self.hidden.contains(&tab) {
            let last = self.last_terminal.filter(|id| Tab::Terminal(*id) != tab);
            let beside = last
                .and_then(|id| self.dock.find_tab(&Tab::Terminal(id)))
                .or_else(|| {
                    self.dock
                        .iter_all_tabs()
                        .find(|(_, t)| matches!(t, Tab::Terminal(_)) && **t != tab)
                        .and_then(|(_, t)| self.dock.find_tab(t))
                })
                .map(|p| egui_dock::NodePath {
                    surface: p.surface,
                    node: p.node,
                })
                .or(self.terminal_place);
            match beside.and_then(|path| self.dock.leaf_mut(path).ok()) {
                Some(leaf) => leaf.append_tab(tab),
                None => self.dock.push_to_first_leaf(tab),
            }
        }
        self.reveal(tab);
        if let Some(path) = self.dock.find_tab(&tab) {
            _ = self.dock.set_active_tab(path);
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

    /// Close a terminal: its tab goes, its shell ends off the UI thread and
    /// its aliases are free again.
    fn end_terminal(&mut self, id: TermId) {
        if let Some(path) = self.dock.find_tab(&Tab::Terminal(id)) {
            self.dock.remove_tab(path);
        }
        if let Some(pane) = self.terminals.remove(id) {
            // Dropping the pane closes the pseudo console on the UI
            // thread; logged to see that it doesn't hold the window.
            let started = std::time::Instant::now();
            pane.close();
            tracing::info!("closed a terminal in {:?}", started.elapsed());
        }
        if self.zoomed == Some(Tab::Terminal(id)) {
            self.zoomed = None;
        }
        if self.last_terminal == Some(id) {
            self.last_terminal = None;
        }
    }

    /// The questions before a busy terminal closes and before quitting,
    /// and the rename box.
    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(busy) = &self.confirm_quit {
            let mut answer = None;
            let modal = egui::Modal::new(egui::Id::new("confirm-quit")).show(ctx, |ui| {
                ui.strong("Quit tai?");
                ui.label(format!(
                    "These terminals may be running a command: {}. Quitting \
                     ends their shells and everything running in them.",
                    busy.join(", ")
                ));
                ui.horizontal(|ui| {
                    if ui.button("Quit").clicked() {
                        answer = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        answer = Some(false);
                    }
                });
            });
            if modal.should_close() && answer.is_none() {
                answer = Some(false);
            }
            match answer {
                Some(true) => {
                    self.confirm_quit = None;
                    self.quit_confirmed = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Some(false) => self.confirm_quit = None,
                None => {}
            }
        }
        if let Some(id) = self.confirm_close {
            let title = self.terminals.title(id);
            let mut answer = None;
            let modal = egui::Modal::new(egui::Id::new("confirm-close-terminal")).show(ctx, |ui| {
                ui.strong(format!("Close {title}?"));
                ui.label(
                    "Its shell may be running a command. Closing ends the shell \
                     and everything running in it.",
                );
                ui.horizontal(|ui| {
                    if ui.button("Close").clicked() {
                        answer = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        answer = Some(false);
                    }
                });
            });
            if modal.should_close() && answer.is_none() {
                answer = Some(false);
            }
            match answer {
                Some(true) => {
                    self.confirm_close = None;
                    self.end_terminal(id);
                }
                Some(false) => self.confirm_close = None,
                None => {}
            }
        }
        if let Some(rename) = &mut self.renaming {
            let title = self.terminals.title(rename.id);
            let mut done = None;
            let modal = egui::Modal::new(egui::Id::new("rename-terminal")).show(ctx, |ui| {
                ui.strong(format!("Rename {title}"));
                ui.weak(
                    "A name (lowercase letters, digits and -, with a letter), \
                     or a number to renumber it; empty takes the name away.",
                );
                let edit = ui.text_edit_singleline(&mut rename.text);
                if !rename.focused {
                    edit.request_focus();
                    // Selected, so a retry after a refusal replaces the
                    // refused text instead of adding to it (popos-tai).
                    select_all(ui.ctx(), edit.id, &rename.text);
                    rename.focused = true;
                }
                let entered = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if let Some(error) = &rename.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                ui.horizontal(|ui| {
                    if ui.button("Rename").clicked() || entered {
                        done = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        done = Some(false);
                    }
                });
            });
            if modal.should_close() && done.is_none() {
                done = Some(false);
            }
            match done {
                Some(true) => {
                    let text = rename.text.trim();
                    let done = match Alias::parse(text) {
                        _ if text.is_empty() => self.terminals.rename(rename.id, None),
                        Ok(Alias::Number(n)) => self.terminals.renumber(rename.id, n),
                        Ok(Alias::Name(name)) => self.terminals.rename(rename.id, Some(&name)),
                        Err(why) => Err(why),
                    };
                    match done {
                        Ok(()) => self.renaming = None,
                        Err(why) => {
                            rename.error = Some(why);
                            // Enter took the focus from the box: give it
                            // back, so the next try can be typed.
                            rename.focused = false;
                        }
                    }
                }
                Some(false) => self.renaming = None,
                None => {}
            }
        }
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
        let mut new_terminal = false;
        egui::Panel::top("menu-bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("View", |ui| {
                    if ui.button("New terminal").clicked() {
                        new_terminal = true;
                        ui.close();
                    }
                    ui.separator();
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
                        let title = tab.title(&self.terminals);
                        if ui.button(format!("Show {title}")).clicked() {
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
                        tab.title(&self.terminals)
                    ));
                }
            });
        });
        if new_terminal {
            self.new_terminal(ui.ctx());
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let started = std::time::Instant::now();
        #[cfg(target_os = "macos")]
        if !self.quit_rerouted {
            self.quit_rerouted = macos_quit::quit_closes_the_window();
        }
        // Quitting with a busy terminal asks first, as closing its tab
        // does; Ghostty's confirm-close-surface covers quitting too
        // (src/config/Config.zig at 108cacb1).
        if !self.quit_confirmed && ui.ctx().input(|i| i.viewport().close_requested()) {
            let busy = self.terminals.titles_where(TerminalPane::busy);
            if !busy.is_empty() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.confirm_quit = Some(busy);
            }
        }
        // Read the shells' output even while their tabs are hidden.
        for terminal in self.terminals.panes_mut() {
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
        // A close request, which asks first when a terminal is busy.
        if ui.input_mut(|i| i.consume_shortcut(&QUIT)) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.menu_bar(ui);

        for (path, tab) in self.dock.iter_all_tabs() {
            let place = egui_dock::NodePath {
                surface: path.surface,
                node: path.node,
            };
            if matches!(tab, Tab::Terminal(_)) {
                self.terminal_place = Some(place);
            } else {
                self.last_place.insert(*tab, place);
            }
        }
        // An editor's terminal closes once its shell has exited; one where
        // the editor didn't start stays, with the shell's error.
        let done: Vec<TermId> = self
            .editors
            .iter()
            .copied()
            .filter(|&id| self.terminals.pane(id).is_none_or(TerminalPane::exited))
            .collect();
        self.editors.retain(|id| !done.contains(id));
        for id in done {
            if self.terminals.contains(id) {
                self.end_terminal(id);
            }
        }
        // The chat can take the folder of the terminal that last had the
        // focus.
        let folder = self
            .last_terminal
            .and_then(|id| self.terminals.pane(id))
            .filter(|t| !t.exited())
            .and_then(TerminalPane::folder);
        self.chat.set_terminal_folder(folder);
        let mut panes = Panes {
            terminals: &mut self.terminals,
            chat: &mut self.chat,
            runs: &mut self.runs,
            engine: &self.engine,
            images: &self.images,
            closed: Vec::new(),
            ended: Vec::new(),
            confirm: None,
            action: None,
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
        let ended = std::mem::take(&mut panes.ended);
        let confirm = panes.confirm.take();
        let action = panes.action.take();
        self.hidden.extend(closed);
        for id in ended {
            self.end_terminal(id);
        }
        if confirm.is_some() {
            self.confirm_close = confirm;
        }
        if let Some(TabAction::Rename(id)) = action {
            self.renaming = Some(Rename {
                id,
                text: self.terminals.name(id).unwrap_or_default().to_string(),
                error: None,
                focused: false,
            });
        }
        self.dialogs(ui.ctx());
        if let Some((_, Tab::Terminal(id))) = self.dock.find_active_focused() {
            self.last_terminal = Some(*id);
        }
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
        tracing::info!("exit: closing the terminals");
        drop(self.terminals.drain());
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
            // tai-gui takes its own quit chord (QUIT).
            cc.egui_ctx.options_mut(|o| o.quit_shortcuts.clear());
            let terminal = fonts::Coverage::new()
                .map_err(Into::into)
                .and_then(|coverage| TerminalPane::new(coverage, cc.egui_ctx.clone(), None))
                .map_err(|e| tracing::error!("the terminal didn't start: {e}"))
                .ok();
            let mut terminals = Terminals::default();
            let first = terminals.add(None, None, terminal);
            Ok(Box::new(App {
                dock: initial_layout(first),
                terminals,
                last_terminal: Some(first),
                terminal_place: None,
                confirm_close: None,
                renaming: None,
                editors: Vec::new(),
                confirm_quit: None,
                quit_confirmed: false,
                #[cfg(target_os = "macos")]
                quit_rerouted: false,
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

/// Select all of a text box's text. A box drawn for the first time has no
/// state yet, and is left as egui makes it.
fn select_all(ctx: &egui::Context, id: egui::Id, text: &str) {
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        let all = egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(text.chars().count()),
        );
        state.cursor.set_char_range(Some(all));
        state.store(ctx, id);
    }
}

#[cfg(test)]
mod tests {
    use super::{Tab, Terminals, initial_layout};

    #[test]
    fn terminals_are_numbered_after_the_first() {
        let mut terminals = Terminals::default();
        let (a, b) = (
            terminals.add(None, None, None),
            terminals.add(None, None, None),
        );
        assert_eq!(Tab::Terminal(a).title(&terminals), "Terminal");
        assert_eq!(Tab::Terminal(b).title(&terminals), "Terminal 2");
    }

    #[test]
    fn layout_has_chat_beside_a_terminal_and_preview_stack() {
        let first = Terminals::default().add(None, None, None);
        let dock = initial_layout(first);
        let stacks: Vec<Vec<Tab>> = dock
            .main_surface()
            .iter()
            .filter_map(|node| node.tabs().map(<[Tab]>::to_vec))
            .collect();
        assert_eq!(
            stacks,
            vec![vec![Tab::Chat], vec![Tab::Terminal(first), Tab::Preview]]
        );
    }
}
