# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- `PositionClosed` event ABI now matches deployed contract (added settlement detail fields: `netUsdDelta`, `funding`, `utilizationFee`, `adl`, `liquidationFee`, `netMargin`)
- `NotionalAdjusted` event ABI now matches deployed contract (added settlement detail fields: `swapPerpDelta`, `swapUsdDelta`, `funding`, `utilizationFee`, `adl`, `tradingFees`)

### Added

- `events` module — `MarketEvent` enum and `decode_log()` for decoding raw on-chain logs into typed events (`PositionOpened`, `NotionalAdjusted`, `PositionClosed`, `IndexUpdated`)
- `feed` module — `MarketFeed` for live WebSocket event streaming with per-perp filtering
- `IBeacon` contract interface (`IndexUpdated` event + `index()` view function)
- `price_x96_to_f64()` — base Q96 fixed-point decoder for beacon index prices
- `Q96_PRECISION` constant — proven 0.000001 absolute error bound for Q96 decode
- End-to-end Anvil fork integration test (`tests/anvil_fork.rs`) — full taker lifecycle on forked Base Sepolia
- Live WebSocket integration test (`tests/ws_feed.rs`) — MarketFeed against Base Sepolia

## [0.1.0] - 2025-03-09

### Added

- `PerpClient` with full taker and maker position lifecycle (open, close, adjust)
- Mark price and funding rate queries with 2s TTL cache
- Open interest and USDC balance queries
- Live position details (PnL, funding, effective margin, liquidation status)
- USDC approval helper
- HFT module: gas price cache, lock-free nonce management, tx pipeline, state cache, latency tracking, position manager
- Transport layer: multi-RPC failover, health monitoring, WebSocket support
- Pure math: tick/price conversions, liquidity calculations, position math (entry price, PnL, leverage, liquidation price)
- Examples: quickstart, open_position, open_maker, market_maker, hft_bot
- Benchmarks: math, HFT pipeline, transport

[Unreleased]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/releases/tag/v0.1.0
