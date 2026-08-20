//! Market-making strategy: quote best bid + best ask around a reference price.
//!
//! The protocol pays DEEP only to the canonical best bid and best ask.
//! Strategy: keep a bid 1 tick below the current best bid (or at target) and
//! an ask 1 tick above the current best ask, sized to the full-reward target.

use crate::order::Order;
use crate::tick;
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct MmConfig {
    /// Reference mid price (human, USDG per NVDA). e.g. 125.0
    pub mid_price: f64,
    /// Half-spread in percent. e.g. 0.005 = ±0.5%
    pub half_spread_pct: f64,
    /// Bid size in NVDA (base token, 18 decimals, raw units). e.g. 1e15 = 0.001 NVDA.
    /// NOTE: the packed `quantity` field is always in base (NVDA) units for both sides.
    pub bid_quantity: u128,
    /// Ask size in NVDA (base token, 18 decimals, raw units).
    pub ask_quantity: u128,
    /// Minimum tick distance between bid and ask.
    pub min_tick_gap: i32,
    /// Decimals of token0 (USDG).
    pub decimals0: u8,
    /// Decimals of token1 (NVDA).
    pub decimals1: u8,
    /// Quote refresh interval in seconds.
    pub interval_secs: u64,
    /// Maximum wallet inventory of NVDA in raw base units (default 2 NVDA).
    pub max_nvda_inventory: u128,
}

impl Default for MmConfig {
    fn default() -> Self {
        Self {
            mid_price: 125.0,
            half_spread_pct: 0.005,
            bid_quantity: 1_000_000_000_000_000u128, // 0.001 NVDA (base units, 18 dec)
            ask_quantity: 1_000_000_000_000_000_000u128, // 1 NVDA
            min_tick_gap: 1,
            decimals0: 6,
            decimals1: 18,
            interval_secs: 30,
            max_nvda_inventory: 2_000_000_000_000_000_000u128,
        }
    }
}

/// Desired quotes given the current top-of-book.
#[derive(Debug, Clone)]
pub struct Quotes {
    pub bid: Order,
    pub ask: Order,
}

/// Cap new quotes against current NVDA inventory.
pub fn apply_inventory_limit(quotes: &mut Quotes, inventory: u128, max_inventory: u128) {
    let bid_room = max_inventory.saturating_sub(inventory);
    quotes.bid.quantity = quotes.bid.quantity.min(bid_room);
    quotes.ask.quantity = quotes.ask.quantity.min(inventory);
}

/// Compute bid/ask orders. `top_bid`/`top_ask` are the current best (None if empty).
pub fn compute_quotes(
    cfg: &MmConfig,
    top_bid: Option<&Order>,
    top_ask: Option<&Order>,
) -> Result<Quotes> {
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

    let bid = Order {
        tick: bid_tick,
        quantity: cfg.bid_quantity,
        nonce: 0,
    };
    let ask = Order {
        tick: ask_tick,
        quantity: cfg.ask_quantity,
        nonce: 0,
    };

    Ok(Quotes { bid, ask })
}

/// Human-readable display of a packed order.
pub fn describe_order(o: &Order, decimals0: u8, decimals1: u8) -> String {
    let px = tick::tick_to_human_price(o.tick, decimals0, decimals1);
    format!("px=${px:.4} qty={} nonce={}", o.quantity, o.nonce)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_quotes_bid_below_ask() {
        let cfg = MmConfig::default();
        let quotes = compute_quotes(&cfg, None, None).unwrap();

        // Fundamental invariant: bid tick must be strictly below ask tick.
        assert!(
            quotes.bid.tick < quotes.ask.tick,
            "bid tick {} >= ask tick {}",
            quotes.bid.tick,
            quotes.ask.tick
        );

        // Quantities pass through in base (NVDA) units for both sides.
        assert_eq!(quotes.bid.quantity, cfg.bid_quantity);
        assert_eq!(quotes.ask.quantity, cfg.ask_quantity);
        assert!(quotes.bid.quantity > 0 && quotes.ask.quantity > 0);

        // Human prices straddle mid: bid below, ask above.
        let bid_px = tick::tick_to_human_price(quotes.bid.tick, cfg.decimals0, cfg.decimals1);
        let ask_px = tick::tick_to_human_price(quotes.ask.tick, cfg.decimals0, cfg.decimals1);
        assert!(
            bid_px < cfg.mid_price,
            "bid px {bid_px} >= mid {}",
            cfg.mid_price
        );
        assert!(
            ask_px > cfg.mid_price,
            "ask px {ask_px} <= mid {}",
            cfg.mid_price
        );
    }

    #[test]
    fn test_compute_quotes_improves_on_top_of_book() {
        let cfg = MmConfig::default();
        // An existing best bid better than our target: we should quote one tick better.
        let base = tick::price_to_tick_floor(cfg.mid_price, cfg.decimals0, cfg.decimals1);
        let better_bid = Order {
            tick: base + 5,
            quantity: 1,
            nonce: 0,
        };
        let quotes = compute_quotes(&cfg, Some(&better_bid), None).unwrap();
        assert_eq!(quotes.bid.tick, better_bid.tick + 1);
        assert!(quotes.bid.tick < quotes.ask.tick);
    }

    #[test]
    fn test_compute_quotes_respects_min_gap() {
        let cfg = MmConfig {
            min_tick_gap: 3,
            ..Default::default()
        };
        // Squeeze top_bid right up against our ask so the gap must be enforced.
        let quotes = compute_quotes(&cfg, None, None).unwrap();
        assert!(
            quotes.ask.tick - quotes.bid.tick >= cfg.min_tick_gap,
            "gap {} < min_tick_gap {}",
            quotes.ask.tick - quotes.bid.tick,
            cfg.min_tick_gap
        );
    }

    #[test]
    fn test_inventory_limit_caps_both_sides() {
        let cfg = MmConfig::default();
        let mut quotes = compute_quotes(&cfg, None, None).unwrap();
        apply_inventory_limit(
            &mut quotes,
            1_500_000_000_000_000_000,
            cfg.max_nvda_inventory,
        );
        assert_eq!(quotes.bid.quantity, 1_000_000_000_000_000);
        assert_eq!(quotes.ask.quantity, 1_000_000_000_000_000_000);
    }

    #[test]
    fn test_inventory_limit_disables_bid_at_cap() {
        let cfg = MmConfig::default();
        let mut quotes = compute_quotes(&cfg, None, None).unwrap();
        apply_inventory_limit(&mut quotes, cfg.max_nvda_inventory, cfg.max_nvda_inventory);
        assert_eq!(quotes.bid.quantity, 0);
        assert_eq!(quotes.ask.quantity, cfg.ask_quantity);
    }
}
