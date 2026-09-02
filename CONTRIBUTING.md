# Contributing to PerpCity Rust SDK

## Prerequisites

- [Rust](https://rustup.rs/) (edition 2024, requires rustc 1.85+)
- [Anvil](https://book.getfoundry.sh/anvil/) (for running examples against a local fork)

## Getting Started

1. Clone the repository:
   ```bash
   git clone https://github.com/StrobeLabs/perpcity-rust-sdk.git
   cd perpcity-rust-sdk
   ```

2. Build the project:
   ```bash
   cargo build
   ```

3. Run tests:
   ```bash
   cargo test
   ```

## Development Commands

| Command | Description |
|---------|-------------|
| `cargo build` | Build the SDK |
| `cargo test` | Run all unit tests (pure math, no network) |
| `cargo clippy` | Run lints |
| `cargo fmt --check` | Check formatting |
| `cargo fmt` | Auto-format code |
| `cargo bench` | Run benchmarks (math, HFT, transport) |
| `cargo run --example quickstart` | Run the quickstart example (requires `.env`) |

## Project Structure

```
src/
  lib.rs              # Public API re-exports
  types.rs            # Client-facing types (Deployments, params/results)
  constants.rs        # Protocol constants + SDK read-policy constants
  contracts.rs        # Alloy sol! contract bindings (Perp, PerpFactory, ...)
  convert.rs          # f64 <-> on-chain representation conversions
  client/
    mod.rs            # PerpClient construction, accessors, network constants
    queries.rs        # Read operations (market data, positions, balances)
    maker_equity.rs   # Block-pinned maker-equity batch read
    trades.rs         # Write operations (open/adjust/close, liquidations)
    transactions.rs   # TxBuilder: simulate, sign, broadcast, poll receipts
  errors/
    mod.rs            # PerpCityError + Result + classification helpers
    contract.rs       # On-chain / protocol state errors
    transaction.rs    # Transaction lifecycle errors
    validation.rs     # Input validation errors
    decode.rs         # Revert-selector decoding
  feeds/
    market.rs         # MarketFeed: live WebSocket market events
    events.rs         # MarketEvent decoding (both deployed eras)
    block.rs          # Block-header feed
    taker.rs          # Live taker-book publisher
  math/
    tick.rs           # Tick <-> price, tick alignment, sqrt price math
    liquidity.rs      # Liquidity <-> amount calculations
    position.rs       # Entry price, PnL, leverage, liquidation price
    ema.rs            # Contract-exact EMA advancement
    swap.rs           # Local V4 taker swap simulation
    maker_equity.rs   # Contract-exact maker settle preview
    storage.rs        # Storage-slot math (crate-internal)
    fixed_point.rs    # mulDiv / rounding primitives (crate-internal)
  hft/
    gas.rs            # Fee cache, gas-limit cache, pre-computed limits
    nonce.rs          # Lock-free nonce management
    pipeline.rs       # Transaction pipeline with confirmation tracking
    state_cache.rs    # Multi-layer state cache
    latency.rs        # RPC latency tracking
    position_manager.rs  # Position tracking with triggers
  transport/
    config.rs         # Transport configuration builder
    provider.rs       # Multi-endpoint RPC routing
    health.rs         # Endpoint health monitoring
    ws.rs             # WebSocket support
examples/
  quickstart.rs       # Open a long, check PnL, close it
  open_position.rs    # Detailed taker lifecycle
  open_maker.rs       # Open a maker (LP) position
  market_maker.rs     # Continuous market-making loop
  hft_bot.rs          # High-frequency trading bot
  race.rs             # Transaction racing example
  taker_price_impact.rs # Local exact taker quoting
  maker_equity.rs     # Batched maker settle previews + gated liquidation
  aws_kms_signer.rs   # AWS KMS signing (requires --features aws)
benches/
  math_bench.rs       # Pure math benchmarks
  hft_bench.rs        # HFT pipeline benchmarks
  transport_bench.rs  # Transport layer benchmarks
```

## Code Style

- Run `cargo fmt` before committing. CI enforces formatting.
- Run `cargo clippy` and fix all warnings. CI treats warnings as errors.
- Keep the `math/` module free of async, I/O, and external network dependencies. All math functions are pure and deterministic.
- Avoid heap allocations on the hot path. The `hft/` module is designed for low-latency execution.
- All public types and functions should have doc comments.

## Pull Request Workflow

1. Create a feature branch from `main`
2. Make your changes
3. Run CI checks locally:
   ```bash
   cargo fmt --check && cargo clippy && cargo test
   ```
4. Open a pull request against `main`
5. All CI checks must pass before merge

## Reporting Issues

Open an issue on [GitHub](https://github.com/StrobeLabs/perpcity-rust-sdk/issues) with:

- A description of the issue
- Steps to reproduce
- Expected vs actual behavior
- Rust version (`rustc --version`)
