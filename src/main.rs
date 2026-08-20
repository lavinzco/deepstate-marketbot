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
//!   DEEPSTATE_BID_QTY      bid size in USDG raw units (default 1e6 = 1 USDG)
//!   DEEPSTATE_ASK_QTY      ask size in NVDA raw units (default 1e18 = 1 NVDA)

use alloy::primitives::{Address, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use anyhow::{ensure, Result};
use deepstate_mm::contracts::{compute_pool_id, sorted_pair, DeepstateRewarder, DeepstateV1, ERC20, REWARDER, ROUTER};
use deepstate_mm::order::pack;
use deepstate_mm::strategy::{compute_quotes, describe_order, MmConfig};
use std::env;
use std::time::Duration;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const DEFAULT_RPC: &str = "https://rpc.mainnet.chain.robinhood.com";

#[derive(Debug, Default)]
struct ActiveOrders {
    bid: Option<(B256, u64)>, // (packed resting order, epoch)
    ask: Option<(B256, u64)>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let key = env::var("DEEPSTATE_PRIVATE_KEY").expect("DEEPSTATE_PRIVATE_KEY not set");
    let signer: PrivateKeySigner = key.parse().expect("invalid private key");
    let address = signer.address();
    info!(%address, "starting deepstate market-maker");

    let rpc = env::var("DEEPSTATE_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC.to_string());
    let provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(rpc.parse()?);

    let cfg = load_config();
    let (token0, token1) = sorted_pair();
    info!(?token0, ?token1, mid = cfg.mid_price, "pair");

    // Approve USDG + NVDA for the router.
    ensure_allowance(&provider, token0, U256::from(cfg.bid_quantity)).await?;
    ensure_allowance(&provider, token1, U256::from(cfg.ask_quantity)).await?;
    info!("allowances ready");

    let mut active = ActiveOrders::default();

    loop {
        match run_cycle(&provider, &cfg, &mut active).await {
            Ok(()) => {}
            Err(e) => warn!("cycle error: {e:#}"),
        }
        tokio::time::sleep(Duration::from_secs(cfg.interval_secs)).await;
    }
}

fn load_config() -> MmConfig {
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
    cfg
}

async fn ensure_allowance<P: Provider + Clone + Send + Sync + 'static>(
    provider: &P,
    token: Address,
    amount: U256,
) -> Result<()> {
    let erc20 = ERC20::new(token, provider.clone());
    let accounts = provider.get_accounts().await?;
    let owner = accounts.first().copied().ok_or_else(|| anyhow::anyhow!("no accounts"))?;
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
    cfg: &MmConfig,
    active: &mut ActiveOrders,
) -> Result<()> {
    let (token0, token1) = sorted_pair();
    let pid = compute_pool_id(token0, token1);
    let v1 = DeepstateV1::new(ROUTER, provider.clone());

    let epoch = v1.poolEpoch(pid).call().await?.to::<u64>();
    let book_id = v1.activeBookId(token0, token1).call().await?;
    info!(epoch, ?book_id, "cycle start");

    // Desired quotes (based on external reference price).
    let quotes = compute_quotes(cfg, None, None)?;
    info!(
        bid = describe_order(&quotes.bid, cfg.decimals0, cfg.decimals1),
        ask = describe_order(&quotes.ask, cfg.decimals0, cfg.decimals1),
        "computed quotes"
    );

    // Cancel stale orders.
    if let Some((packed, old_epoch)) = active.bid.take() {
        cancel_order(provider, token0, token1, old_epoch, packed).await?;
    }
    if let Some((packed, old_epoch)) = active.ask.take() {
        cancel_order(provider, token0, token1, old_epoch, packed).await?;
    }

    // Place fresh bid + ask.
    let bid_packed = pack(quotes.bid.tick, quotes.bid.quantity, 0)?;
    let ask_packed = pack(quotes.ask.tick, quotes.ask.quantity, 0)?;

    let bid_resting = place_order(provider, token0, token1, epoch, bid_packed, true).await?;
    active.bid = Some((bid_resting, epoch));

    let ask_resting = place_order(provider, token0, token1, epoch, ask_packed, false).await?;
    active.ask = Some((ask_resting, epoch));

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
    let pending = v1.fill(params).send().await?;
    let receipt = pending.get_receipt().await?;
    ensure!(receipt.status(), "fill tx failed");
    let side = if is_bid { "bid" } else { "ask" };
    info!(%side, %order, tx=%receipt.transaction_hash, "order placed");
    Ok(order)
}

async fn cancel_order<P: Provider + Clone + Send + Sync + 'static>(
    provider: &P,
    token0: Address,
    token1: Address,
    epoch: u64,
    order: B256,
) -> Result<()> {
    let v1 = DeepstateV1::new(ROUTER, provider.clone());
    let pending = v1.cancel(token0, token1, U256::from(epoch), order).send().await?;
    let receipt = pending.get_receipt().await?;
    ensure!(receipt.status(), "cancel tx failed");
    info!(%order, tx=%receipt.transaction_hash, "order cancelled");
    Ok(())
}

/// Claim DEEP rewards for a closed order.
/// Sequence: registerClaimant -> (order already cancelled) -> distributeRewards.
pub async fn claim_rewards<P: Provider + Clone + Send + Sync + 'static>(
    provider: &P,
    book_id: B256,
    order: B256,
    sold_token: Address,
) -> Result<()> {
    let rewarder = DeepstateRewarder::new(REWARDER, provider.clone());
    let pending = rewarder.registerClaimant(book_id, order).send().await?;
    pending.get_receipt().await?;
    let pending = rewarder.distributeRewards(book_id, order, sold_token).send().await?;
    let receipt = pending.get_receipt().await?;
    ensure!(receipt.status(), "distribute tx failed");
    info!(%order, tx=%receipt.transaction_hash, "rewards distributed");
    Ok(())
}
