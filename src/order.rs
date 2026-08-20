//! Order packing (Deepstate packed order format)
//!
//! Layout (MSB -> LSB):
//! - bits 224-255: signed 32-bit tick (two's complement)
//! - bits 64-223:  160-bit quantity
//! - bits 32-63:   reserved
//! - bits 0-31:    32-bit nonce (0 for fill, engine-assigned when resting)

use alloy::primitives::{B256, U256};
use anyhow::{ensure, Result};

pub const TICK_OFFSET: usize = 224;
pub const QUANTITY_OFFSET: usize = 64;
pub const MASK_32: U256 = U256::from_limbs([u64::MAX, 0, 0, 0]); // low 64 bits covers 32
pub const MASK_160: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Order {
    pub tick: i32,
    pub quantity: u128, // uint160, but u128 is plenty for our sizes
    pub nonce: u32,
}

/// Pack an order into the contract's bytes32 representation.
pub fn pack(tick: i32, quantity: u128, nonce: u32) -> Result<B256> {
    ensure!(quantity <= u128::MAX, "quantity out of range");
    ensure!(tick >= i32::MIN && tick <= i32::MAX, "tick must be int32");

    // tick as two's-complement uint32
    let tick_u32 = tick as u32;
    let tick_bits = U256::from(tick_u32);
    let qty_bits = U256::from(quantity);
    let nonce_bits = U256::from(nonce);

    let packed = (tick_bits << U256::from(TICK_OFFSET))
        | (qty_bits << U256::from(QUANTITY_OFFSET))
        | nonce_bits;

    Ok(B256::from(packed.to_be_bytes()))
}

/// Unpack a bytes32 order back into its components.
pub fn unpack(packed: &B256) -> Order {
    let value = U256::from_be_slice(packed.as_slice());

    let nonce = (value & U256::from(u64::MAX)).to::<u64>() as u32;
    let quantity = ((value >> U256::from(QUANTITY_OFFSET)) & MASK_160).to::<u128>();
    let tick_raw = ((value >> U256::from(TICK_OFFSET)) & U256::from(u64::MAX)).to::<u64>() as u32;

    let tick = if tick_raw >= (1u32 << 31) {
        (tick_raw as i64 - (1i64 << 32)) as i32
    } else {
        tick_raw as i32
    };

    Order { tick, quantity, nonce }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pack_unpack_roundtrip() {
        let o = Order { tick: 12345, quantity: 1_000_000, nonce: 7 };
        let packed = pack(o.tick, o.quantity, o.nonce).unwrap();
        let back = unpack(&packed);
        assert_eq!(o, back);
    }

    #[test]
    fn test_negative_tick() {
        let o = Order { tick: -54321, quantity: 500, nonce: 0 };
        let packed = pack(o.tick, o.quantity, o.nonce).unwrap();
        let back = unpack(&packed);
        assert_eq!(o, back);
    }
}
