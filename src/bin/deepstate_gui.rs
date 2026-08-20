//! Cross-platform egui front end for the market-maker configuration.
//!
//! This MVP deliberately never submits a transaction. The live-trading switch
//! is opt-in and requires an explicit confirmation, but remains a guarded
//! placeholder until the execution engine is wired to a user-controlled task.

use alloy::providers::{Provider, ProviderBuilder};
use deepstate_mm::strategy::{compute_quotes, describe_order, MmConfig};
use eframe::egui;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const DEFAULT_RPC: &str = "https://rpc.mainnet.chain.robinhood.com";
const EXPECTED_CHAIN_ID: u64 = 4663;

#[derive(Clone)]
struct Form {
    rpc_url: String,
    private_key: String,
    mid_price: String,
    spread: String,
    bid_quantity: String,
    ask_quantity: String,
    max_nvda: String,
    interval: String,
    live_enabled: bool,
    confirmation: String,
}

impl Default for Form {
    fn default() -> Self {
        let cfg = MmConfig::default();
        Self {
            rpc_url: DEFAULT_RPC.into(),
            private_key: String::new(),
            mid_price: cfg.mid_price.to_string(),
            spread: cfg.half_spread_pct.to_string(),
            bid_quantity: cfg.bid_quantity.to_string(),
            ask_quantity: cfg.ask_quantity.to_string(),
            max_nvda: cfg.max_nvda_inventory.to_string(),
            interval: cfg.interval_secs.to_string(),
            live_enabled: false,
            confirmation: String::new(),
        }
    }
}

impl Form {
    fn config(&self) -> anyhow::Result<MmConfig> {
        let cfg = MmConfig {
            mid_price: self.mid_price.parse()?,
            half_spread_pct: self.spread.parse()?,
            bid_quantity: self.bid_quantity.parse()?,
            ask_quantity: self.ask_quantity.parse()?,
            max_nvda_inventory: self.max_nvda.parse()?,
            interval_secs: self.interval.parse()?,
            ..MmConfig::default()
        };
        anyhow::ensure!(
            cfg.interval_secs >= 5,
            "interval must be at least 5 seconds"
        );
        anyhow::ensure!(
            cfg.bid_quantity > 0 && cfg.ask_quantity > 0,
            "quantities must be > 0"
        );
        anyhow::ensure!(cfg.max_nvda_inventory > 0, "max NVDA must be > 0");
        anyhow::ensure!(
            cfg.mid_price.is_finite() && cfg.mid_price > 0.0,
            "mid price must be finite and > 0"
        );
        anyhow::ensure!(
            cfg.half_spread_pct.is_finite()
                && cfg.half_spread_pct > 0.0
                && cfg.half_spread_pct < 1.0,
            "spread must be in (0, 1)"
        );
        Ok(cfg)
    }
}

#[derive(Default)]
struct Shared {
    running: bool,
    status: String,
    logs: Vec<String>,
    balance: String,
    book: String,
    error: String,
}

#[derive(Default)]
struct Gui {
    form: Form,
    shared: Arc<Mutex<Shared>>,
    stop: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Gui {
    fn start(&mut self) {
        let cfg = match self.form.config() {
            Ok(cfg) => cfg,
            Err(error) => {
                self.log(format!("configuration error: {error}"));
                return;
            }
        };
        if self.form.live_enabled && self.form.confirmation != "I UNDERSTAND" {
            self.log("Live trading requires typing I UNDERSTAND".into());
            return;
        }
        let shared = Arc::clone(&self.shared);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.stop = Some(Arc::clone(&stop));
        let form = self.form.clone();
        {
            let mut state = shared.lock().unwrap();
            state.running = true;
            state.status = if form.live_enabled {
                "GUARDED LIVE MODE"
            } else {
                "PREVIEW MODE"
            }
            .into();
            state.error.clear();
            state
                .logs
                .push("started; no transaction will be submitted by this MVP".into());
        }
        thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(error) => {
                    shared.lock().unwrap().error = error.to_string();
                    return;
                }
            };
            runtime.block_on(async move {
                let provider = ProviderBuilder::new().connect_http(form.rpc_url.parse().unwrap());
                match provider.get_chain_id().await {
                    Ok(chain_id) => {
                        let mut state = shared.lock().unwrap();
                        state.logs.push(format!("RPC chain id: {chain_id}"));
                        if chain_id != EXPECTED_CHAIN_ID {
                            state.error = format!(
                                "unexpected chain id {chain_id}, expected {EXPECTED_CHAIN_ID}"
                            );
                        }
                    }
                    Err(error) => shared.lock().unwrap().error = format!("RPC error: {error}"),
                }
                if let Ok(quotes) = compute_quotes(&cfg, None, None) {
                    let mut state = shared.lock().unwrap();
                    state.book = format!(
                        "bid: {}\nask: {}",
                        quotes
                            .bid
                            .as_ref()
                            .map(|o| describe_order(o, 6, 18))
                            .unwrap_or_else(|| "disabled".into()),
                        quotes
                            .ask
                            .as_ref()
                            .map(|o| describe_order(o, 6, 18))
                            .unwrap_or_else(|| "disabled".into())
                    );
                    state
                        .logs
                        .push("local strategy quotes computed from MmConfig".into());
                }
                // A short polling loop keeps Start/Stop responsive without placing orders.
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_secs(cfg.interval_secs.min(5))).await;
                }
                shared.lock().unwrap().running = false;
            });
        });
    }

    fn log(&self, message: String) {
        self.shared.lock().unwrap().logs.push(message);
    }

    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.shared.lock().unwrap().status = "STOPPING".into();
    }
}

impl eframe::App for Gui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(250));
        let state = self.shared.lock().unwrap();
        let running = state.running;
        let status = state.status.clone();
        let balance = state.balance.clone();
        let book = state.book.clone();
        let error = state.error.clone();
        let logs = state.logs.clone();
        drop(state);
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Deepstate NVDA/USDG market maker");
            ui.label("Windows-ready MVP: preview and read-only RPC checks only.");
            egui::Grid::new("config").num_columns(2).show(ui, |ui| {
                ui.label("RPC URL");
                ui.text_edit_singleline(&mut self.form.rpc_url);
                ui.end_row();
                ui.label("Private key (optional, never logged)");
                ui.add(egui::TextEdit::singleline(&mut self.form.private_key).password(true));
                ui.end_row();
                ui.label("Mid price");
                ui.text_edit_singleline(&mut self.form.mid_price);
                ui.end_row();
                ui.label("Half spread");
                ui.text_edit_singleline(&mut self.form.spread);
                ui.end_row();
                ui.label("Bid quantity (USDG raw)");
                ui.text_edit_singleline(&mut self.form.bid_quantity);
                ui.end_row();
                ui.label("Ask quantity (NVDA raw)");
                ui.text_edit_singleline(&mut self.form.ask_quantity);
                ui.end_row();
                ui.label("Max NVDA (raw)");
                ui.text_edit_singleline(&mut self.form.max_nvda);
                ui.end_row();
                ui.label("Interval (seconds)");
                ui.text_edit_singleline(&mut self.form.interval);
                ui.end_row();
            });
            ui.separator();
            ui.checkbox(
                &mut self.form.live_enabled,
                "Live trading enabled (default OFF)",
            );
            if self.form.live_enabled {
                ui.colored_label(
                    egui::Color32::RED,
                    "WARNING: live mode is experimental and this MVP still sends no transactions.",
                );
                ui.label("Type I UNDERSTAND to unlock Start");
                ui.text_edit_singleline(&mut self.form.confirmation);
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!running, egui::Button::new("Start"))
                    .clicked()
                {
                    self.start();
                }
                if ui.add_enabled(running, egui::Button::new("Stop")).clicked() {
                    self.stop();
                }
            });
            ui.separator();
            ui.label(format!("Status: {}", status));
            ui.columns(3, |columns| {
                columns[0].heading("Balances");
                columns[0].label(&balance);
                columns[1].heading("Book / quotes");
                columns[1].label(&book);
                columns[2].heading("Errors");
                columns[2].colored_label(egui::Color32::RED, &error);
            });
            ui.heading("Log");
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .show(ui, |ui| {
                    for line in &logs {
                        ui.label(line);
                    }
                });
        });
    }
}

fn main() -> eframe::Result {
    eframe::run_native(
        "Deepstate Market Maker",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(Gui::default()))),
    )
}
