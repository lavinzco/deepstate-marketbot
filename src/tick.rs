//! Tick <-> price conversion (Deepstate TickMath32)
//!
//! protocol price = 2^(96 * tick / 2^31), expressed as token1/token0
//! in raw units. Human quote/base price is its reciprocal after decimal
//! normalization.

const TWO_POW_31: f64 = 2_147_483_648.0;
const LOG_BASE: f64 = 96.0 / TWO_POW_31;

/// Protocol tick -> raw token1/token0 contract price.
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

/// Human-readable quote/base price -> raw token1/token0 contract price.
///
/// e.g. NVDA/USDG: dec0 = 6 (USDG), dec1 = 18 (NVDA) -> raw =
/// `1 / (human * 10^(dec0 - dec1))`.
pub fn human_to_contract_price(human_price: f64, decimals0: u8, decimals1: u8) -> f64 {
    let scale = 10f64.powi(decimals0 as i32 - decimals1 as i32);
    1.0 / (human_price * scale)
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

/// Protocol tick -> human-readable quote/base price.
pub fn tick_to_human_price(tick: i32, decimals0: u8, decimals1: u8) -> f64 {
    let contract_price = tick_to_price(tick);
    let scale = 10f64.powi(decimals0 as i32 - decimals1 as i32);
    1.0 / (contract_price * scale)
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
        // $125 USDG per NVDA -> inverse raw price 8e9 -> tick ≈ +735.9M.
        // Guards against the token-decimal and protocol price-direction inversion.
        let tick = price_to_tick_floor(125.0, 6, 18);
        assert!(tick > 0, "tick must be positive, got {tick}");
        assert!(
            (735_800_000..=736_000_000).contains(&tick),
            "tick {tick} not in expected ≈[735.8M, 736.0M] range"
        );

        let contract_price = human_to_contract_price(125.0, 6, 18);
        assert!(
            (contract_price - 8.0e9).abs() < 1.0,
            "raw price {contract_price:e} != 8e9"
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
        assert!(bid > ask);
        let bid_px = tick_to_human_price(bid, 6, 18);
        let ask_px = tick_to_human_price(ask, 6, 18);
        assert!(bid_px < 125.0 && ask_px > 125.0);
    }
}
