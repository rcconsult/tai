//! tai-gui: the egui prototype of ADR 0011, step 1, the shell.
//!
//! One eframe window on wgpu with two tab stacks (egui_dock). The tabs are
//! placeholders: later steps put a libghostty-vt terminal, a preview and a
//! chat pane in them. Build and run with
//! `scripts/cargo.sh run --features gui --bin tai-gui`.
//!
//! wgpu's validation: egui-wgpu creates the instance with
//! `InstanceFlags::from_build_config().with_env()` (egui-wgpu 0.36.2,
//! `src/setup.rs`), so `VALIDATION_INDIRECT_CALL` stays on and
//! `WGPU_VALIDATION_INDIRECT_CALL=0` turns it off, as ADR 0011 decision 3
//! asks. Debug builds also turn on full validation, so measure in release.

use eframe::egui;
use egui_dock::{DockArea, DockState, NodeIndex, Style, TabViewer};

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

struct Panes;

impl TabViewer for Panes {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        egui::Id::new(*tab)
    }

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        tab.title().into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        ui.label(format!("{} pane: not built yet.", tab.title()));
    }

    fn is_closeable(&self, _tab: &Tab) -> bool {
        false
    }
}

struct App {
    dock: DockState<Tab>,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let style = Style::from_egui(ui.style().as_ref());
        DockArea::new(&mut self.dock)
            .style(style)
            .show_leaf_close_all_buttons(false)
            .show_inside(ui, &mut Panes);
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
            Ok(Box::new(App {
                dock: initial_layout(),
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
