//! Read-only probe: connect to Robinhood Chain and inspect the NVDA/USDG book state.
//!
//! Usage: DEEPSTATE_RPC_URL=<rpc> cargo run --bin probe

use alloy::providers::ProviderBuilder;
use anyhow::Result;
use deepstate_mm::contracts::{compute_pool_id, sorted_pair, DeepstateV1, ROUTER};

#[tokio::main]
async fn main() -> Result<()> {
    let rpc = std::env::var("DEEPSTATE_RPC_URL")
        .unwrap_or_else(|_| "https://rpc.mainnet.chain.robinhood.com".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc.parse()?);

    let (token0, token1) = sorted_pair();
    let pid = compute_pool_id(token0, token1);
    let v1 = DeepstateV1::new(ROUTER, &provider);

    println!("poolId:      {pid:#x}");
    match v1.poolEpoch(pid).call().await {
        Ok(epoch) => {
            println!("poolEpoch:   {epoch}");
            let book = v1.activeBookId(token0, token1).call().await?;
            println!("activeBook:  {book:#x}");
            let top_bid = v1.topOrder(book, true).call().await?;
            let top_ask = v1.topOrder(book, false).call().await?;
            println!(
                "topBid:      nonce={} sold={}",
                top_bid.nonce, top_bid.soldAmount
            );
            println!(
                "topAsk:      nonce={} sold={}",
                top_ask.nonce, top_ask.soldAmount
            );
        }
        Err(e) => println!("poolEpoch failed: {e:#} (pool may not be initialized)"),
    }
    Ok(())
}
