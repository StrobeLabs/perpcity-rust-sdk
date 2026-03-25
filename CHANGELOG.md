# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- `PositionClosed` event ABI now matches deployed contract (added settlement detail fields: `netUsdDelta`, `funding`, `utilizationFee`, `adl`, `liquidationFee`, `netMargin`)
- `NotionalAdjusted` event ABI now matches deployed contract (added settlement detail fields: `swapPerpDelta`, `swapUsdDelta`, `funding`, `utilizationFee`, `adl`, `tradingFees`)
- `adjust_notional` doc comment: corrected `usd_delta` sign convention (positive = receive USD / reduce exposure, negative = spend USD / increase exposure)

### Changed

- `PerpClient::open_taker()` and `open_maker()` now return `OpenResult` (pos_id + entry deltas from the `PositionOpened` event) instead of bare `U256`, eliminating the need for a follow-up RPC read after opening a position
- `PerpClient::adjust_notional()` now takes `&AdjustNotionalParams` and returns `AdjustNotionalResult` (parsed from the `NotionalAdjusted` event) instead of bare `B256`
- `PerpClient::adjust_margin()` now takes `&AdjustMarginParams` and returns `AdjustMarginResult` (parsed from the `MarginAdjusted` event) instead of bare `B256`
- `CloseResult` now includes all `PositionClosed` event fields: `was_maker`, `was_liquidated`, `exit_perp_delta`, `exit_usd_delta`, `net_usd_delta`, `funding`, `utilization_fee`, `adl`, `liquidation_fee`, `net_margin`
- `OpenResult` now includes `tick_lower` and `tick_upper` from the `PositionOpened` event
- Extracted `parse_open_result()`, `parse_close_result()`, `parse_adjust_result()`, and `parse_margin_result()` receipt-parsing helpers

### Added

- `PerpClient::transfer_eth(to, amount_wei, urgency)` — ETH transfer routed through the transaction pipeline for correct nonce management
- `PerpClient::transfer_usdc(to, amount, urgency)` — USDC transfer routed through the transaction pipeline for correct nonce management
- `AdjustNotionalParams` / `AdjustMarginParams` — client-facing params structs consistent with `OpenTakerParams` / `CloseParams`
- `AdjustNotionalResult` — contains `new_perp_delta`, `swap_perp_delta`, `swap_usd_delta`, `funding`, `utilization_fee`, `adl`, `trading_fees`
- `AdjustMarginResult` — contains `new_margin`
- `OpenResult` type — contains `pos_id`, `is_maker`, `perp_delta`, `usd_delta`, `tick_lower`, `tick_upper`
- `send_tx` now applies the `TxRequest.value` field to the `TransactionRequest` (was previously ignored)
- `PerpClient::get_index_price(beacon)` — read oracle index price from a beacon contract (single RPC call)
- `PerpClient::get_positions_by_owner(owner)` — scan position NFTs and return IDs owned by a given address
- `events` module — `MarketEvent` enum and `decode_log()` for decoding raw on-chain logs into typed events (`PositionOpened`, `NotionalAdjusted`, `PositionClosed`, `IndexUpdated`)
- `feed` module — `MarketFeed` for live WebSocket event streaming with per-perp filtering
- `IBeacon` contract interface (`IndexUpdated` event + `index()` view function)
- `price_x96_to_f64()` — base Q96 fixed-point decoder for beacon index prices
- `Q96_PRECISION` constant — proven 0.000001 absolute error bound for Q96 decode
- End-to-end Anvil fork integration test (`tests/anvil_fork.rs`) — full taker lifecycle with adjust notional, adjust margin, and expanded result verification
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
