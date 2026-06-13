//! One module per monitor tab.

pub mod data;
pub mod nodes;
pub mod overview;
pub mod settings;

use egui_components::{Card, typography};

/// A small stat card: big value over a muted caption.
pub fn stat_card(ui: &mut egui::Ui, value: &str, caption: &str) {
    Card::new().padding(12.0).show(ui, |ui| {
        ui.set_min_width(130.0);
        typography::heading3(ui, value);
        typography::muted_text(ui, caption);
    });
}

/// Human-readable byte count (matches the TUI monitor's format).
#[allow(clippy::cast_precision_loss)]
pub fn human_bytes(b: u64) -> String {
    if b >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", b as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024 * 1024 {
        format!("{:.1} MB", b as f64 / (1024.0 * 1024.0))
    } else if b >= 1024 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}

/// Health → badge color mapping shared by the Nodes and Settings tabs.
pub fn health_badge(ui: &mut egui::Ui, health: crate::state::NodeHealth) {
    use crate::state::NodeHealth;
    use egui_components::{Badge, BadgeVariant};
    let variant = match health {
        NodeHealth::Ok => BadgeVariant::Secondary,
        NodeHealth::Unknown => BadgeVariant::Outline,
        NodeHealth::Unauthorized
        | NodeHealth::Unreachable
        | NodeHealth::BadResponse => BadgeVariant::Destructive,
    };
    Badge::new(health.label()).variant(variant).show(ui);
}

/// Category axis with grid and tick text hidden — for dense sample axes
/// (IOps history, page map) where labels only matter in the hover tooltip.
pub fn silent_category_axis(
    labels: impl IntoIterator<Item = String>,
) -> egui_charts::Axis {
    let mut axis = egui_charts::Axis::category(labels).hide_grid();
    axis.show_tick_labels = false;
    axis
}

/// Chart theme that follows the app's light/dark mode.
///
/// Seeded from a fixed saturated blue, not `ShadcnTheme::primary` — shadcn's
/// dark-mode primary is near-white, which collapses the whole generated
/// palette into indistinguishable grays.
pub fn chart_theme(ctx: &egui::Context) -> egui_charts::ChartTheme {
    egui_charts::ChartTheme::follow_egui(
        ctx,
        egui::Color32::from_rgb(0x4F, 0x8C, 0xFF),
        egui_charts::theme::Harmony::Square,
        6,
    )
}
