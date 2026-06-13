//! The egui application: tab chrome, theming, and per-tab rendering.

use std::sync::mpsc::Sender;

use egui_components::{
    Badge, BadgeVariant, ShadcnTheme,
    tabs::Tabs,
    typography::{heading2, muted_text},
};

use crate::state::{Command, NodeHealth, SharedHandle};
use crate::tabs;

/// Top-level GUI state owned by the egui thread.
#[allow(clippy::struct_excessive_bools)] // independent UI toggles, not a state machine
pub struct MonitorApp {
    pub shared: SharedHandle,
    pub commands: Sender<Command>,
    pub dark: bool,
    tab: usize,
    /// Settings tab scratch: hex key field + parse error + applied marker.
    pub key_input: String,
    pub key_error: Option<String>,
    pub key_applied: bool,
    pub refresh_ms: u64,
    /// Nodes tab: selected row.
    pub selected_node: usize,
    /// Data tab scratch.
    pub selected_slow: usize,
    pub selected_record: Option<u128>,
    /// One-shot: the Data tab auto-browses the first time it's shown.
    pub auto_browse_fired: bool,
    /// One-shot: the first browse response auto-selects the first tenant.
    pub auto_tenant_fired: bool,
}

impl MonitorApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        shared: SharedHandle,
        commands: Sender<Command>,
        refresh_ms: u64,
        key_preloaded: bool,
        start_tab: usize,
    ) -> Self {
        egui_components::register_font(&cc.egui_ctx);
        let app = Self {
            shared,
            commands,
            dark: true,
            tab: start_tab,
            key_input: String::new(),
            key_error: None,
            key_applied: key_preloaded,
            refresh_ms,
            selected_node: 0,
            selected_slow: 0,
            selected_record: None,
            auto_browse_fired: false,
            auto_tenant_fired: false,
        };
        app.apply_theme(&cc.egui_ctx);
        app
    }

    fn apply_theme(&self, ctx: &egui::Context) {
        let theme = ShadcnTheme::build(self.dark, None);
        ShadcnTheme::set(ctx, theme.clone());
        theme.apply(ctx);
    }

    /// Render the whole monitor into `ui`.  Split from [`eframe::App::ui`]
    /// so tests can drive it headlessly.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        let theme = ShadcnTheme::get(ui.ctx());
        egui::Frame::new()
            .fill(theme.background)
            .inner_margin(16.0)
            .show(ui, |ui| {
                self.show_header(ui);
                ui.add_space(8.0);

                let labels = ["Overview", "Nodes", "Data", "Settings"];
                let mut tab = self.tab;
                Tabs::new("monitor_tabs", &labels, &mut tab).show(
                    ui,
                    |ui, active| {
                        ui.add_space(8.0);
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| match active {
                                0 => tabs::overview::show(self, ui),
                                1 => tabs::nodes::show(self, ui),
                                2 => tabs::data::show(self, ui),
                                _ => tabs::settings::show(self, ui),
                            });
                    },
                );
                self.tab = tab;
            });
    }

    fn show_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            heading2(ui, "WaveDB Monitor");
            ui.add_space(12.0);

            let (auth, all_ok, any_unauthorized) = {
                let s = self.shared.lock().expect("shared state");
                (
                    s.auth_enabled,
                    s.nodes.iter().all(|n| n.health == NodeHealth::Ok),
                    s.nodes
                        .iter()
                        .any(|n| n.health == NodeHealth::Unauthorized),
                )
            };

            if auth {
                Badge::new("HMAC auth")
                    .variant(BadgeVariant::Default)
                    .show(ui);
            } else {
                Badge::new("open / dev mode")
                    .variant(BadgeVariant::Outline)
                    .show(ui);
            }
            if any_unauthorized {
                Badge::new("403 — key rejected")
                    .variant(BadgeVariant::Destructive)
                    .show(ui);
            } else if all_ok {
                Badge::new("cluster reachable")
                    .variant(BadgeVariant::Secondary)
                    .show(ui);
            }

            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    let label = if self.dark { "Light" } else { "Dark" };
                    if egui_components::Button::new(label)
                        .variant(egui_components::ButtonVariant::Ghost)
                        .show(ui)
                        .clicked()
                    {
                        self.dark = !self.dark;
                        self.apply_theme(ui.ctx());
                    }
                    muted_text(ui, &format!("poll {} ms", self.refresh_ms));
                },
            );
        });
    }
}

impl eframe::App for MonitorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}
