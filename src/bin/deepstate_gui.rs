//! Windows-ready GUI for read-only monitoring and explicitly armed market making.
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use deepstate_mm::engine::{self, CycleView};
use deepstate_mm::strategy::{describe_order, MmConfig};
use eframe::egui;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

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
        let c = MmConfig::default();
        Self {
            rpc_url: engine::DEFAULT_RPC.into(),
            private_key: String::new(),
            mid_price: c.mid_price.to_string(),
            spread: c.half_spread_pct.to_string(),
            bid_quantity: c.bid_quantity.to_string(),
            ask_quantity: c.ask_quantity.to_string(),
            max_nvda: c.max_nvda_inventory.to_string(),
            interval: c.interval_secs.to_string(),
            live_enabled: false,
            confirmation: String::new(),
        }
    }
}
impl Form {
    fn config(&self) -> anyhow::Result<MmConfig> {
        let c = MmConfig {
            mid_price: self.mid_price.parse()?,
            half_spread_pct: self.spread.parse()?,
            bid_quantity: self.bid_quantity.parse()?,
            ask_quantity: self.ask_quantity.parse()?,
            max_nvda_inventory: self.max_nvda.parse()?,
            interval_secs: self.interval.parse()?,
            ..Default::default()
        };
        engine::validate_config(&c)?;
        Ok(c)
    }
}
#[derive(Default)]
struct Shared {
    running: bool,
    status: String,
    epoch: String,
    book: String,
    balance: String,
    quotes: String,
    tx: String,
    error: String,
    logs: Vec<String>,
}
#[derive(Default)]
struct Gui {
    form: Form,
    shared: Arc<Mutex<Shared>>,
    stop: Option<Arc<AtomicBool>>,
}

fn push(shared: &Arc<Mutex<Shared>>, msg: impl Into<String>) {
    if let Ok(mut s) = shared.lock() {
        s.logs.push(msg.into());
    }
}
fn set_error(shared: &Arc<Mutex<Shared>>, msg: impl Into<String>) {
    if let Ok(mut s) = shared.lock() {
        s.error = msg.into();
    }
}
fn render_view(shared: &Arc<Mutex<Shared>>, view: &CycleView, cfg: &MmConfig) {
    if let Ok(mut s) = shared.lock() {
        s.epoch = view.epoch.to_string();
        s.book = format!(
            "best bid: {}\nbest ask: {}",
            view.bid
                .as_ref()
                .map(|o| describe_order(o, 6, 18))
                .unwrap_or_else(|| "empty".into()),
            view.ask
                .as_ref()
                .map(|o| describe_order(o, 6, 18))
                .unwrap_or_else(|| "empty".into())
        );
        s.balance = format!(
            "USDG: {}\nNVDA: {}\nmax NVDA: {}",
            view.usdg_balance, view.nvda_balance, cfg.max_nvda_inventory
        );
        s.quotes = format!(
            "bid: {}\nask: {}",
            view.quotes
                .bid
                .as_ref()
                .map(|o| describe_order(o, 6, 18))
                .unwrap_or_else(|| "disabled".into()),
            view.quotes
                .ask
                .as_ref()
                .map(|o| describe_order(o, 6, 18))
                .unwrap_or_else(|| "disabled".into())
        );
    }
}
impl Gui {
    fn start(&mut self) {
        let cfg = match self.form.config() {
            Ok(c) => c,
            Err(e) => {
                set_error(&self.shared, format!("configuration error: {e}"));
                return;
            }
        };
        if self.form.live_enabled
            && (self.form.private_key.trim().is_empty() || self.form.confirmation != "I UNDERSTAND")
        {
            set_error(
                &self.shared,
                "Live trading requires a private key and exact confirmation: I UNDERSTAND",
            );
            return;
        }
        let rpc = self.form.rpc_url.clone();
        let key = self.form.private_key.clone();
        let live = self.form.live_enabled;
        let shared = Arc::clone(&self.shared);
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Some(Arc::clone(&stop));
        if let Ok(mut s) = shared.lock() {
            s.running = true;
            s.status = if live {
                "LIVE ARMED"
            } else {
                "READ-ONLY PREVIEW"
            }
            .into();
            s.error.clear();
            s.logs.clear();
        }
        thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(r) => r,
                Err(e) => {
                    set_error(&shared, e.to_string());
                    return;
                }
            };
            runtime.block_on(async move {
                if live {
                    let signer: PrivateKeySigner = match key.parse() {
                        Ok(k) => k,
                        Err(e) => {
                            set_error(&shared, format!("invalid private key: {e}"));
                            return;
                        }
                    };
                    let owner = signer.address();
                    let provider = match rpc.parse() {
                        Ok(url) => ProviderBuilder::new().wallet(signer).connect_http(url),
                        Err(e) => {
                            set_error(&shared, format!("invalid RPC URL: {e}"));
                            return;
                        }
                    };
                    let result =
                        cycle_loop(provider, owner, cfg, Arc::clone(&shared), stop, true).await;
                    if let Err(e) = result {
                        set_error(&shared, format!("engine stopped: {e:#}"));
                    }
                } else {
                    let provider = match rpc.parse() {
                        Ok(url) => ProviderBuilder::new().connect_http(url),
                        Err(e) => {
                            set_error(&shared, format!("invalid RPC URL: {e}"));
                            return;
                        }
                    };
                    let result = cycle_loop(
                        provider,
                        Address::ZERO,
                        cfg,
                        Arc::clone(&shared),
                        stop,
                        false,
                    )
                    .await;
                    if let Err(e) = result {
                        set_error(&shared, format!("read-only error: {e:#}"));
                    }
                }
            });
        });
    }
    fn stop(&mut self) {
        if let Some(flag) = self.stop.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Ok(mut s) = self.shared.lock() {
            s.status = "STOPPING (receipt waits finish)".into();
        }
    }
}
async fn cycle_loop<P: Provider + Clone + Send + Sync + 'static>(
    provider: P,
    owner: Address,
    cfg: MmConfig,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    live: bool,
) -> anyhow::Result<()> {
    let chain = provider.get_chain_id().await?;
    anyhow::ensure!(
        chain == engine::EXPECTED_CHAIN_ID,
        "unexpected chain id {chain}, expected {}",
        engine::EXPECTED_CHAIN_ID
    );
    push(&shared, format!("RPC chain id verified: {chain}"));
    let state_path = engine::state_path();
    let mut active = engine::load_active_orders(&state_path)?;
    if live {
        engine::reconcile_active_orders(&provider, owner, &mut active).await?;
        engine::save_active_orders(&state_path, &active)?;
    }
    loop {
        let view = engine::inspect(&provider, owner, &cfg).await?;
        render_view(&shared, &view, &cfg);
        push(
            &shared,
            format!(
                "epoch {}: real order book and balances refreshed",
                view.epoch
            ),
        );
        if live {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            engine::run_cycle(&provider, owner, &cfg, &mut active).await?;
            engine::save_active_orders(&state_path, &active)?;
            push(&shared, "live cycle confirmed and state saved");
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(cfg.interval_secs)).await;
    }
    if let Ok(mut s) = shared.lock() {
        s.running = false;
        s.status = "STOPPED".into();
    }
    Ok(())
}
impl eframe::App for Gui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(250));
        let (running, status, epoch, book, balance, quotes, tx, error, logs) = self
            .shared
            .lock()
            .map(|s| {
                (
                    s.running,
                    s.status.clone(),
                    s.epoch.clone(),
                    s.book.clone(),
                    s.balance.clone(),
                    s.quotes.clone(),
                    s.tx.clone(),
                    s.error.clone(),
                    s.logs.clone(),
                )
            })
            .unwrap_or_default();
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Deepstate NVDA/USDG market maker");
            egui::Grid::new("config").num_columns(2).show(ui, |ui| {
                for (label, value) in [
                    ("RPC URL", &mut self.form.rpc_url),
                    ("Mid price", &mut self.form.mid_price),
                    ("Half spread", &mut self.form.spread),
                    ("Bid quantity (USDG raw)", &mut self.form.bid_quantity),
                    ("Ask quantity (NVDA raw)", &mut self.form.ask_quantity),
                    ("Max NVDA (raw)", &mut self.form.max_nvda),
                    ("Interval seconds", &mut self.form.interval),
                ] {
                    ui.label(label);
                    ui.text_edit_singleline(value);
                    ui.end_row();
                }
                ui.label("Private key (never logged)");
                ui.add(egui::TextEdit::singleline(&mut self.form.private_key).password(true));
                ui.end_row();
            });
            ui.checkbox(
                &mut self.form.live_enabled,
                "Enable LIVE trading (default OFF)",
            );
            if self.form.live_enabled {
                ui.colored_label(
                    egui::Color32::RED,
                    "REAL TRANSACTIONS: type I UNDERSTAND to arm",
                );
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
            ui.label(format!("Status: {status} | epoch: {epoch}"));
            ui.columns(4, |c| {
                c[0].heading("Balances");
                c[0].label(balance);
                c[1].heading("Real book");
                c[1].label(book);
                c[2].heading("Target quotes");
                c[2].label(quotes);
                c[3].heading("Last tx hash");
                c[3].label(tx);
            });
            ui.colored_label(egui::Color32::RED, error);
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .show(ui, |ui| {
                    for l in logs {
                        ui.label(l);
                    }
                });
        });
    }
}
fn main() -> eframe::Result {
    eframe::run_native(
        "Deepstate Market Maker",
        eframe::NativeOptions::default(),
        Box::new(|_| Ok(Box::new(Gui::default()))),
    )
}
