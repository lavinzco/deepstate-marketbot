//! Tick <-> price conversion (Deepstate TickMath32)
//!
//! price = 2^(96 * tick / 2^31)
//! tick  = log2(price) * 2^31 / 96

const TWO_POW_31: f64 = 2_147_483_648.0;
const LOG_BASE: f64 = 96.0 / TWO_POW_31;

/// Protocol tick -> raw contract price (quote/base in raw units, i.e. token0 per token1).
pub fn tick_to_price(tick: i32) -> f64 {
    2f64.powf(LOG_BASE * tick as f64)
}

/// Raw contract price -> protocol tick (not rounded).
pub fn price_to_tick(price: f64) -> f64 {
    if price <= 0.0 {
        panic!("price must be positive");
    }
    price.log2() * TWO_POW_31 / 96.0
}

/// Human-readable price (quote/base, i.e. token0 per token1) -> raw contract price.
///
/// The tick represents a quote/base price: `raw = human * 10^(dec0 - dec1)`.
/// e.g. NVDA/USDG: dec0 = 6 (USDG), dec1 = 18 (NVDA) -> raw = human * 1e-12.
pub fn human_to_contract_price(human_price: f64, decimals0: u8, decimals1: u8) -> f64 {
    let scale = 10f64.powi(decimals0 as i32 - decimals1 as i32);
    human_price * scale
}

/// Human-readable price -> tick, floored (maker bid side).
pub fn price_to_tick_floor(human_price: f64, decimals0: u8, decimals1: u8) -> i32 {
    let contract_price = human_to_contract_price(human_price, decimals0, decimals1);
    price_to_tick(contract_price).floor() as i32
}

/// Human-readable price -> tick, ceiling (maker ask side).
pub fn price_to_tick_ceil(human_price: f64, decimals0: u8, decimals1: u8) -> i32 {
    let contract_price = human_to_contract_price(human_price, decimals0, decimals1);
    price_to_tick(contract_price).ceil() as i32
}

/// Protocol tick -> human-readable price (quote/base, i.e. token0 per token1).
pub fn tick_to_human_price(tick: i32, decimals0: u8, decimals1: u8) -> f64 {
    let contract_price = tick_to_price(tick);
    let scale = 10f64.powi(decimals0 as i32 - decimals1 as i32);
    contract_price / scale
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tick_roundtrip() {
        // mid ~ $125 (NVDA price in USDG): raw = 125 * 10^(6-18) = 125 * 1e-12
        let human_price = 125.0;
        let tick = price_to_tick_floor(human_price, 6, 18);
        let back = tick_to_human_price(tick, 6, 18);
        assert!((back - 125.0).abs() < 0.01, "got {back}");
    }

    #[test]
    fn test_absolute_tick_magnitude() {
        // $125 USDG per NVDA -> raw price 1.25e-10 -> tick ≈ -735.9M (negative, ~7.36e8).
        // Guards against the 10^(dec1-dec0) sign inversion (which would give +735.9M).
        let tick = price_to_tick_floor(125.0, 6, 18);
        assert!(tick < 0, "tick must be negative, got {tick}");
        assert!(
            (-736_000_000..=-735_800_000).contains(&tick),
            "tick {tick} not in expected ≈[-736.0M, -735.8M] range"
        );

        let contract_price = human_to_contract_price(125.0, 6, 18);
        assert!(
            (contract_price - 1.25e-10).abs() < 1e-22,
            "raw price {contract_price:e} != 1.25e-10"
        );

        // Inverse: the human price of the computed tick must round-trip near $125.
        let back = tick_to_human_price(tick, 6, 18);
        assert!((back - 125.0).abs() < 0.01, "roundtrip got {back}");
    }

    #[test]
    fn test_spread_ticks() {
        // $125 ± 1% spread
        let bid = price_to_tick_floor(125.0 * 0.99, 6, 18);
        let ask = price_to_tick_ceil(125.0 * 1.01, 6, 18);
        assert!(bid < ask);
        let bid_px = tick_to_human_price(bid, 6, 18);
        let ask_px = tick_to_human_price(ask, 6, 18);
        assert!(bid_px < 125.0 && ask_px > 125.0);
    }
}
