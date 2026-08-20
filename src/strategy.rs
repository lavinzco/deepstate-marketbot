//! Market-making strategy: quote best bid + best ask around a reference price.
//!
//! The protocol pays DEEP only to the canonical best bid and best ask.
//! Strategy: keep a bid 1 tick below the current best bid (or at target) and
//! an ask 1 tick above the current best ask, sized to the full-reward target.

use crate::order::{self, Order};
use crate::tick;
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct MmConfig {
    /// Reference mid price (human, USDG per NVDA). e.g. 125.0
    pub mid_price: f64,
    /// Half-spread in percent. e.g. 0.005 = ±0.5%
    pub half_spread_pct: f64,
    /// Bid size in USDG (6 decimals, raw units).
    pub bid_quantity: u128,
    /// Ask size in NVDA (18 decimals, raw units).
    pub ask_quantity: u128,
    /// Minimum tick distance between bid and ask.
    pub min_tick_gap: i32,
    /// Decimals of token0 (USDG).
    pub decimals0: u8,
    /// Decimals of token1 (NVDA).
    pub decimals1: u8,
    /// Quote refresh interval in seconds.
    pub interval_secs: u64,
}

impl Default for MmConfig {
    fn default() -> Self {
        Self {
            mid_price: 125.0,
            half_spread_pct: 0.005,
            bid_quantity: 1_000_000,          // 1 USDG
            ask_quantity: 1_000_000_000_000_000_000u128, // 1 NVDA
            min_tick_gap: 1,
            decimals0: 6,
            decimals1: 18,
            interval_secs: 30,
        }
    }
}

/// Desired quotes given the current top-of-book.
#[derive(Debug, Clone)]
pub struct Quotes {
    pub bid: Order,
    pub ask: Order,
}

/// Compute bid/ask orders. `top_bid`/`top_ask` are the current best (None if empty).
pub fn compute_quotes(cfg: &MmConfig, top_bid: Option<&Order>, top_ask: Option<&Order>) -> Result<Quotes> {
    let mid = cfg.mid_price;
    let spread = cfg.half_spread_pct;

    // Desired raw prices around mid.
    let bid_px = mid * (1.0 - spread);
    let ask_px = mid * (1.0 + spread);

    let bid_tick = tick::price_to_tick_floor(bid_px, cfg.decimals0, cfg.decimals1);
    let ask_tick = tick::price_to_tick_ceil(ask_px, cfg.decimals0, cfg.decimals1);

    // Improve on existing best if it's better than our target (or fill if empty).
    let bid_tick = match top_bid {
        Some(t) if t.tick > bid_tick => t.tick + 1, // one tick better than current best
        _ => bid_tick,
    };
    let ask_tick = match top_ask {
        Some(t) if t.tick < ask_tick => t.tick - 1, // one tick better than current best
        _ => ask_tick,
    };

    // Ensure spread gap.
    let bid_tick = if ask_tick - bid_tick < cfg.min_tick_gap {
        ask_tick - cfg.min_tick_gap
    } else {
        bid_tick
    };

    let bid = Order { tick: bid_tick, quantity: cfg.bid_quantity, nonce: 0 };
    let ask = Order { tick: ask_tick, quantity: cfg.ask_quantity, nonce: 0 };

    Ok(Quotes { bid, ask })
}

/// Human-readable display of a packed order.
pub fn describe_order(o: &Order, decimals0: u8, decimals1: u8) -> String {
    let px = tick::tick_to_human_price(o.tick, decimals0, decimals1);
    format!("px=${px:.4} qty={} nonce={}", o.quantity, o.nonce)
}
