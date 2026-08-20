# Deepstate Market-Making Bot (Rust)

自动做市机器人：在 Deepstate Protocol 的 NVDA/USDG 链上订单簿上持续挂最优 bid + best ask，赚取 DEEP 做市奖励。

## 原理

- Deepstate 只奖励 **best bid** 和 **best ask**（最优价）的做市商
- 奖励按对数曲线排放：`C(t) = M × ln(1+t/T)/ln(1+D/T)`（M=5亿 DEEP/侧，D=395天，T=30天）
- 满额数量指数爬坡 30 天（USDG: 1→1,000,000；NVDA: 1→5,000），低于目标按比例
- 机器人每轮：撤旧单 → 按外部参考价 ± spread 挂新单 → 下轮重复

## 架构

```
src/
├── lib.rs          # 库入口
├── contracts.rs    # 合约接口 (sol! ABI) + 地址常量
├── tick.rs         # tick ↔ 价格转换 (2^(96*t/2^31))
├── order.rs        # 订单打包 (tick<<224 | qty<<64 | nonce)
├── strategy.rs     # 报价策略（bid/ask 计算）
├── main.rs         # 主循环（撤单+下单）
└── bin/probe.rs    # 只读探针（查链上状态）
```

## 快速开始

```bash
# 1. 配置
export DEEPSTATE_PRIVATE_KEY=0x你的私钥
export DEEPSTATE_MID_PRICE=125.0        # NVDA 参考价 (USDG per NVDA)
export DEEPSTATE_SPREAD=0.005           # 半价差 0.5%
export DEEPSTATE_INTERVAL=30            # 报价刷新间隔（秒）
export DEEPSTATE_BID_QTY=1000000                 # bid: USDG raw units (1 USDG at 6 decimals)
export DEEPSTATE_ASK_QTY=1000000000000000000    # ask: NVDA raw units (1 NVDA at 18 decimals)

# 2. 构建 + 测试
cargo build
cargo test

# 3. 只读探针（先看链上状态，不下单）
cargo run --bin probe

# 4. 运行机器人
cargo run
```

## 链上参数（Robinhood Chain, Chain ID 4663）

| 合约 | 地址 |
|---|---|
| Router (DeepstateV1) | `0x6cf19308C22FC82ea620Fa0B3E94948d20f27B96` |
| Rewarder | `0xE85ADBC03a6b52a2c9894c1BB525eC883ea156D7` |
| USDG (token0, 6 dec) | `0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168` |
| NVDA (token1, 18 dec) | `0xd0601CE157Db5bdC3162BbaC2a2C8aF5320D9EEC` |
| DEEP | `0x1DA24f6Bb623b9d1aFEae3F3146659A2662D6d27` |
| RPC | `https://rpc.mainnet.chain.robinhood.com` |

## 订单格式

```
packed = (tick << 224) | (quantity << 64) | nonce
```
- tick: int32 对数价格（`price = 2^(96*tick/2^31)`）
- quantity: uint160
- nonce: uint32（fill 时填 0，引擎分配）

## 领奖励流程（重要）

```rust
// 1. 撤单前先注册 claimant（否则奖励永久丢失）
rewarder.registerClaimant(bookId, order)
// 2. 撤单（同时回收抵押 + 成交款）
engine.cancel(token0, token1, epoch, order)
// 3. 领取 DEEP
rewarder.distributeRewards(bookId, order, soldToken)
```

⚠️ **顺序不能反**：先 cancel 再 registerClaimant = 奖励丢失。

## 风险提示

- **存货风险**：NVDA 价格波动可能导致 bid 被吃后持有亏损仓位
- **价格竞争**：只有最优价赚奖励，需要持续盯盘调价
- **实验协议**：Deepstate 官方声明可能失败，仅用少量资金
- 主循环每轮通过 `roots` + `tree` 读取链上完整 top leaf（不会把 `topOrder.soldAmount` 猜成 tick）；`topOrder` 只用于 nonce 一致性校验。
- `DEEPSTATE_MID_PRICE` 仍是外部参考价，必须由部署者提供可验证行情源；没有链上 top leaf 时策略回退到参考价目标。
