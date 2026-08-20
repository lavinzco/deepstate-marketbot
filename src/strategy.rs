//! Market-making strategy: quote best bid + best ask around a reference price.
//!
//! The protocol pays DEEP only to the canonical best bid and best ask.
//! Strategy: improve the current best bid/ask by one tick (or use the target
//! when that side of the book is empty), without crossing the opposite side.

use crate::order::Order;
use crate::tick;
use anyhow::{ensure, Result};

#[derive(Debug, Clone)]
pub struct MmConfig {
    /// Reference mid price (human, USDG per NVDA). e.g. 125.0
    pub mid_price: f64,
    /// Half-spread in percent. e.g. 0.005 = ±0.5%
    pub half_spread_pct: f64,
    /// Bid size in USDG raw units (token0, 6 decimals).
    pub bid_quantity: u128,
    /// Ask size in NVDA raw units (token1, 18 decimals).
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
            bid_quantity: 1_000_000u128, // 1 USDG (raw units, 6 dec)
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
    /// `None` means this side must be safely left unquoted.
    pub bid: Option<Order>,
    pub ask: Option<Order>,
}

/// Cap new quotes against current NVDA inventory.
pub fn apply_balance_limit(
    quotes: &mut Quotes,
    usdg_balance: u128,
    nvda_balance: u128,
    max_nvda_inventory: u128,
) {
    if let Some(bid) = quotes.bid.as_mut() {
        bid.quantity = bid.quantity.min(usdg_balance);
    }
    if let Some(ask) = quotes.ask.as_mut() {
        ask.quantity = ask.quantity.min(nvda_balance).min(max_nvda_inventory);
    }
}

/// Compute bid/ask orders. `top_bid`/`top_ask` are the current best (None if empty).
pub fn compute_quotes(
    cfg: &MmConfig,
    top_bid: Option<&Order>,
    top_ask: Option<&Order>,
) -> Result<Quotes> {
    ensure!(cfg.min_tick_gap >= 0, "min_tick_gap must be non-negative");
    ensure!(
        cfg.mid_price.is_finite() && cfg.mid_price > 0.0,
        "mid price must be finite and positive"
    );
    ensure!(
        cfg.half_spread_pct.is_finite() && cfg.half_spread_pct > 0.0 && cfg.half_spread_pct < 1.0,
        "spread must be finite and in (0, 1)"
    );
    let mid = cfg.mid_price;
    let spread = cfg.half_spread_pct;

    // Desired raw prices around mid.
    let bid_px = mid * (1.0 - spread);
    let ask_px = mid * (1.0 + spread);

    let bid_tick = tick::price_to_tick_floor(bid_px, cfg.decimals0, cfg.decimals1);
    let ask_tick = tick::price_to_tick_ceil(ask_px, cfg.decimals0, cfg.decimals1);

    // Checked arithmetic avoids wrapping at the protocol's i32 tick limits.
    let bid = top_bid.map_or(Some(bid_tick), |top| top.tick.checked_sub(1));
    let ask = top_ask.map_or(Some(ask_tick), |top| top.tick.checked_add(1));
    let bid = bid.filter(|&tick| top_ask.is_none_or(|ask| tick > ask.tick));
    let ask = ask.filter(|&tick| top_bid.is_none_or(|bid| tick < bid.tick));

    let (bid, ask) = match (bid, ask) {
        (Some(bid), Some(ask))
            if i64::from(bid) - i64::from(ask) >= i64::from(cfg.min_tick_gap) =>
        {
            (Some(bid), Some(ask))
        }
        (Some(_), Some(ask)) => (None, Some(ask)),
        (bid, ask) => (bid, ask),
    };

    Ok(Quotes {
        bid: bid.map(|tick| Order {
            tick,
            quantity: cfg.bid_quantity,
            nonce: 0,
        }),
        ask: ask.map(|tick| Order {
            tick,
            quantity: cfg.ask_quantity,
            nonce: 0,
        }),
    })
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
        let bid = quotes.bid.unwrap();
        let ask = quotes.ask.unwrap();
        assert!(bid.tick > ask.tick);
        assert_eq!(bid.quantity, cfg.bid_quantity);
        assert_eq!(ask.quantity, cfg.ask_quantity);
        let bid_px = tick::tick_to_human_price(bid.tick, cfg.decimals0, cfg.decimals1);
        let ask_px = tick::tick_to_human_price(ask.tick, cfg.decimals0, cfg.decimals1);
        assert!(bid_px < cfg.mid_price && ask_px > cfg.mid_price);
    }

    #[test]
    fn test_compute_quotes_improves_both_sides() {
        let cfg = MmConfig::default();
        let bid = Order {
            tick: 736_000_000,
            quantity: 1,
            nonce: 0,
        };
        let ask = Order {
            tick: 735_000_000,
            quantity: 1,
            nonce: 0,
        };
        let quotes = compute_quotes(&cfg, Some(&bid), Some(&ask)).unwrap();
        assert_eq!(quotes.bid.unwrap().tick, bid.tick - 1);
        assert_eq!(quotes.ask.unwrap().tick, ask.tick + 1);
    }

    #[test]
    fn test_narrow_book_safely_disables_ask() {
        let cfg = MmConfig {
            min_tick_gap: 3,
            ..Default::default()
        };
        let bid = Order {
            tick: 735_900_000,
            quantity: 1,
            nonce: 0,
        };
        let ask = Order {
            tick: bid.tick - 3,
            quantity: 1,
            nonce: 0,
        };
        let quotes = compute_quotes(&cfg, Some(&bid), Some(&ask)).unwrap();
        assert!(quotes.bid.is_none());
        assert_eq!(quotes.ask.unwrap().tick, ask.tick + 1);
    }

    #[test]
    fn test_tick_overflow_disables_side() {
        let cfg = MmConfig::default();
        let top_bid = Order {
            tick: i32::MIN,
            quantity: 1,
            nonce: 0,
        };
        let quotes = compute_quotes(&cfg, Some(&top_bid), None).unwrap();
        assert!(quotes.bid.is_none());
        assert!(quotes.ask.is_none());
    }

    #[test]
    fn test_inventory_limit_caps_both_sides() {
        let cfg = MmConfig::default();
        let mut quotes = compute_quotes(&cfg, None, None).unwrap();
        apply_balance_limit(
            &mut quotes,
            500_000,
            1_500_000_000_000_000_000,
            cfg.max_nvda_inventory,
        );
        assert_eq!(quotes.bid.unwrap().quantity, 500_000);
        assert_eq!(quotes.ask.unwrap().quantity, 1_000_000_000_000_000_000);
    }

    #[test]
    fn test_inventory_limit_disables_bid_at_cap() {
        let cfg = MmConfig::default();
        let mut quotes = compute_quotes(&cfg, None, None).unwrap();
        apply_balance_limit(
            &mut quotes,
            0,
            cfg.max_nvda_inventory,
            cfg.max_nvda_inventory,
        );
        assert_eq!(quotes.bid.unwrap().quantity, 0);
        assert_eq!(quotes.ask.unwrap().quantity, cfg.ask_quantity);
    }
}
