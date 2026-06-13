//! Nodes tab: cluster table + per-node detail with page-map heatmap.

use egui_charts::{Chart, ChartWidget, GaugeSeries, HeatmapSeries, Series};
use egui_components::{
    Card, card_header,
    table::{Table, TableColumn},
    typography,
};

use crate::app::MonitorApp;
use crate::state::{NodeKind, NodeState};

use super::{chart_theme, health_badge, human_bytes};

pub fn show(app: &mut MonitorApp, ui: &mut egui::Ui) {
    let nodes = {
        let s = app.shared.lock().expect("shared state");
        s.nodes.clone()
    };

    show_table(app, ui, &nodes);
    ui.add_space(12.0);

    if let Some(node) = nodes.get(app.selected_node) {
        match node.kind {
            NodeKind::Quick => show_quick_detail(app, ui, node),
            NodeKind::Slow => show_slow_detail(ui, node),
        }
    } else {
        typography::muted_text(ui, "no node selected");
    }
}

fn show_table(app: &mut MonitorApp, ui: &mut egui::Ui, nodes: &[NodeState]) {
    let columns = [
        TableColumn {
            header: "",
            width: Some(56.0),
        },
        TableColumn {
            header: "Type",
            width: Some(64.0),
        },
        TableColumn {
            header: "Address",
            width: None,
        },
        TableColumn {
            header: "Status",
            width: Some(72.0),
        },
        TableColumn {
            header: "Ring",
            width: Some(56.0),
        },
        TableColumn {
            header: "Own",
            width: Some(56.0),
        },
        TableColumn {
            header: "Writes",
            width: Some(84.0),
        },
        TableColumn {
            header: "Reads",
            width: Some(84.0),
        },
        TableColumn {
            header: "Records",
            width: Some(84.0),
        },
        TableColumn {
            header: "Mem",
            width: Some(84.0),
        },
        TableColumn {
            header: "Disk",
            width: Some(84.0),
        },
        TableColumn {
            header: "Uptime",
            width: Some(72.0),
        },
    ];

    let mut clicked: Option<usize> = None;
    Table::new(&columns).show(ui, nodes.len(), |row_idx, row| {
        let n = &nodes[row_idx];
        let selected = row_idx == app.selected_node;

        row.cell(|ui| {
            if ui
                .selectable_label(selected, format!("#{row_idx}"))
                .clicked()
            {
                clicked = Some(row_idx);
            }
        });
        row.cell(|ui| {
            ui.label(match n.kind {
                NodeKind::Quick => "Quick",
                NodeKind::Slow => "Slow",
            });
        });
        row.cell(|ui| {
            ui.label(n.short_addr());
        });
        row.cell(|ui| health_badge(ui, n.health));

        // Remaining 8 cells: Ring, Own, Writes, Reads, Records, Mem,
        // Disk, Uptime — quick and slow nodes fill different columns.
        let cells = metric_cells(n);
        for text in cells {
            row.cell(|ui| {
                ui.label(text);
            });
        }
    });

    if let Some(i) = clicked {
        app.selected_node = i;
    }
}

/// Per-tier text for the eight metric columns of the nodes table.
fn metric_cells(n: &NodeState) -> [String; 8] {
    const DASH: &str = "—";
    match (&n.quick, &n.slow) {
        (Some(m), _) => [
            m.ring_size.to_string(),
            m.owned_partitions.to_string(),
            m.write_count.to_string(),
            m.read_count.to_string(),
            DASH.to_string(),
            human_bytes(m.estimated_memory_bytes),
            if m.has_storage {
                human_bytes(m.journal_bytes + m.heap_bytes + m.data_file_bytes)
            } else {
                "RAM".to_string()
            },
            format!("{}s", m.uptime_secs),
        ],
        (_, Some(m)) => [
            DASH.to_string(),
            DASH.to_string(),
            DASH.to_string(),
            DASH.to_string(),
            m.record_count.to_string(),
            human_bytes(m.index_bytes),
            human_bytes(m.journal_bytes),
            format!("{}s", m.uptime_secs),
        ],
        _ => std::array::from_fn(|_| DASH.to_string()),
    }
}

fn show_quick_detail(app: &MonitorApp, ui: &mut egui::Ui, node: &NodeState) {
    let Some(m) = &node.quick else {
        Card::new().padding(12.0).show(ui, |ui| {
            card_header(ui, node.short_addr(), Some("no metrics yet"));
        });
        return;
    };

    let wide = ui.available_width() > 900.0;
    if wide {
        ui.columns(2, |cols| {
            show_quick_storage(app, &mut cols[0], node, m);
            show_page_map(app, &mut cols[1], m);
        });
    } else {
        show_quick_storage(app, ui, node, m);
        ui.add_space(12.0);
        show_page_map(app, ui, m);
    }
}

fn show_quick_storage(
    app: &MonitorApp,
    ui: &mut egui::Ui,
    node: &NodeState,
    m: &wavedb_net::metrics::QuickNodeMetrics,
) {
    Card::new().padding(12.0).show(ui, |ui| {
        card_header(
            ui,
            &format!("Quick-Node {}", node.short_addr()),
            Some(if m.is_draining {
                "DRAINING — withdrawing from the ring"
            } else {
                "storage & replication"
            }),
        );
        let kv = |ui: &mut egui::Ui, k: &str, v: String| {
            ui.horizontal(|ui| {
                typography::muted_text(ui, k);
                ui.label(v);
            });
        };
        kv(ui, "node id", format!("{:016x}", m.node_id));
        kv(ui, "ring size", m.ring_size.to_string());
        kv(ui, "owned partitions", m.owned_partitions.to_string());
        kv(ui, "rejected writes", m.rejected_count.to_string());
        kv(ui, "replicated records", m.replicated_count.to_string());
        kv(ui, "write bytes", human_bytes(m.write_bytes));
        kv(ui, "est. memory", human_bytes(m.estimated_memory_bytes));
        ui.add_space(6.0);
        if m.has_storage {
            kv(ui, "journal.log (WAL)", human_bytes(m.journal_bytes));
            kv(ui, "data.bin (pages)", human_bytes(m.data_file_bytes));
            kv(ui, "heap.bin (blobs)", human_bytes(m.heap_bytes));
        } else {
            typography::muted_text(ui, "in-memory mode — no disk files");
        }

        // data.bin fill ratio as a gauge.
        if m.data_file_page_count > 0 {
            #[allow(clippy::cast_precision_loss)]
            let pct = m.data_file_used_pages as f64
                / m.data_file_page_count as f64
                * 100.0;
            let chart = Chart::new().series(Series::Gauge(
                GaugeSeries::new("data.bin pages used %")
                    .value(pct)
                    .range(0.0, 100.0),
            ));
            ui.push_id(("gauge", app.selected_node), |ui| {
                ChartWidget::new(&chart)
                    .theme(chart_theme(ui.ctx()))
                    .min_size(egui::vec2(260.0, 200.0))
                    .show(ui);
            });
        }
    });
}

/// Page-occupancy heat map mirroring the TUI's block-character page map.
const PAGE_MAP_COLS: usize = 32;

fn show_page_map(
    app: &MonitorApp,
    ui: &mut egui::Ui,
    m: &wavedb_net::metrics::QuickNodeMetrics,
) {
    Card::new().padding(12.0).show(ui, |ui| {
        card_header(
            ui,
            "Page map",
            Some(
                "traffic per page slot, relative to the busiest slot \
                 (dark = untouched, sqrt color scale)",
            ),
        );
        if m.page_map.is_empty() {
            typography::muted_text(ui, "no data yet");
            return;
        }
        let data: Vec<(usize, usize, f64)> = m
            .page_map
            .iter()
            .enumerate()
            .map(|(i, &occ)| {
                (i % PAGE_MAP_COLS, i / PAGE_MAP_COLS, f64::from(occ))
            })
            .collect();
        let rows = m.page_map.len().div_ceil(PAGE_MAP_COLS);
        let chart = Chart::new()
            .x_axis(super::silent_category_axis(
                (0..PAGE_MAP_COLS).map(|i| i.to_string()),
            ))
            .y_axis(super::silent_category_axis(
                (0..rows).map(|i| i.to_string()),
            ))
            .series(Series::Heatmap(
                HeatmapSeries::new("occupancy")
                    .data(data)
                    .range(0.0, 255.0)
                    .sqrt_scale(),
            ));
        ui.push_id(("page_map", app.selected_node), |ui| {
            ChartWidget::new(&chart)
                .theme(chart_theme(ui.ctx()))
                .min_size(egui::vec2(360.0, 280.0))
                .show(ui);
        });
    });
}

fn show_slow_detail(ui: &mut egui::Ui, node: &NodeState) {
    Card::new().padding(12.0).show(ui, |ui| {
        card_header(
            ui,
            &format!("Slow-Node {}", node.short_addr()),
            Some("immutable history archive"),
        );
        let Some(m) = &node.slow else {
            typography::muted_text(ui, "no metrics yet");
            return;
        };
        let kv = |ui: &mut egui::Ui, k: &str, v: String| {
            ui.horizontal(|ui| {
                typography::muted_text(ui, k);
                ui.label(v);
            });
        };
        kv(ui, "records", m.record_count.to_string());
        kv(ui, "tenants", m.tenant_count.to_string());
        kv(ui, "flush batches", m.flush_count.to_string());
        kv(ui, "audit journal", human_bytes(m.journal_bytes));
        kv(ui, "in-memory index", human_bytes(m.index_bytes));
        kv(ui, "uptime", format!("{}s", m.uptime_secs));
    });
}
