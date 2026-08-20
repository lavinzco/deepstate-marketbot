//! Deepstate market-making bot (Rust)
//!
//! Quotes the best bid + best ask on NVDA/USDG and claims DEEP rewards.
//!
//! Env:
//!   DEEPSTATE_PRIVATE_KEY  wallet private key (0x...)
//!   DEEPSTATE_RPC_URL      optional RPC override
//!   DEEPSTATE_MID_PRICE    reference mid price in USDG per NVDA
//!   DEEPSTATE_SPREAD       half-spread fraction (default 0.005)
//!   DEEPSTATE_INTERVAL     quote refresh interval seconds (default 30)
//!   DEEPSTATE_BID_QTY      bid size in USDG raw units, 6 decimals (default 1e6 = 1 USDG)
//!   DEEPSTATE_ASK_QTY      ask size in NVDA raw units, 18 decimals (default 1e18 = 1 NVDA)
//!   DEEPSTATE_MAX_NVDA     maximum wallet NVDA inventory, raw units (default 2e18 = 2 NVDA)

use crate::contracts::{
    compute_pool_id, sorted_pair, DeepstateRewarder, DeepstateV1, ERC20, REWARDER, ROUTER,
};
use crate::order::{pack, unpack, Order};
use crate::strategy::{apply_balance_limit, compute_quotes, describe_order, MmConfig};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::time::Duration;
use tracing::{info, warn};

pub const DEFAULT_RPC: &str = "https://rpc.mainnet.chain.robinhood.com";
pub const EXPECTED_CHAIN_ID: u64 = 4663;

pub fn state_path() -> String {
    env::var("DEEPSTATE_STATE_FILE").unwrap_or_else(|_| "active_orders.json".to_string())
}

pub fn load_active_orders(path: &str) -> Result<ActiveOrders> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(serde_json::from_str(&contents)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ActiveOrders::default()),
        Err(error) => Err(error.into()),
    }
}

pub fn save_active_orders(path: &str, active: &ActiveOrders) -> Result<()> {
    let temporary = format!("{path}.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(active)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

async fn wait_receipt<P: Provider + Clone + Send + Sync + 'static>(
    provider: &P,
    hash: B256,
) -> Result<alloy::rpc::types::TransactionReceipt> {
    for _ in 0..60 {
        if let Some(receipt) = provider.get_transaction_receipt(hash).await? {
            return Ok(receipt);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(anyhow::anyhow!(
        "receipt timeout for tx {hash}; refusing to resend"
    ))
}

/// A resting order we have live on the book.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveOrder {
    /// Packed resting order returned by the engine (contains the assigned nonce).
    pub packed: B256,
    /// Epoch the order was placed in (needed for bookId + cancel).
    epoch: u64,
    /// Token sold when this order fills (USDG for bid, NVDA for ask) — used for reward claims.
    pub sold_token: Address,
    pub claimant_registered: bool,
    pub cancelled: bool,
    #[serde(default)]
    pub last_tx_hash: Option<B256>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ActiveOrders {
    pub bid: Option<ActiveOrder>,
    pub ask: Option<ActiveOrder>,
}

#[derive(Debug, Clone)]
pub struct CycleView {
    pub epoch: u64,
    pub bid: Option<Order>,
    pub ask: Option<Order>,
    pub usdg_balance: u128,
    pub nvda_balance: u128,
    pub quotes: crate::strategy::Quotes,
}

pub async fn inspect<P: Provider + Clone + Send + Sync + 'static>(
    provider: &P,
    owner: Address,
    cfg: &MmConfig,
) -> Result<CycleView> {
    validate_config(cfg)?;
    let (token0, token1) = sorted_pair();
    let v1 = DeepstateV1::new(ROUTER, provider.clone());
    let epoch = v1
        .poolEpoch(compute_pool_id(token0, token1))
        .call()
        .await?
        .to::<u64>();
    let book_id = v1.bookId(token0, token1, U256::from(epoch)).call().await?;
    let (bid, ask) = fetch_top_of_book(&v1, token0, token1, epoch, book_id).await?;
    let usdg_balance = ERC20::new(token0, provider.clone())
        .balanceOf(owner)
        .call()
        .await?
        .to::<u128>();
    let nvda_balance = ERC20::new(token1, provider.clone())
        .balanceOf(owner)
        .call()
        .await?
        .to::<u128>();
    let mut quotes = compute_quotes(cfg, bid.as_ref(), ask.as_ref())?;
    apply_balance_limit(
        &mut quotes,
        usdg_balance,
        nvda_balance,
        cfg.max_nvda_inventory,
    );
    Ok(CycleView {
        epoch,
        bid,
        ask,
        usdg_balance,
        nvda_balance,
        quotes,
    })
}

pub fn validate_config(cfg: &MmConfig) -> Result<()> {
    ensure!(
        cfg.mid_price.is_finite() && cfg.mid_price > 0.0,
        "mid price must be finite and > 0"
    );
    ensure!(
        cfg.half_spread_pct > 0.0 && cfg.half_spread_pct < 1.0,
        "spread must be in (0, 1)"
    );
    ensure!(cfg.interval_secs >= 5, "interval must be >= 5 seconds");
    ensure!(
        cfg.bid_quantity > 0 && cfg.ask_quantity > 0,
        "quantities must be > 0"
    );
    ensure!(cfg.max_nvda_inventory > 0, "max inventory must be > 0");
    Ok(())
}

pub fn load_config() -> Result<MmConfig> {
    let mut cfg = MmConfig::default();
    if let Ok(v) = env::var("DEEPSTATE_MID_PRICE") {
        cfg.mid_price = v.parse().expect("DEEPSTATE_MID_PRICE must be float");
    }
    if let Ok(v) = env::var("DEEPSTATE_SPREAD") {
        cfg.half_spread_pct = v.parse().expect("DEEPSTATE_SPREAD must be float");
    }
    if let Ok(v) = env::var("DEEPSTATE_INTERVAL") {
        cfg.interval_secs = v.parse().expect("DEEPSTATE_INTERVAL must be u64");
    }
    if let Ok(v) = env::var("DEEPSTATE_BID_QTY") {
        cfg.bid_quantity = v.parse().expect("DEEPSTATE_BID_QTY must be u128");
    }
    if let Ok(v) = env::var("DEEPSTATE_ASK_QTY") {
        cfg.ask_quantity = v.parse().expect("DEEPSTATE_ASK_QTY must be u128");
    }
    if let Ok(v) = env::var("DEEPSTATE_MAX_NVDA") {
        cfg.max_nvda_inventory = v.parse().expect("DEEPSTATE_MAX_NVDA must be u128");
    }

    // A5: validate configuration before the bot starts quoting.
    validate_config(&cfg)?;
    Ok(cfg)
}

pub async fn ensure_allowance<P: Provider + Clone + Send + Sync + 'static>(
    provider: &P,
    owner: Address,
    token: Address,
    amount: U256,
) -> Result<()> {
    let erc20 = ERC20::new(token, provider.clone());
    let current = erc20.allowance(owner, ROUTER).call().await?;
    if current >= amount {
        return Ok(());
    }
    let pending = erc20.approve(ROUTER, U256::MAX).send().await?;
    let hash = pending.tx_hash();
    let receipt = wait_receipt(provider, *hash).await?;
    ensure!(receipt.status(), "approve failed for {token}");
    info!(%token, "approved");
    Ok(())
}

pub async fn run_cycle<P: Provider + Clone + Send + Sync + 'static>(
    provider: &P,
    owner: Address,
    cfg: &MmConfig,
    active: &mut ActiveOrders,
) -> Result<()> {
    let (token0, token1) = sorted_pair();
    let pid = compute_pool_id(token0, token1);
    let v1 = DeepstateV1::new(ROUTER, provider.clone());

    let epoch = v1.poolEpoch(pid).call().await?.to::<u64>();
    // bookId(token0, token1, epoch) is pure; use it for reward claims + cancels.
    let book_id = v1.bookId(token0, token1, U256::from(epoch)).call().await?;
    info!(epoch, ?book_id, "cycle start");

    let (top_bid, top_ask) = fetch_top_of_book(&v1, token0, token1, epoch, book_id).await?;
    let mut quotes = compute_quotes(cfg, top_bid.as_ref(), top_ask.as_ref())?;
    let nvda = ERC20::new(token1, provider.clone());
    let usdg = ERC20::new(token0, provider.clone());
    let usdg_balance = usdg.balanceOf(owner).call().await?.to::<u128>();
    let nvda_balance = nvda.balanceOf(owner).call().await?.to::<u128>();
    apply_balance_limit(
        &mut quotes,
        usdg_balance,
        nvda_balance,
        cfg.max_nvda_inventory,
    );
    info!(
        usdg_balance,
        nvda_balance,
        max_inventory = cfg.max_nvda_inventory,
        "balance risk check"
    );
    info!(
        bid = quotes
            .bid
            .as_ref()
            .map(|order| describe_order(order, cfg.decimals0, cfg.decimals1))
            .unwrap_or_else(|| "disabled".to_string()),
        ask = quotes
            .ask
            .as_ref()
            .map(|order| describe_order(order, cfg.decimals0, cfg.decimals1))
            .unwrap_or_else(|| "disabled".to_string()),
        "computed quotes"
    );

    // Close stale orders and claim rewards.
    // S4: registerClaimant MUST run BEFORE cancel (otherwise rewards are lost forever),
    // and distributeRewards runs AFTER cancel. S6: only drop the active state once the
    // order has been closed successfully — a failed cancel keeps the order for retry.
    if let Some(order) = active.bid.as_mut() {
        close_active_order(provider, token0, token1, order).await?;
        active.bid.take();
    }
    if let Some(order) = active.ask.as_mut() {
        close_active_order(provider, token0, token1, order).await?;
        active.ask.take();
    }

    if let Some(bid) = quotes.bid.as_ref().filter(|order| order.quantity > 0) {
        let bid_packed = pack(bid.tick, bid.quantity, 0)?;
        let bid_resting =
            place_order(provider, owner, token0, token1, epoch, bid_packed, true).await?;
        if bid_resting != B256::ZERO {
            active.bid = Some(ActiveOrder {
                packed: bid_resting,
                epoch,
                sold_token: token0,
                claimant_registered: false,
                cancelled: false,
                last_tx_hash: None,
            }); // bid sells USDG
        }
    }

    if let Some(ask) = quotes.ask.as_ref().filter(|order| order.quantity > 0) {
        let ask_packed = pack(ask.tick, ask.quantity, 0)?;
        let ask_resting =
            place_order(provider, owner, token0, token1, epoch, ask_packed, false).await?;
        if ask_resting != B256::ZERO {
            active.ask = Some(ActiveOrder {
                packed: ask_resting,
                epoch,
                sold_token: token1,
                claimant_registered: false,
                cancelled: false,
                last_tx_hash: None,
            }); // ask sells NVDA
        }
    }
    info!("cycle complete");
    Ok(())
}

/// Reconcile current-epoch orders after startup. Historical epochs cannot be
/// enumerated from the router, so persisted older-epoch orders remain on disk.
pub async fn reconcile_active_orders<P: Provider + Clone + Send + Sync + 'static>(
    provider: &P,
    owner: Address,
    active: &mut ActiveOrders,
) -> Result<()> {
    let (token0, token1) = sorted_pair();
    let engine = DeepstateV1::new(ROUTER, provider.clone());
    let epoch = engine
        .poolEpoch(compute_pool_id(token0, token1))
        .call()
        .await?
        .to::<u64>();
    let book_id = engine
        .bookId(token0, token1, U256::from(epoch))
        .call()
        .await?;
    let roots = engine
        .roots(token0, token1, U256::from(epoch))
        .call()
        .await?;
    let bids = find_owned_orders(&engine, book_id, roots.bidRoot, owner).await?;
    let asks = find_owned_orders(&engine, book_id, roots.askRoot, owner).await?;
    ensure!(
        bids.len() <= 1,
        "multiple owned bid orders found during reconcile"
    );
    ensure!(
        asks.len() <= 1,
        "multiple owned ask orders found during reconcile"
    );
    if active
        .bid
        .as_ref()
        .is_some_and(|order| order.epoch == epoch)
        && active
            .bid
            .as_ref()
            .is_some_and(|order| !bids.contains(&order.packed))
    {
        warn!("persisted bid is absent from the current order tree; dropping stale state");
        active.bid = None;
    }
    if active
        .ask
        .as_ref()
        .is_some_and(|order| order.epoch == epoch)
        && active
            .ask
            .as_ref()
            .is_some_and(|order| !asks.contains(&order.packed))
    {
        warn!("persisted ask is absent from the current order tree; dropping stale state");
        active.ask = None;
    }
    if active.bid.is_none() {
        if let Some(&packed) = bids.first() {
            active.bid = Some(ActiveOrder {
                packed,
                epoch,
                sold_token: token0,
                claimant_registered: false,
                cancelled: false,
                last_tx_hash: None,
            });
        }
    }
    if active.ask.is_none() {
        if let Some(&packed) = asks.first() {
            active.ask = Some(ActiveOrder {
                packed,
                epoch,
                sold_token: token1,
                claimant_registered: false,
                cancelled: false,
                last_tx_hash: None,
            });
        }
    }
    info!(
        epoch,
        discovered_bids = bids.len(),
        discovered_asks = asks.len(),
        "startup reconcile complete"
    );
    Ok(())
}

async fn find_owned_orders<P: Provider + Clone + Send + Sync + 'static>(
    engine: &DeepstateV1::DeepstateV1Instance<P>,
    book_id: B256,
    root: B256,
    owner: Address,
) -> Result<Vec<B256>> {
    let mut stack = vec![root];
    let mut found = Vec::new();
    while let Some(node) = stack.pop() {
        if node == B256::ZERO {
            continue;
        }
        let key = engine.orderId(book_id, node).call().await?;
        if engine.ownerOfOrder(key).call().await? == owner {
            found.push(node);
        }
        let children = engine.tree(book_id, node).call().await?;
        stack.push(children.leftNode);
        stack.push(children.rightNode);
        ensure!(
            stack.len() < 10000,
            "order tree traversal exceeded safety bound"
        );
    }
    Ok(found)
}

async fn place_order<P: Provider + Clone + Send + Sync + 'static>(
    provider: &P,
    owner: Address,
    token0: Address,
    token1: Address,
    epoch: u64,
    order: B256,
    is_bid: bool,
) -> Result<B256> {
    let v1 = DeepstateV1::new(ROUTER, provider.clone());
    let params = DeepstateV1::FillParams {
        token0,
        token1,
        epoch: U256::from(epoch),
        order,
        isBid: is_bid,
        noRest: false,
        fillOrKill: false,
    };
    // S2: simulation is only a candidate; the confirmed transaction is reconciled below.
    let simulated = v1.fill(params.clone()).call().await?;
    let pending = v1.fill(params).send().await?;
    let tx_hash = pending.tx_hash();
    let receipt = wait_receipt(provider, *tx_hash).await?;
    ensure!(receipt.status(), "fill tx failed");
    let side = if is_bid { "bid" } else { "ask" };
    info!(%side, tx=%receipt.transaction_hash, "fill confirmed; reconciling order tree");
    if simulated == B256::ZERO {
        info!(%side, tx=%receipt.transaction_hash, "fill completed without resting order");
        return Ok(B256::ZERO);
    }
    let requested = unpack(&simulated);
    let book_id = v1.bookId(token0, token1, U256::from(epoch)).call().await?;
    let roots = v1.roots(token0, token1, U256::from(epoch)).call().await?;
    let root = if is_bid { roots.bidRoot } else { roots.askRoot };
    let mut stack = vec![root];
    let mut matches = Vec::new();
    while let Some(node) = stack.pop() {
        if node == B256::ZERO {
            continue;
        }
        let candidate = unpack(&node);
        if candidate.tick == requested.tick && candidate.quantity == requested.quantity {
            let key = v1.orderId(book_id, node).call().await?;
            if v1.ownerOfOrder(key).call().await? == owner {
                matches.push(node);
            }
        }
        let children = v1.tree(book_id, node).call().await?;
        stack.push(children.leftNode);
        stack.push(children.rightNode);
        ensure!(
            stack.len() < 10000,
            "order tree traversal exceeded safety bound"
        );
    }
    ensure!(
        matches.len() <= 1,
        "multiple matching owned resting orders found"
    );
    matches
        .pop()
        .ok_or_else(|| anyhow::anyhow!("fill confirmed but no unique owned resting order found"))
}

async fn cancel_order<P: Provider + Clone + Send + Sync + 'static>(
    provider: &P,
    token0: Address,
    token1: Address,
    epoch: u64,
    order: B256,
) -> Result<B256> {
    let v1 = DeepstateV1::new(ROUTER, provider.clone());
    let pending = v1
        .cancel(token0, token1, U256::from(epoch), order)
        .send()
        .await?;
    let tx_hash = pending.tx_hash();
    let receipt = wait_receipt(provider, *tx_hash).await?;
    ensure!(receipt.status(), "cancel tx failed");
    info!(%order, tx=%receipt.transaction_hash, "order cancelled");
    Ok(receipt.transaction_hash)
}

pub async fn fetch_top_of_book<P: Provider + Clone + Send + Sync + 'static>(
    v1: &DeepstateV1::DeepstateV1Instance<P>,
    token0: Address,
    token1: Address,
    epoch: u64,
    book_id: B256,
) -> Result<(Option<Order>, Option<Order>)> {
    let roots = v1.roots(token0, token1, U256::from(epoch)).call().await?;
    let ask_root = roots.askRoot;
    let bid_root = roots.bidRoot;
    let (bid, bid_nonce) = walk_top(v1, book_id, bid_root).await?;
    let (ask, ask_nonce) = walk_top(v1, book_id, ask_root).await?;
    let bid_meta = v1.topOrder(book_id, true).call().await?;
    let ask_meta = v1.topOrder(book_id, false).call().await?;
    ensure!(
        bid.map_or(bid_nonce == 0, |o| o.nonce == bid_meta.nonce),
        "bid top nonce mismatch"
    );
    ensure!(
        ask.map_or(ask_nonce == 0, |o| o.nonce == ask_meta.nonce),
        "ask top nonce mismatch"
    );
    Ok((bid, ask))
}

async fn walk_top<P: Provider + Clone + Send + Sync + 'static>(
    v1: &DeepstateV1::DeepstateV1Instance<P>,
    book_id: B256,
    mut node: B256,
) -> Result<(Option<Order>, u32)> {
    for _ in 0..256 {
        if node == B256::ZERO {
            return Ok((None, 0));
        }
        let children = v1.tree(book_id, node).call().await?;
        let left = children.leftNode;
        let right = children.rightNode;
        if left == B256::ZERO {
            let order = unpack(&node);
            return Ok((Some(order), order.nonce));
        }
        node = if right != B256::ZERO { right } else { left };
    }
    Err(anyhow::anyhow!("book tree exceeded 256 levels"))
}

/// Close a resting order and claim its rewards.
///
/// Order matters (S4): registerClaimant MUST happen while the order is still resting
/// (BEFORE cancel), and distributeRewards AFTER the cancel. Reversing register/cancel
/// loses the rewards permanently.
async fn close_active_order<P: Provider + Clone + Send + Sync + 'static>(
    provider: &P,
    token0: Address,
    token1: Address,
    order: &mut ActiveOrder,
) -> Result<()> {
    let rewarder = DeepstateRewarder::new(REWARDER, provider.clone());
    let engine = DeepstateV1::new(ROUTER, provider.clone());
    let book_id = engine
        .bookId(token0, token1, U256::from(order.epoch))
        .call()
        .await?;

    // 1. Register claimant while the order is still on the book.
    if !order.claimant_registered {
        let pending = rewarder
            .registerClaimant(book_id, order.packed)
            .send()
            .await?;
        let tx_hash = pending.tx_hash();
        order.last_tx_hash = Some(*tx_hash);
        let receipt = wait_receipt(provider, *tx_hash).await?;
        ensure!(receipt.status(), "registerClaimant tx failed");
        order.claimant_registered = true;
        info!(order=%order.packed, tx=%receipt.transaction_hash, "claimant registered");
    }

    // 2. Cancel the order (releases collateral + any fills).
    if !order.cancelled {
        let tx_hash = cancel_order(provider, token0, token1, order.epoch, order.packed).await?;
        order.last_tx_hash = Some(tx_hash);
        order.cancelled = true;
    }

    // 3. Distribute rewards after the order is closed. A failure keeps the active state
    // so the next cycle retries this step without repeating register/cancel.
    let pending = rewarder
        .distributeRewards(book_id, order.packed, order.sold_token)
        .send()
        .await?;
    let tx_hash = pending.tx_hash();
    order.last_tx_hash = Some(*tx_hash);
    let receipt = wait_receipt(provider, *tx_hash).await?;
    ensure!(receipt.status(), "distributeRewards tx failed");
    info!(order=%order.packed, tx=%receipt.transaction_hash, "rewards distributed");
    Ok(())
}
