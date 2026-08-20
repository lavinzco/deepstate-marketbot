//! Read-only probe for the Deepstate NVDA/USDG order book.

use alloy::primitives::U256;
use alloy::providers::ProviderBuilder;
use anyhow::Result;
use deepstate_mm::contracts::{compute_pool_id, sorted_pair, DeepstateV1, ROUTER};
use deepstate_mm::engine::fetch_top_of_book;
use deepstate_mm::strategy::describe_order;

#[tokio::main]
async fn main() -> Result<()> {
    let rpc = std::env::var("DEEPSTATE_RPC_URL")
        .unwrap_or_else(|_| "https://rpc.mainnet.chain.robinhood.com".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc.parse()?);
    let (token0, token1) = sorted_pair();
    let v1 = DeepstateV1::new(ROUTER, provider.clone());
    let pool_id = compute_pool_id(token0, token1);
    let epoch = v1.poolEpoch(pool_id).call().await?.to::<u64>();
    let book_id = v1.bookId(token0, token1, U256::from(epoch)).call().await?;
    let (bid, ask) = fetch_top_of_book(&v1, token0, token1, epoch, book_id).await?;

    println!("epoch={epoch} book={book_id:#x}");
    println!(
        "best bid: {}",
        bid.as_ref()
            .map(|order| describe_order(order, 6, 18))
            .unwrap_or_else(|| "empty".to_string())
    );
    println!(
        "best ask: {}",
        ask.as_ref()
            .map(|order| describe_order(order, 6, 18))
            .unwrap_or_else(|| "empty".to_string())
    );
    Ok(())
}
