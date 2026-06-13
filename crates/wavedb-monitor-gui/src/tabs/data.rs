//! Data tab: browse tenants and records held by a Slow-Node, draw the
//! tenant's records as a force graph clustered by struct family, and
//! inspect a single record's payload.

use egui_charts::{
    Chart, ChartWidget, GraphLayout, GraphLink, GraphNode, GraphSeries, Series,
};
use egui_components::{
    Button, ButtonVariant, Card, card_header,
    table::{Table, TableColumn},
    typography,
};
use wavedb_core::Id;
use wavedb_net::browse::RecordSummary;

use crate::app::MonitorApp;
use crate::state::{Command, DataView, NodeKind};

use super::{chart_theme, human_bytes};

/// Cap on record nodes drawn in the graph — beyond this the chart melts.
const GRAPH_RECORD_CAP: usize = 150;

pub fn show(app: &mut MonitorApp, ui: &mut egui::Ui) {
    let (slow_urls, data) = {
        let s = app.shared.lock().expect("shared state");
        let urls: Vec<String> = s
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Slow)
            .map(|n| n.short_addr().to_string())
            .collect();
        (urls, s.data.clone())
    };

    if slow_urls.is_empty() {
        typography::muted_text(
            ui,
            "no Slow-Nodes configured — data browsing needs at least one",
        );
        return;
    }

    // First visit: fetch the tenant list without waiting for a click.
    if !app.auto_browse_fired && data.generation == 0 {
        app.auto_browse_fired = true;
        let _ = app.commands.send(Command::Browse {
            slow_idx: app.selected_slow,
            tenant: None,
        });
    }
    // First tenant list: graph the first tenant right away.
    if !app.auto_tenant_fired
        && data.generation > 0
        && data.selected_tenant.is_none()
    {
        app.auto_tenant_fired = true;
        if let Some(first) = data.tenants.first() {
            let _ = app.commands.send(Command::Browse {
                slow_idx: app.selected_slow,
                tenant: Some(first.tenant),
            });
        }
    }

    show_toolbar(app, ui, &slow_urls, &data);
    ui.add_space(12.0);

    let wide = ui.available_width() > 900.0;
    if wide {
        ui.columns(2, |cols| {
            show_tenants(app, &mut cols[0], &data);
            show_record_graph(&mut cols[1], &data);
        });
    } else {
        show_tenants(app, ui, &data);
        ui.add_space(12.0);
        show_record_graph(ui, &data);
    }

    ui.add_space(12.0);
    show_records_table(app, ui, &data);
    ui.add_space(12.0);
    show_record_detail(ui, &data);
}

fn show_toolbar(
    app: &mut MonitorApp,
    ui: &mut egui::Ui,
    slow_urls: &[String],
    data: &DataView,
) {
    ui.horizontal(|ui| {
        typography::muted_text(ui, "Slow-Node:");
        for (i, url) in slow_urls.iter().enumerate() {
            if ui
                .selectable_label(app.selected_slow == i, url.as_str())
                .clicked()
            {
                app.selected_slow = i;
            }
        }
        if Button::new("Browse")
            .variant(ButtonVariant::Default)
            .show(ui)
            .clicked()
        {
            let _ = app.commands.send(Command::Browse {
                slow_idx: app.selected_slow,
                tenant: data.selected_tenant,
            });
        }
        if data.truncated {
            typography::muted_text(ui, "(record list truncated at cap)");
        }
        if let Some(err) = &data.last_error {
            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err);
        }
    });
}

fn show_tenants(app: &MonitorApp, ui: &mut egui::Ui, data: &DataView) {
    Card::new().padding(12.0).show(ui, |ui| {
        card_header(
            ui,
            "Tenants",
            Some("every tenant with history on this node"),
        );
        if data.tenants.is_empty() {
            typography::muted_text(ui, "nothing browsed yet — press Browse");
            return;
        }

        let columns = [
            TableColumn {
                header: "Tenant",
                width: Some(120.0),
            },
            TableColumn {
                header: "Records",
                width: Some(90.0),
            },
            TableColumn {
                header: "Bytes",
                width: None,
            },
        ];
        let mut clicked: Option<u64> = None;
        Table::new(&columns).show(ui, data.tenants.len(), |i, row| {
            let t = &data.tenants[i];
            let selected = data.selected_tenant == Some(t.tenant);
            row.cell(|ui| {
                if ui
                    .selectable_label(selected, t.tenant.to_string())
                    .clicked()
                {
                    clicked = Some(t.tenant);
                }
            });
            row.cell(|ui| {
                ui.label(t.record_count.to_string());
            });
            row.cell(|ui| {
                ui.label(human_bytes(t.total_bytes));
            });
        });

        if let Some(tenant) = clicked {
            let _ = app.commands.send(Command::Browse {
                slow_idx: app.selected_slow,
                tenant: Some(tenant),
            });
        }
    });
}

/// Force graph: one hub per struct family, records linked to their hub.
/// Bubble size tracks payload bytes — visualizes the tenant's data shape
/// the way it's actually clustered (per-tenant, per-struct), not as tables.
fn show_record_graph(ui: &mut egui::Ui, data: &DataView) {
    Card::new().padding(12.0).show(ui, |ui| {
        let title = data.selected_tenant.map_or_else(
            || "Record graph".to_string(),
            |t| format!("Record graph — tenant {t}"),
        );
        card_header(
            ui,
            &title,
            Some("hub = struct family, bubble = record (size: bytes)"),
        );

        if data.records.is_empty() {
            typography::muted_text(ui, "select a tenant to graph its records");
            return;
        }

        let mut nodes: Vec<GraphNode> = Vec::new();
        let mut links: Vec<GraphLink> = Vec::new();
        // struct family (header >> 8) → hub node index
        let mut hubs: std::collections::BTreeMap<u32, usize> =
            std::collections::BTreeMap::new();

        let shown = data.records.iter().take(GRAPH_RECORD_CAP);
        for rec in shown {
            let family = rec.header >> 8;
            let version = rec.header & 0xFF;
            let hub = *hubs.entry(family).or_insert_with(|| {
                let idx = nodes.len();
                let label = if family == 0 {
                    "struct ?".to_string()
                } else {
                    format!("struct {family}")
                };
                // Hub size fixed: prominence without drowning records.
                nodes.push(GraphNode::new(label, 0.0));
                idx
            });

            let idx = nodes.len();
            let id = Id::from_raw(rec.id);
            let label = format!(
                "{}·v{} @{}",
                short_hex(rec.id),
                version,
                id.created_at()
            );
            nodes.push(GraphNode::new(label, f64::from(rec.data_bytes)));
            links.push(GraphLink::new(hub, idx, 1.0));
        }

        // Hubs sized to their family's total bytes so big families read as
        // bigger clusters.
        let mut family_bytes: std::collections::BTreeMap<u32, f64> =
            std::collections::BTreeMap::new();
        for rec in data.records.iter().take(GRAPH_RECORD_CAP) {
            *family_bytes.entry(rec.header >> 8).or_insert(0.0) +=
                f64::from(rec.data_bytes);
        }
        for (family, &hub_idx) in &hubs {
            if let Some(n) = nodes.get_mut(hub_idx) {
                n.value =
                    family_bytes.get(family).copied().unwrap_or(1.0).max(1.0);
            }
        }

        let chart = Chart::new().series(Series::Graph(
            GraphSeries::new("records")
                .nodes(nodes)
                .links(links)
                .layout(GraphLayout::Force),
        ));
        ui.push_id("record_graph", |ui| {
            ChartWidget::new(&chart)
                .theme(chart_theme(ui.ctx()))
                .min_size(egui::vec2(380.0, 340.0))
                .show(ui);
        });

        if data.records.len() > GRAPH_RECORD_CAP {
            typography::muted_text(
                ui,
                &format!(
                    "showing first {GRAPH_RECORD_CAP} of {} records",
                    data.records.len()
                ),
            );
        }
    });
}

fn show_records_table(
    app: &mut MonitorApp,
    ui: &mut egui::Ui,
    data: &DataView,
) {
    if data.records.is_empty() {
        return;
    }
    Card::new().padding(12.0).show(ui, |ui| {
        card_header(
            ui,
            "Records",
            Some("click an ID to fetch and inspect its payload"),
        );
        let columns = [
            TableColumn {
                header: "ID",
                width: Some(180.0),
            },
            TableColumn {
                header: "Struct",
                width: Some(90.0),
            },
            TableColumn {
                header: "Ver",
                width: Some(50.0),
            },
            TableColumn {
                header: "Shard",
                width: Some(60.0),
            },
            TableColumn {
                header: "Created",
                width: None,
            },
            TableColumn {
                header: "Bytes",
                width: Some(90.0),
            },
            TableColumn {
                header: "Flush seq",
                width: Some(90.0),
            },
        ];

        let mut clicked: Option<&RecordSummary> = None;
        Table::new(&columns).show(ui, data.records.len(), |i, row| {
            let rec = &data.records[i];
            let id = Id::from_raw(rec.id);
            let selected = app.selected_record == Some(rec.id);
            row.cell(|ui| {
                if ui.selectable_label(selected, short_hex(rec.id)).clicked() {
                    clicked = Some(rec);
                }
            });
            row.cell(|ui| {
                let family = rec.header >> 8;
                ui.label(if family == 0 {
                    "?".to_string()
                } else {
                    family.to_string()
                });
            });
            row.cell(|ui| {
                ui.label((rec.header & 0xFF).to_string());
            });
            row.cell(|ui| {
                ui.label(id.shard_id().to_string());
            });
            row.cell(|ui| {
                ui.label(id.created_at().to_string());
            });
            row.cell(|ui| {
                ui.label(human_bytes(u64::from(rec.data_bytes)));
            });
            row.cell(|ui| {
                ui.label(rec.write_seq.to_string());
            });
        });

        if let Some(rec) = clicked {
            app.selected_record = Some(rec.id);
            if let Some(tenant) = data.selected_tenant {
                let _ = app.commands.send(Command::FetchRecord {
                    slow_idx: app.selected_slow,
                    tenant,
                    id: rec.id,
                });
            }
        }
    });
}

fn show_record_detail(ui: &mut egui::Ui, data: &DataView) {
    let Some(detail) = &data.detail else { return };

    Card::new().padding(12.0).show(ui, |ui| {
        let id = Id::from_raw(detail.id);
        card_header(
            ui,
            &format!("Record {}", short_hex(detail.id)),
            Some("payload fetched via /history"),
        );

        let kv = |ui: &mut egui::Ui, k: &str, v: String| {
            ui.horizontal(|ui| {
                typography::muted_text(ui, k);
                ui.label(egui::RichText::new(v).monospace());
            });
        };
        kv(ui, "raw id", format!("{:032x}", detail.id));
        kv(ui, "tenant", id.tenant_id().to_string());
        kv(ui, "shard", id.shard_id().to_string());
        kv(ui, "struct", id.struct_id().to_string());
        kv(ui, "created_at (100µs ticks)", id.created_at().to_string());

        if let Some(rec) = &detail.decoded {
            // The slow node packs flush metadata into the chain-link slots
            // (old = tenant, new = write_seq) — show them as what they are.
            #[allow(clippy::cast_possible_truncation)]
            kv(
                ui,
                "flushed by tenant",
                (rec.old_modification_id as u64).to_string(),
            );
            #[allow(clippy::cast_possible_truncation)]
            kv(
                ui,
                "flush write_seq",
                (rec.new_modification_id as u64).to_string(),
            );
            kv(ui, "payload bytes", rec.data.len().to_string());
            ui.add_space(6.0);
            typography::muted_text(ui, "payload hex dump (first 512 bytes):");
            hex_dump(ui, &rec.data);
        } else if detail.raw.is_empty() {
            typography::muted_text(
                ui,
                "record not found on this node (empty /history reply)",
            );
        } else {
            typography::muted_text(
                ui,
                "payload did not decode as a VersionedRecord — raw bytes:",
            );
            hex_dump(ui, &detail.raw);
        }
    });
}

fn hex_dump(ui: &mut egui::Ui, bytes: &[u8]) {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (row, chunk) in bytes.chunks(16).take(32).enumerate() {
        let _ = write!(out, "{:04x}  ", row * 16);
        for b in chunk {
            let _ = write!(out, "{b:02x} ");
        }
        for _ in chunk.len()..16 {
            out.push_str("   ");
        }
        out.push(' ');
        for &b in chunk {
            out.push(if (0x20..0x7F).contains(&b) {
                b as char
            } else {
                '·'
            });
        }
        out.push('\n');
    }
    if bytes.len() > 512 {
        let _ = writeln!(out, "… {} more bytes", bytes.len() - 512);
    }
    ui.label(egui::RichText::new(out).monospace().size(12.0));
}

/// Last 8 hex digits — enough to tell records apart in labels.
fn short_hex(id: u128) -> String {
    format!("…{:08x}", id & 0xFFFF_FFFF)
}
