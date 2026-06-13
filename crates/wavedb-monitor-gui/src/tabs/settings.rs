//! Settings tab: cluster-key authorization and poll cadence.

use egui_components::{
    Button, ButtonVariant, Card, Input, card_header, typography,
};
use wavedb_net::auth::ClusterKey;

use crate::app::MonitorApp;
use crate::state::Command;

use super::health_badge;

pub fn show(app: &mut MonitorApp, ui: &mut egui::Ui) {
    show_auth(app, ui);
    ui.add_space(12.0);
    show_poll(app, ui);
    ui.add_space(12.0);
    show_node_health(app, ui);
}

fn show_auth(app: &mut MonitorApp, ui: &mut egui::Ui) {
    Card::new().padding(12.0).show(ui, |ui| {
        card_header(
            ui,
            "Cluster authorization",
            Some(
                "nodes verify each monitor request with an HMAC-SHA256 \
                 token (purpose: Monitor, ±30 s replay window) minted from \
                 the shared cluster key",
            ),
        );

        ui.horizontal(|ui| {
            Input::new(&mut app.key_input)
                .label("cluster key")
                .placeholder("64 hex characters")
                .password(true)
                .width(420.0)
                .show(ui);

            if Button::new("Apply")
                .variant(ButtonVariant::Default)
                .show(ui)
                .clicked()
            {
                let hex = app.key_input.trim();
                match ClusterKey::from_hex(hex) {
                    Ok(key) => {
                        let _ = app
                            .commands
                            .send(Command::SetClusterKey(Some(key)));
                        app.key_error = None;
                        app.key_applied = true;
                    }
                    Err(e) => {
                        app.key_error = Some(e.to_string());
                        app.key_applied = false;
                    }
                }
            }

            if Button::new("Clear")
                .variant(ButtonVariant::Outline)
                .show(ui)
                .clicked()
            {
                app.key_input.clear();
                app.key_error = None;
                app.key_applied = false;
                let _ = app.commands.send(Command::SetClusterKey(None));
            }
        });

        if let Some(err) = &app.key_error {
            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err);
        } else if app.key_applied {
            typography::muted_text(
                ui,
                "key active — Monitor tokens are minted per poll; \
                 the key itself never leaves this process",
            );
        } else {
            typography::muted_text(
                ui,
                "no key — works only against open/dev clusters; \
                 nodes with a key answer 403",
            );
        }
    });
}

fn show_poll(app: &mut MonitorApp, ui: &mut egui::Ui) {
    Card::new().padding(12.0).show(ui, |ui| {
        card_header(ui, "Polling", Some("metrics refresh cadence"));
        let mut ms = app.refresh_ms;
        ui.horizontal(|ui| {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            {
                let mut ms_f = ms as f64;
                ui.add(
                    egui::Slider::new(&mut ms_f, 100.0..=5000.0)
                        .step_by(50.0)
                        .suffix(" ms"),
                );
                ms = ms_f as u64;
            }
        });
        if ms != app.refresh_ms {
            app.refresh_ms = ms;
            let _ = app.commands.send(Command::SetRefreshMs(ms));
        }
    });
}

fn show_node_health(app: &MonitorApp, ui: &mut egui::Ui) {
    Card::new().padding(12.0).show(ui, |ui| {
        card_header(
            ui,
            "Node authorization status",
            Some("AUTH = node rejected the token (HTTP 403)"),
        );
        let nodes = {
            let s = app.shared.lock().expect("shared state");
            s.nodes.clone()
        };
        for n in &nodes {
            ui.horizontal(|ui| {
                health_badge(ui, n.health);
                ui.label(n.short_addr());
            });
        }
    });
}
