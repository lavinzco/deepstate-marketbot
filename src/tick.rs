//! Tick <-> price conversion (Deepstate TickMath32)
//!
//! price = 2^(96 * tick / 2^31)
//! tick  = log2(price) * 2^31 / 96

const TWO_POW_31: f64 = 2_147_483_648.0;
const LOG_BASE: f64 = 96.0 / TWO_POW_31;

/// Protocol tick -> raw price (token1 per token0, contract decimals).
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

/// Human-readable price (token0 in terms of token1) -> raw contract price.
pub fn human_to_contract_price(human_price: f64, decimals0: u8, decimals1: u8) -> f64 {
    let scale = 10f64.powi(decimals1 as i32 - decimals0 as i32);
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

/// Protocol tick -> human-readable price.
pub fn tick_to_human_price(tick: i32, decimals0: u8, decimals1: u8) -> f64 {
    let contract_price = tick_to_price(tick);
    let scale = 10f64.powi(decimals1 as i32 - decimals0 as i32);
    contract_price / scale
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tick_roundtrip() {
        // mid ~ $125 (NVDA price in USDG): price = 125 * 10^12 (dec1-dec0 = 18-6)
        let human_price = 125.0;
        let tick = price_to_tick_floor(human_price, 6, 18);
        let back = tick_to_human_price(tick, 6, 18);
        assert!((back - 125.0).abs() < 0.01, "got {back}");
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
