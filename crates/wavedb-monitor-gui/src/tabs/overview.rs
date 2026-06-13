//! Overview tab: stat cards, cluster topology graph, IOps charts.

use egui_charts::{
    Axis, Chart, ChartWidget, GraphLayout, GraphLink, GraphNode, GraphSeries,
    Series,
};
use egui_components::{Card, card_header, typography};

use crate::app::MonitorApp;
use crate::state::{NodeHealth, NodeKind, NodeState};

use super::{chart_theme, human_bytes, stat_card};

pub fn show(app: &MonitorApp, ui: &mut egui::Ui) {
    let (nodes, write_history, read_history) = {
        let s = app.shared.lock().expect("shared state");
        (
            s.nodes.clone(),
            s.write_history.iter().copied().collect::<Vec<u64>>(),
            s.read_history.iter().copied().collect::<Vec<u64>>(),
        )
    };

    show_stat_cards(ui, &nodes);
    ui.add_space(12.0);

    // Topology graph and IOps charts side by side when there's room. The
    // charts stretch to the remaining window height (clamped so small
    // windows still scroll instead of crushing the plots).
    let chart_h = (ui.available_height() - 110.0).clamp(300.0, 720.0);
    let wide = ui.available_width() > 900.0;
    if wide {
        ui.columns(2, |cols| {
            show_topology(&mut cols[0], &nodes, chart_h);
            show_iops(&mut cols[1], &write_history, &read_history, chart_h);
        });
    } else {
        show_topology(ui, &nodes, chart_h);
        ui.add_space(12.0);
        show_iops(ui, &write_history, &read_history, chart_h);
    }
}

fn show_stat_cards(ui: &mut egui::Ui, nodes: &[NodeState]) {
    let total_writes: u64 = nodes
        .iter()
        .filter_map(|n| n.quick.as_ref())
        .map(|m| m.write_count)
        .sum();
    let total_reads: u64 = nodes
        .iter()
        .filter_map(|n| n.quick.as_ref())
        .map(|m| m.read_count)
        .sum();
    let rejected: u64 = nodes
        .iter()
        .filter_map(|n| n.quick.as_ref())
        .map(|m| m.rejected_count)
        .sum();
    let history_records: usize = nodes
        .iter()
        .filter_map(|n| n.slow.as_ref())
        .map(|m| m.record_count)
        .sum();
    let up = nodes.iter().filter(|n| n.health == NodeHealth::Ok).count();
    let disk: u64 = nodes
        .iter()
        .filter_map(|n| n.quick.as_ref())
        .map(|m| m.journal_bytes + m.heap_bytes + m.data_file_bytes)
        .sum();

    ui.horizontal_wrapped(|ui| {
        stat_card(ui, &format!("{up}/{}", nodes.len()), "nodes up");
        stat_card(ui, &total_writes.to_string(), "writes");
        stat_card(ui, &total_reads.to_string(), "reads");
        stat_card(ui, &rejected.to_string(), "rejected writes");
        stat_card(ui, &history_records.to_string(), "history records");
        stat_card(ui, &human_bytes(disk), "hot-tier disk");
    });
}

/// Cluster topology: Quick-Nodes in the gossip mesh, Slow-Nodes hanging off
/// every Quick-Node (history flush path).  Node size tracks write (quick)
/// or record (slow) volume.
#[allow(clippy::cast_precision_loss)]
fn show_topology(ui: &mut egui::Ui, nodes: &[NodeState], chart_h: f32) {
    Card::new().padding(12.0).show(ui, |ui| {
        card_header(
            ui,
            "Cluster topology",
            Some("gossip mesh between Quick-Nodes, flush edges to Slow-Nodes"),
        );

        let mut graph_nodes = Vec::new();
        let mut links = Vec::new();
        let quick_idx: Vec<usize> = nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.kind == NodeKind::Quick)
            .map(|(i, _)| i)
            .collect();
        let slow_idx: Vec<usize> = nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.kind == NodeKind::Slow)
            .map(|(i, _)| i)
            .collect();

        // Graph node per cluster node; value drives the bubble size.
        let mut graph_pos = vec![0usize; nodes.len()];
        for (gi, &i) in quick_idx.iter().chain(slow_idx.iter()).enumerate() {
            let n = &nodes[i];
            let (label, value) = match n.kind {
                NodeKind::Quick => {
                    let writes = n.quick.as_ref().map_or(0, |m| m.write_count);
                    (
                        format!("Q {} [{}]", n.short_addr(), n.health.label()),
                        (writes as f64).max(1.0),
                    )
                }
                NodeKind::Slow => {
                    let recs = n.slow.as_ref().map_or(0, |m| m.record_count);
                    (
                        format!("S {} [{}]", n.short_addr(), n.health.label()),
                        (recs as f64).max(1.0),
                    )
                }
            };
            graph_pos[i] = gi;
            // Color by tier (quick vs slow), not by node index — the tiers
            // are the meaningful clusters in a topology view.
            let group = usize::from(n.kind == NodeKind::Slow);
            graph_nodes.push(GraphNode::new(label, value).group(group));
        }

        // Gossip mesh: every Quick pair.
        for (a, &i) in quick_idx.iter().enumerate() {
            for &j in quick_idx.iter().skip(a + 1) {
                links.push(GraphLink::new(graph_pos[i], graph_pos[j], 1.0));
            }
        }
        // Flush edges: every Quick → every Slow.
        for &i in &quick_idx {
            for &j in &slow_idx {
                links.push(GraphLink::new(graph_pos[i], graph_pos[j], 2.0));
            }
        }

        let chart = Chart::new().series(Series::Graph(
            GraphSeries::new("cluster")
                .nodes(graph_nodes)
                .links(links)
                .layout(GraphLayout::Circular),
        ));
        // push_id: chart-internal widgets (legend chips) key off `ui.id()`,
        // so sibling charts collide without a scope per chart.
        ui.push_id("topology_chart", |ui| {
            ChartWidget::new(&chart)
                .theme(chart_theme(ui.ctx()))
                .min_size(egui::vec2(360.0, chart_h))
                .show(ui);
        });

        typography::muted_text(
            ui,
            "bubble size: writes (quick) / records (slow)",
        );
    });
}

fn show_iops(ui: &mut egui::Ui, writes: &[u64], reads: &[u64], chart_h: f32) {
    Card::new().padding(12.0).show(ui, |ui| {
        card_header(ui, "Throughput", Some("cluster-wide ops per poll tick"));

        #[allow(clippy::cast_precision_loss)]
        let to_f64 =
            |xs: &[u64]| xs.iter().map(|&v| v as f64).collect::<Vec<f64>>();

        // One category per sample: labels feed the hover tooltip, but the
        // grid and tick text stay hidden — 120 gridlines is a picket fence.
        // y pinned to 0: ops counts can't go negative, and the auto-range
        // pads an idle (all-zero) history out to [-1, 1] otherwise.
        let labels = (0..writes.len()).map(|i| i.to_string());
        let chart = Chart::new()
            .x_axis(super::silent_category_axis(labels))
            .y_axis(Axis::value().min(0.0))
            .series(
                Series::line("writes/tick")
                    .data(to_f64(writes))
                    .smooth(true)
                    .area()
                    .hide_symbols(),
            )
            .series(
                Series::line("reads/tick")
                    .data(to_f64(reads))
                    .smooth(true)
                    .area()
                    .hide_symbols(),
            );
        ui.push_id("iops_chart", |ui| {
            ChartWidget::new(&chart)
                .theme(chart_theme(ui.ctx()))
                .min_size(egui::vec2(360.0, chart_h))
                .show(ui);
        });
    });
}
