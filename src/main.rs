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
//!   DEEPSTATE_BID_QTY      bid size in NVDA base units, 18 decimals (default 1e15 = 0.001 NVDA)
//!   DEEPSTATE_ASK_QTY      ask size in NVDA base units, 18 decimals (default 1e18 = 1 NVDA)
//!   DEEPSTATE_MAX_NVDA     maximum wallet NVDA inventory, raw units (default 2e18 = 2 NVDA)

use alloy::primitives::{Address, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use anyhow::{ensure, Result};
use deepstate_mm::contracts::{
    compute_pool_id, sorted_pair, DeepstateRewarder, DeepstateV1, ERC20, REWARDER, ROUTER,
};
use deepstate_mm::order::{pack, unpack, Order};
use deepstate_mm::strategy::{apply_inventory_limit, compute_quotes, describe_order, MmConfig};
use std::env;
use std::time::Duration;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const DEFAULT_RPC: &str = "https://rpc.mainnet.chain.robinhood.com";
const EXPECTED_CHAIN_ID: u64 = 4663;

/// A resting order we have live on the book.
#[derive(Debug, Clone)]
struct ActiveOrder {
    /// Packed resting order returned by the engine (contains the assigned nonce).
    packed: B256,
    /// Epoch the order was placed in (needed for bookId + cancel).
    epoch: u64,
    /// Token sold when this order fills (USDG for bid, NVDA for ask) — used for reward claims.
    sold_token: Address,
    claimant_registered: bool,
    cancelled: bool,
}

#[derive(Debug, Default)]
struct ActiveOrders {
    bid: Option<ActiveOrder>,
    ask: Option<ActiveOrder>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let key = env::var("DEEPSTATE_PRIVATE_KEY").expect("DEEPSTATE_PRIVATE_KEY not set");
    let signer: PrivateKeySigner = key.parse().expect("invalid private key");
    let address = signer.address();
    info!(%address, "starting deepstate market-maker");

    let rpc = env::var("DEEPSTATE_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC.to_string());
    let provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(rpc.parse()?);

    // A1: verify we are talking to Robinhood Chain before touching any funds.
    let chain_id = provider.get_chain_id().await?;
    ensure!(
        chain_id == EXPECTED_CHAIN_ID,
        "unexpected chain id {chain_id}, expected {EXPECTED_CHAIN_ID} (Robinhood Chain)"
    );
    info!(chain_id, "chain id verified");

    let cfg = load_config()?;
    let (token0, token1) = sorted_pair();
    info!(?token0, ?token1, mid = cfg.mid_price, "pair");

    // Approve USDG + NVDA for the router.
    // S3: public RPCs return an empty account list, so use the signer address directly.
    ensure_allowance(
        &provider,
        signer.address(),
        token0,
        U256::from(cfg.bid_quantity),
    )
    .await?;
    ensure_allowance(
        &provider,
        signer.address(),
        token1,
        U256::from(cfg.ask_quantity),
    )
    .await?;
    info!("allowances ready");

    let mut active = ActiveOrders::default();

    loop {
        match run_cycle(&provider, address, &cfg, &mut active).await {
            Ok(()) => {}
            Err(e) => warn!("cycle error: {e:#}"),
        }
        tokio::time::sleep(Duration::from_secs(cfg.interval_secs)).await;
    }
}

fn load_config() -> Result<MmConfig> {
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
    ensure!(
        cfg.mid_price.is_finite() && cfg.mid_price > 0.0,
        "DEEPSTATE_MID_PRICE must be finite and > 0, got {}",
        cfg.mid_price
    );
    ensure!(
        cfg.half_spread_pct > 0.0 && cfg.half_spread_pct < 1.0,
        "DEEPSTATE_SPREAD must be in (0, 1), got {}",
        cfg.half_spread_pct
    );
    ensure!(
        cfg.interval_secs >= 5,
        "DEEPSTATE_INTERVAL must be >= 5 seconds, got {}",
        cfg.interval_secs
    );
    ensure!(
        cfg.bid_quantity > 0 && cfg.ask_quantity > 0,
        "DEEPSTATE_BID_QTY and DEEPSTATE_ASK_QTY must be > 0"
    );
    ensure!(cfg.max_nvda_inventory > 0, "DEEPSTATE_MAX_NVDA must be > 0");
    Ok(cfg)
}

async fn ensure_allowance<P: Provider + Clone + Send + Sync + 'static>(
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
    let receipt = pending.get_receipt().await?;
    ensure!(receipt.status(), "approve failed for {token}");
    info!(%token, "approved");
    Ok(())
}

async fn run_cycle<P: Provider + Clone + Send + Sync + 'static>(
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
    let inventory = nvda.balanceOf(owner).call().await?.to::<u128>();
    apply_inventory_limit(&mut quotes, inventory, cfg.max_nvda_inventory);
    info!(
        inventory,
        max_inventory = cfg.max_nvda_inventory,
        "inventory risk check"
    );
    info!(
        bid = describe_order(&quotes.bid, cfg.decimals0, cfg.decimals1),
        ask = describe_order(&quotes.ask, cfg.decimals0, cfg.decimals1),
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

    // Place fresh bid + ask.
    if quotes.bid.quantity > 0 {
        let bid_packed = pack(quotes.bid.tick, quotes.bid.quantity, 0)?;
        let bid_resting = place_order(provider, token0, token1, epoch, bid_packed, true).await?;
        if bid_resting != B256::ZERO {
            active.bid = Some(ActiveOrder {
                packed: bid_resting,
                epoch,
                sold_token: token0,
                claimant_registered: false,
                cancelled: false,
            }); // bid sells USDG
        }
    }

    if quotes.ask.quantity > 0 {
        let ask_packed = pack(quotes.ask.tick, quotes.ask.quantity, 0)?;
        let ask_resting = place_order(provider, token0, token1, epoch, ask_packed, false).await?;
        if ask_resting != B256::ZERO {
            active.ask = Some(ActiveOrder {
                packed: ask_resting,
                epoch,
                sold_token: token1,
                claimant_registered: false,
                cancelled: false,
            }); // ask sells NVDA
        }
    }

    info!("cycle complete");
    Ok(())
}

async fn place_order<P: Provider + Clone + Send + Sync + 'static>(
    provider: &P,
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
    // S2: simulate first to get the resting order (engine assigns the nonce at fill time).
    // The returned bytes32 is the order we must store as active and cancel later.
    let resting = v1.fill(params.clone()).call().await?;
    let pending = v1.fill(params).send().await?;
    let receipt = pending.get_receipt().await?;
    ensure!(receipt.status(), "fill tx failed");
    let side = if is_bid { "bid" } else { "ask" };
    info!(%side, %resting, tx=%receipt.transaction_hash, "order placed");
    if resting == B256::ZERO {
        info!(%side, tx=%receipt.transaction_hash, "fill completed without resting order");
        return Ok(resting);
    }
    let key = v1
        .orderId(
            v1.bookId(token0, token1, U256::from(epoch)).call().await?,
            resting,
        )
        .call()
        .await?;
    let owner = v1.ownerOfOrder(key).call().await?;
    ensure!(
        owner != Address::ZERO,
        "returned resting order has no on-chain owner"
    );
    Ok(resting)
}

async fn cancel_order<P: Provider + Clone + Send + Sync + 'static>(
    provider: &P,
    token0: Address,
    token1: Address,
    epoch: u64,
    order: B256,
) -> Result<()> {
    let v1 = DeepstateV1::new(ROUTER, provider.clone());
    let pending = v1
        .cancel(token0, token1, U256::from(epoch), order)
        .send()
        .await?;
    let receipt = pending.get_receipt().await?;
    ensure!(receipt.status(), "cancel tx failed");
    info!(%order, tx=%receipt.transaction_hash, "order cancelled");
    Ok(())
}

async fn fetch_top_of_book<P: Provider + Clone + Send + Sync + 'static>(
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
        let receipt = pending.get_receipt().await?;
        ensure!(receipt.status(), "registerClaimant tx failed");
        order.claimant_registered = true;
        info!(order=%order.packed, tx=%receipt.transaction_hash, "claimant registered");
    }

    // 2. Cancel the order (releases collateral + any fills).
    if !order.cancelled {
        cancel_order(provider, token0, token1, order.epoch, order.packed).await?;
        order.cancelled = true;
    }

    // 3. Distribute rewards after the order is closed. A failure keeps the active state
    // so the next cycle retries this step without repeating register/cancel.
    let pending = rewarder
        .distributeRewards(book_id, order.packed, order.sold_token)
        .send()
        .await?;
    let receipt = pending.get_receipt().await?;
    ensure!(receipt.status(), "distributeRewards tx failed");
    info!(order=%order.packed, tx=%receipt.transaction_hash, "rewards distributed");
    Ok(())
}
