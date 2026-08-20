//! Deepstate contract interfaces and interaction helpers.

use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolValue;
use anyhow::{ensure, Result};

sol! {
    #[sol(rpc)]
    contract DeepstateV1 {
        struct FillParams {
            address token0;
            address token1;
            uint256 epoch;
            bytes32 order;
            bool isBid;
            bool noRest;
            bool fillOrKill;
        }

        function fill(FillParams calldata params) external payable returns (bytes32 restingOrder);
        function fillRoute(FillParams[] calldata fills) external payable returns (bytes32[] memory restingOrders);
        function cancel(address token0, address token1, uint256 epoch, bytes32 order) external returns (uint256 baseAmount, uint256 quoteAmount);
        function poolId(address token0, address token1) external pure returns (bytes32 id);
        function bookId(address token0, address token1, uint256 epoch) external pure returns (bytes32 id);
        function activeBookId(address token0, address token1) external view returns (bytes32);
        function poolEpoch(bytes32 pid) external view returns (uint256);
        function topOrder(bytes32 id, bool isBid) external view returns (uint32 nonce, uint160 soldAmount);
        function nextNonce(address token0, address token1, uint256 epoch) external view returns (uint32);
        function ownerOfOrder(bytes32 orderKey) external view returns (address);
        function feeConfig() external view returns (address recipient, uint16 bps);
    }

    #[sol(rpc)]
    contract DeepstateRewarder {
        struct OrderReference {
            bytes32 bookId;
            bytes32 order;
        }
        struct RewardClaim {
            bytes32 bookId;
            bytes32 order;
            address token;
        }

        function registerClaimant(bytes32 bookId, bytes32 order) external returns (address claimant);
        function registerClaimants(OrderReference[] calldata orders) external returns (address claimant);
        function distributeRewards(bytes32 bookId, bytes32 order, address token) external;
        function distributeRewardsBatch(RewardClaim[] calldata claims) external;
        function previewReward(address token, uint256 start, uint256 end, uint160 amount) external view returns (uint256);
        function rewardees(address token) external view returns (uint32 orderNonce, uint64 startedAt);
        function emissionStart(address token) external view returns (uint64 activatedAt);
        function totalAccrued(address token) external view returns (uint96 accrued);
    }

    #[sol(rpc)]
    contract ERC20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

pub const ROUTER: Address = alloy::primitives::address!("6cf19308C22FC82ea620Fa0B3E94948d20f27B96");
pub const REWARDER: Address = alloy::primitives::address!("E85ADBC03a6b52a2c9894c1BB525eC883ea156D7");
pub const USDG: Address = alloy::primitives::address!("5fc5360D0400a0Fd4f2af552ADD042D716F1d168");
pub const NVDA: Address = alloy::primitives::address!("d0601CE157Db5bdC3162BbaC2a2C8aF5320D9EEC");
pub const DEEP: Address = alloy::primitives::address!("1DA24f6Bb623b9d1aFEae3F3146659A2662D6d27");

/// Sorted pair: USDG < NVDA (address ordering).
pub fn sorted_pair() -> (Address, Address) {
    (USDG, NVDA)
}

/// Pool id = keccak256(token0, token1).
pub fn compute_pool_id(token0: Address, token1: Address) -> B256 {
    let mut bytes = [0u8; 64];
    bytes[12..32].copy_from_slice(token0.as_slice());
    bytes[44..64].copy_from_slice(token1.as_slice());
    let hash = alloy::primitives::keccak256(bytes);
    B256::from(hash)
}

/// Fetch current epoch for a pool.
pub async fn fetch_epoch<P: Provider>(provider: &P, token0: Address, token1: Address) -> Result<u64> {
    let pid = compute_pool_id(token0, token1);
    let v1 = DeepstateV1::new(ROUTER, provider);
    let epoch = v1.poolEpoch(pid).call().await?;
    Ok(epoch.to::<u64>())
}

/// Fetch active book id.
pub async fn fetch_active_book_id<P: Provider>(provider: &P, token0: Address, token1: Address) -> Result<B256> {
    let v1 = DeepstateV1::new(ROUTER, provider);
    let book = v1.activeBookId(token0, token1).call().await?;
    Ok(book)
}

/// Fetch current top-of-book for a side.
pub async fn fetch_top<P: Provider>(
    provider: &P,
    book_id: B256,
    is_bid: bool,
) -> Result<(u32, u128)> {
    let v1 = DeepstateV1::new(ROUTER, provider);
    let res = v1.topOrder(book_id, is_bid).call().await?;
    Ok((res.nonce, res.soldAmount.to::<u128>()))
}
