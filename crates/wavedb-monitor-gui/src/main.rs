//! WaveDB GUI monitor — desktop dashboard for WaveDB clusters.
//!
//! Same polling protocol as the TUI monitor (`wavedb-monitor`): POST
//! `/metrics` to every node with an optional HMAC Monitor token, plus the
//! Slow-Node `/browse` + `/history` endpoints for the data explorer.
//!
//! ```text
//! wavedb-monitor-gui \
//!   --quick-nodes http://127.0.0.1:7700,http://127.0.0.1:7701 \
//!   --slow-nodes  http://127.0.0.1:7800 \
//!   --cluster-key <64-hex>            # optional, also settable in-app
//! ```

use std::sync::{Arc, Mutex, mpsc};

use clap::Parser;

use wavedb_monitor_gui::app::MonitorApp;
use wavedb_monitor_gui::config::{Args, Config};
use wavedb_monitor_gui::poll;
use wavedb_monitor_gui::state::Shared;

fn main() -> eframe::Result<()> {
    let args = Args::parse();
    let cfg = Config::from_args(&args);

    let shared = Arc::new(Mutex::new(Shared::new(
        &cfg.quick_node_urls,
        &cfg.slow_node_urls,
    )));
    let (tx, rx) = mpsc::channel();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("WaveDB Monitor")
            .with_inner_size([1280.0, 860.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };

    let key = cfg.cluster_key.clone();
    let key_preloaded = key.is_some();
    let refresh_ms = cfg.refresh_ms;
    let start_tab = args.tab.index();

    eframe::run_native(
        "wavedb-monitor-gui",
        native_options,
        Box::new(move |cc| {
            // The worker needs the egui context to request repaints, so it
            // spawns once the window exists.
            poll::spawn_worker(
                shared.clone(),
                rx,
                key,
                refresh_ms,
                cc.egui_ctx.clone(),
            );
            Ok(Box::new(MonitorApp::new(
                cc,
                shared,
                tx,
                refresh_ms,
                key_preloaded,
                start_tab,
            )))
        }),
    )
}
