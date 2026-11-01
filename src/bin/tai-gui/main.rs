//! tai-gui: the egui prototype of ADR 0011.
//!
//! One eframe window on wgpu with two tab stacks (egui_dock): chat beside a
//! terminal and preview stack. The terminal is a libghostty-vt pane on
//! tai's pty (step 2); chat and preview are placeholders for later steps. Build and run with
//! `scripts/cargo.sh run --features gui --bin tai-gui`.
//!
//! wgpu's validation: egui-wgpu creates the instance with
//! `InstanceFlags::from_build_config().with_env()` (egui-wgpu 0.36.2,
//! `src/setup.rs`), so `VALIDATION_INDIRECT_CALL` stays on and
//! `WGPU_VALIDATION_INDIRECT_CALL=0` turns it off, as ADR 0011 decision 3
//! asks. Debug builds also turn on full validation, so measure in release.

use eframe::egui;
use egui_dock::{DockArea, DockState, NodeIndex, Style, TabViewer};

// Modules shared with the macroquad build. Each is compiled into both
// binaries; tai-gui uses only part of some.
#[path = "../../cells.rs"]
#[allow(dead_code)]
mod cells;
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
#[path = "../../shortcuts.rs"]
#[allow(dead_code)]
mod shortcuts;

mod gpu;
mod terminal;

// The pty modules size the pseudo terminal from `crate::Dimensions`.
use terminal::{Dimensions, TerminalPane};

/// What a tab shows. Each kind gets its own pane in a later step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Tab {
    Chat,
    Terminal,
    Preview,
}

impl Tab {
    fn title(self) -> &'static str {
        match self {
            Tab::Chat => "Chat",
            Tab::Terminal => "Terminal",
            Tab::Preview => "Preview",
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
}

impl TabViewer for Panes<'_> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        egui::Id::new(*tab)
    }

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        tab.title().into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        match (tab, self.terminal.as_deref_mut()) {
            (Tab::Terminal, Some(terminal)) => terminal.ui(ui),
            (Tab::Terminal, None) => {
                ui.label("The terminal didn't start; see the log.");
            }
            (tab, _) => {
                ui.label(format!("{} pane: not built yet.", tab.title()));
            }
        }
    }

    // The terminal draws to its own edges.
    fn clear_background(&self, tab: &Tab) -> bool {
        *tab != Tab::Terminal
    }

    fn scroll_bars(&self, tab: &Tab) -> [bool; 2] {
        if *tab == Tab::Terminal {
            [false, false]
        } else {
            [true, true]
        }
    }

    fn is_closeable(&self, _tab: &Tab) -> bool {
        false
    }
}

struct App {
    dock: DockState<Tab>,
    terminal: Option<TerminalPane>,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Read the shell's output even while its tab is hidden.
        if let Some(terminal) = &mut self.terminal {
            terminal.pump();
            // The pty has no waker into egui yet, so poll it; step 5
            // measures what that costs.
            if !terminal.exited() {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(8));
            }
        }
        let style = Style::from_egui(ui.style().as_ref());
        DockArea::new(&mut self.dock)
            .style(style)
            .show_leaf_close_all_buttons(false)
            .show_inside(
                ui,
                &mut Panes {
                    terminal: self.terminal.as_mut(),
                },
            );
    }
}

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
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
    }
    eframe::run_native(
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
            let terminal = fonts::Coverage::new()
                .map_err(Into::into)
                .and_then(|coverage| TerminalPane::new(coverage, cc.egui_ctx.pixels_per_point()))
                .map_err(|e| tracing::error!("the terminal didn't start: {e}"))
                .ok();
            Ok(Box::new(App {
                dock: initial_layout(),
                terminal,
            }))
        }),
    )
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
