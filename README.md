# PerpCity Rust SDK

Rust SDK for the [PerpCity](https://perp.city) perpetual futures protocol on Base L2. Built for high-frequency trading with lock-free nonce management, multi-endpoint transport with circuit breakers, and a 2-tier state cache.

## Installation

```toml
[dependencies]
perpcity-sdk = "0.2"
```

## Quickstart

**Prerequisites:** Rust 1.85+, a Base Sepolia RPC (the public endpoint works fine).

1. Clone and create a `.env` file:

```bash
git clone https://github.com/StrobeLabs/perpcity-rust-sdk.git
cd perpcity-rust-sdk
```

```env
PERPCITY_PRIVATE_KEY=your_hex_private_key
PERPCITY_MANAGER=0x...   # PerpManager contract address
PERPCITY_PERP_ID=0x...   # bytes32 perp market ID
```

2. Fund your wallet on Base Sepolia — you need a small amount of ETH for gas and some USDC for margin. The testnet USDC has a public `mint` function:

```bash
# Mint 10,000 USDC to your address
cast send 0xC1a5D4E99BB224713dd179eA9CA2Fa6600706210 \
  "mint(address,uint256)" YOUR_ADDRESS 10000000000 \
  --rpc-url https://sepolia.base.org \
  --private-key YOUR_PRIVATE_KEY
```

3. Run the quickstart:

```bash
cargo run --release --example quickstart
```

## Examples

All examples load configuration from `.env` automatically via `dotenvy`.

| Example | Run | What it does |
|---------|-----|-------------|
| **quickstart** | `cargo run --example quickstart` | Open a 2x long, close it. ~20 lines of setup. |
| **open_position** | `cargo run --example open_position` | Full taker lifecycle: market data, open, monitor PnL/funding/liquidation, close. |
| **market_maker** | `cargo run --example market_maker` | LP position: calculate tick range around mark, estimate liquidity, open maker position. *Note: makers are currently subject to a 7-day lockup, so this example shouldn't run.* |
| **hft_bot** | `cargo run --example hft_bot` | Full trading loop: multi-endpoint transport, momentum strategy, position manager with SL/TP/trailing stop, latency stats. |

## API Overview

### Client Setup

```rust
use perpcity_sdk::*;

// 1. Transport — single endpoint or read/write split
let transport = HftTransport::new(
    TransportConfig::builder()
        .shared_endpoint("https://sepolia.base.org")
        .build()?,
)?;

// 2. Client
let client = PerpClient::new(transport, signer, deployments, 84532)?;

// 3. Warm caches (required before first transaction)
client.sync_nonce().await?;
client.refresh_gas().await?;
client.ensure_approval(U256::MAX).await?;
```

### Trading

```rust
// Open a 5x long with 10 USDC margin
let pos_id = client.open_taker(perp_id, &OpenTakerParams {
    is_long: true,
    margin: 10.0,
    leverage: 5.0,
    unspecified_amount_limit: 0,
}, Urgency::Normal).await?;

// Close it
let result = client.close_position(pos_id, &CloseParams {
    min_amt0_out: 0,
    min_amt1_out: 0,
    max_amt1_in: u128::MAX,
}, Urgency::Normal).await?;
```

Every write method takes an `Urgency` level that scales the EIP-1559 priority fee:

| Urgency | Priority fee multiplier | Use case |
|---------|------------------------|----------|
| `Low` | 0.8x | Background tasks |
| `Normal` | 1.0x | Standard trading |
| `High` | 1.5x | Time-sensitive fills |
| `Critical` | 2.0x | Liquidation defense |

### Market Data

```rust
// Snapshot — config + live data in 2 multicalls (2 CUs instead of 5+)
let (config, snapshot) = client.get_perp_snapshot(perp_id).await?;

// Or individually
let mark    = client.get_mark_price(perp_id).await?;    // f64 price
let funding = client.get_funding_rate(perp_id).await?;  // daily rate
let oi      = client.get_open_interest(perp_id).await?;  // long/short OI
let live    = client.get_live_details(pos_id).await?;    // PnL, funding, liquidation

// Batch balances — N addresses in 1 multicall (1 CU instead of 2N)
let (usdc, eth) = client.get_balances(address).await?;
let all = client.get_balances_batch(&addresses).await?;
```

### HFT Infrastructure

The SDK is designed for sub-millisecond transaction preparation on the hot path.

**Multi-endpoint transport with read/write split** — route reads to free public RPCs, writes to paid endpoints:

```rust
let transport = HftTransport::new(
    TransportConfig::builder()
        .shared_endpoint("https://base.g.alchemy.com/v2/KEY")  // writes + read fallback
        .read_endpoint("https://base-rpc.publicnode.com")       // dedicated reads
        .strategy(Strategy::LatencyBased)
        .request_timeout(Duration::from_millis(2000))
        .build()?,
)?;
```

Each pool gets independent circuit breakers — if the read endpoint goes down, reads automatically fall back to the shared endpoint.

**Lock-free nonce management** — `AtomicU64::fetch_add` for O(1) nonce acquisition. No mutex on the hot path.

**Fee cache** — EIP-1559 base fee + priority fee cached from block headers. Refreshed per block.

**Gas limit cache** — `eth_estimateGas` results cached by function selector (1 hour TTL, 20% buffer). First call estimates, subsequent calls use the cache.

**2-tier state cache:**
- **Slow tier** (60s TTL): fees, margin bounds — rarely change
- **Fast tier** (2s TTL): mark price, funding rate, USDC balance — refreshed per block

**Position manager** — track open positions with automated triggers:

```rust
pos_manager.track(ManagedPosition {
    perp_id: perp_bytes,
    position_id: pos_id,
    is_long: true,
    entry_price: mark,
    margin: 10.0,
    stop_loss: Some(mark * 0.98),
    take_profit: Some(mark * 1.05),
    trailing_stop_pct: Some(0.03),
    trailing_stop_anchor: None,
});
```

**Latency tracker** — rolling-window stats (P50/P95/P99) for monitoring RPC and tx latency.

### Math Utilities

```rust
use perpcity_sdk::{price_to_tick, tick_to_price, align_tick_down};
use perpcity_sdk::math::position::{entry_price, liquidation_price};
use perpcity_sdk::math::liquidity::estimate_liquidity;

let tick = price_to_tick(50.0)?;
let price = tick_to_price(tick)?;
let liq = estimate_liquidity(tick_lower, tick_upper, margin_scaled)?;
```

All math functions are pure, `O(1)`, and ported faithfully from PerpCity's Solidity contracts and Uniswap V4's `TickMath`.

## Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `PERPCITY_PRIVATE_KEY` | Yes | Hex-encoded private key (with or without `0x` prefix) |
| `PERPCITY_MANAGER` | Yes | PerpManager contract address |
| `PERPCITY_PERP_ID` | Yes | bytes32 perp market identifier |
| `RPC_URL` | No | RPC endpoint (default: `https://sepolia.base.org`) |
| `RPC_URL_1`, `RPC_URL_2` | No | Multi-endpoint config for `hft_bot` example |

## Configuration

### Deployments

The `Deployments` struct holds contract addresses. For Base Sepolia:

```rust
let manager: Address = std::env::var("PERPCITY_MANAGER")?.parse()?;

let deployments = Deployments {
    perp_manager: manager,
    usdc: address!("C1a5D4E99BB224713dd179eA9CA2Fa6600706210"),
    fees_module: None,
    margin_ratios_module: None,
    lockup_period_module: None,
    sqrt_price_impact_limit_module: None,
};

let client = PerpClient::new(transport, signer, deployments, 84532)?; // Base Sepolia
```

For Base mainnet, use `PerpClient::new_base_mainnet()` which sets chain ID 8453 and the mainnet USDC address (`0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913`).

### Release Profile

The crate ships with aggressive release optimizations for trading workloads:

```toml
[profile.release]
lto = "fat"           # Cross-crate inlining
codegen-units = 1     # Maximum optimization
panic = "abort"       # No unwind overhead
strip = "symbols"     # Smaller binary
```

## Benchmarks

```bash
cargo bench
```

| Benchmark | What it measures |
|-----------|-----------------|
| `math_bench` | Tick math, price conversions, entry/liquidation price |
| `hft_bench` | Nonce acquire (~1-5ns), gas peek, cache hit/miss, position triggers |
| `transport_bench` | Endpoint selection, circuit breaker state checks, struct sizes |

## License

MIT
