# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Error restructuring.** The monolithic `PerpCityError` enum is now composed from per-module error types: `TransactionError` (simulation, signing, gas, pipeline), `ValidationError` (prices, margins, ticks, leverage, config), and `ContractError` (perps, positions, events, quotes, multicall). `PerpCityError` composes all three via `#[from]` conversions. Internal functions return the narrowest error type they can produce (e.g. `pipeline::prepare()` returns `Result<_, TransactionError>`).
- **Pre-flight simulation.** `simulate()` replaces `resolve_gas_limit()` as the unified entry point for gas resolution and transaction validation. On cache miss, `eth_estimateGas` provides both the gas estimate and simulation. On cache hit, `eth_call` verifies the transaction is still valid before broadcast. Every code path guarantees the transaction has been simulated — no more on-chain reverts from warm gas cache.
- **Transaction builder.** `send_tx()` and `send_tx_with_value()` replaced by `TxBuilder`, created via `PerpClient::tx(to, calldata)`. Optional setters: `.with_value()`, `.with_gas_limit()`, `.with_urgency()`. Defaults: `value = 0`, `gas_limit = None` (triggers simulation), `urgency = Normal`. Adding new transaction parameters no longer changes existing call sites.
- **Tracing discipline.** All SDK logging demoted from `info` to `debug`. The SDK is a library — consumers own the `info`-level narrative. SDK logs are for infrastructure debugging via `RUST_LOG=perpcity_sdk=debug`. Warnings and errors unchanged.
- **Client module restructured.** `client.rs` (1500+ lines) split into `client/mod.rs`, `client/trades.rs`, `client/queries.rs`, `client/transactions.rs`. Each submodule implements `impl PerpClient` for its methods.
- **Manual receipt polling.** Replaced Alloy's `pending.get_receipt()` with direct `get_transaction_receipt` polling (2s initial delay, 2s interval, 30s timeout). Avoids triggering Alloy's background `eth_blockNumber` poller that persists for the provider's lifetime. Retries on transient RPC errors during polling.

### Fixed

- **In-flight transaction eviction.** Failed `poll_receipt` and on-chain reverts now evict the transaction from the pipeline's in-flight map, preventing permanent slot consumption. Previously, 16 failed transactions would jam the pipeline and block all new transactions including closes and retreats.

### Added

- `errors::decode` module — decodes PerpCity contract error selectors (20+ known selectors) into human-readable names. Used in gas estimation, pre-flight simulation, and quote functions.
- `TransactionError::SimulationReverted` — returned when `eth_estimateGas` or `eth_call` detects a contract revert before broadcast. Carries decoded error name, selector, and revert data.
- `TransactionError::ReceiptTimeout` — distinct from `Reverted` for receipt polling timeouts.
- `TransactionError::SigningFailed` — distinct from `Reverted` for signing errors.
- `ValidationError::DecodeFailed` — for ABI decode errors (distinct from `Overflow`).
- `ContractError::QuoteReverted` — for quote function reverts with decoded error names.
- `ContractError::MulticallFailed` — for multicall count mismatches and subcall failures.
- `PerpCityError::is_simulation_revert()` — check if the error is a pre-broadcast simulation revert.
- `PerpCityError::is_transient()` — check if the error is transient and worth retrying (RPC errors, gas unavailable, receipt timeouts).
- `TxBuilder` — public transaction builder, re-exported from crate root.
- Gas limit validation: `TxBuilder::send()` rejects `gas_limit = 0` with a clear `ValidationError`.

## [0.2.1] - 2026-04-01

### Fixed

- Set `is_local=false` on `RpcClient` — was incorrectly set to `true`, causing Alloy's internal block poller to run at 250ms intervals (local node rate) instead of 7s (remote node rate). This was the largest hidden source of `eth_blockNumber` RPC calls.

## [0.2.0] - 2026-04-01

### Fixed

- Write retry: stale-replica rejections no longer affect the circuit breaker — these are transient conditions, not evidence of an unhealthy endpoint
- `PositionClosed` event ABI now matches deployed contract (added settlement detail fields: `netUsdDelta`, `funding`, `utilizationFee`, `adl`, `liquidationFee`, `netMargin`)
- `NotionalAdjusted` event ABI now matches deployed contract (added settlement detail fields: `swapPerpDelta`, `swapUsdDelta`, `funding`, `utilizationFee`, `adl`, `tradingFees`)
- `adjust_notional` doc comment: corrected `usd_delta` sign convention (positive = receive USD / reduce exposure, negative = spend USD / increase exposure)

### Changed

- **Transport: read/write/shared endpoint pools.** `TransportConfig` now supports three endpoint pools: shared (`.shared_endpoint()`), read (`.read_endpoint()`), and write (`.write_endpoint()`). Reads prefer the read pool, writes prefer the write pool, both fall back to the shared pool when dedicated endpoints are unhealthy. Each pool gets independent circuit breakers and health tracking. This enables routing reads to free public RPCs while reserving paid endpoints for writes.
- **Transport: `TransportInner` → `Router` + `EndpointPool`.** Endpoint selection logic extracted into `EndpointPool` (owns endpoints, round-robin counter, and selection methods). `Router` holds three pools and implements pool-aware request routing. `EndpointPool` is public for benchmarking.
- `.endpoint()` renamed to `.shared_endpoint()` on `TransportConfigBuilder`
- `http_endpoints` renamed to `shared_endpoints` on `TransportConfig`
- **Gas limits now estimated dynamically.** Contract calls use `eth_estimateGas` on first invocation, cached by function selector (1 hour TTL, 20% buffer). Explicit gas limits can still be passed to skip estimation. Hardcoded `GasLimits` constants are preserved as reference values.
- `GasCache` renamed to `FeeCache` (caches EIP-1559 base fee pricing); `GasEstimateCache` renamed to `GasLimitCache` (caches per-operation gas limits)
- Removed dead `POOL_MANAGER` and `USDC` address constants (the `Deployments` struct is the actual source of deployed addresses)
- `refresh_gas()` now fetches the latest block directly in a single RPC call (`get_block_by_number(Latest)`) instead of two (`get_block_number` + `get_block_by_number`)
- `RetryConfig` split into `ReadRetryConfig` and `WriteRetryConfig` with separate defaults and builder methods (`read_retry()`, `write_retry()`)
- Writes now retry on any pre-mempool RPC rejection (any error response to `eth_sendRawTransaction` means the tx never entered the mempool, so resending is safe); defaults: 3 retries, 500ms exponential backoff
- `WriteRetryConfig::is_retriable()` centralizes the retriable error code policy
- `TransportConfig` fields renamed: `retry` → `read_retry`, added `write_retry`
- `PerpClient::open_taker()` and `open_maker()` now return `OpenResult` (pos_id + entry deltas from the `PositionOpened` event) instead of bare `U256`, eliminating the need for a follow-up RPC read after opening a position
- `PerpClient::adjust_notional()` now takes `&AdjustNotionalParams` and returns `AdjustNotionalResult` (parsed from the `NotionalAdjusted` event) instead of bare `B256`
- `PerpClient::adjust_margin()` now takes `&AdjustMarginParams` and returns `AdjustMarginResult` (parsed from the `MarginAdjusted` event) instead of bare `B256`
- `CloseResult` now includes all `PositionClosed` event fields: `was_maker`, `was_liquidated`, `exit_perp_delta`, `exit_usd_delta`, `net_usd_delta`, `funding`, `utilization_fee`, `adl`, `liquidation_fee`, `net_margin`
- `OpenResult` now includes `tick_lower` and `tick_upper` from the `PositionOpened` event
- Extracted `parse_open_result()`, `parse_close_result()`, `parse_adjust_result()`, and `parse_margin_result()` receipt-parsing helpers

### Added

- `IMulticall3` contract interface — `aggregate3`, `Call3`, `Result`, and `getEthBalance` bindings for the canonical [Multicall3](https://www.multicall3.com) contract
- `MULTICALL3` constant — the canonical Multicall3 address (`0xcA11bde05977b3631167028862bE2a173976CA11`), deployed identically on all EVM chains
- `PerpClient::get_balances(address) → (f64, U256)` — fetch USDC + ETH balance for one address via a single Multicall3 call (1 CU instead of 2)
- `PerpClient::get_balances_batch(addresses) → Vec<(f64, U256)>` — fetch USDC + ETH balances for N addresses via a single Multicall3 call (1 CU instead of 2N)
- `TransportConfigBuilder::read_endpoint()` — add a dedicated read endpoint
- `TransportConfigBuilder::write_endpoint()` — add a dedicated write endpoint
- `EndpointPool` — public type encapsulating a pool of endpoints with health-aware selection (`select`, `select_n`, `record_success`, `record_failure`, `healthy_count`, `len`)
- Re-exported `tick_to_price`, `price_to_tick`, `get_sqrt_ratio_at_tick`, `align_tick_down`, `align_tick_up` from crate root
- `PerpSnapshot` type — live market data: `mark_price`, `index_price`, `funding_rate_daily`, `open_interest`
- `GasEstimateCache` — caches `eth_estimateGas` results by function selector with configurable TTL and buffer
- `GasLimits::ETH_TRANSFER` constant (21,000 gas — protocol-defined invariant)
- `PerpClient::get_perp_snapshot(perp_id) → (PerpData, PerpSnapshot)` — fetch perp config and live market data via two-phase multicall (2 CUs instead of 5+). Phase 1 multicalls cfgs + mark + funding + OI (1 CU), phase 2 fetches index price from the beacon (1 CU)
- Anvil fork integration tests for batch balances and perp snapshot multicalls
- `PerpClient::set_base_fee(base_fee)` — inject a base fee from an external source (e.g. shared poller) without RPC calls
- `PerpClient::base_fee()` — read the current cached base fee (ignores TTL), intended for poller distribution
- `GasCache::base_fee()` — read the raw cached base fee
- `PerpClient::set_gas_ttl(ttl_ms)` — override gas cache TTL for externally-managed clients
- `GasCache::set_ttl(ttl_ms)` — override cache TTL
- Transport tracing: circuit breaker state transitions, write retry attempts/exhaustion, transport errors and timeouts now emit structured `tracing` events with endpoint URLs
- `tracing` crate added as a dependency (zero-cost when no subscriber is installed)
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

[Unreleased]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/releases/tag/v0.1.0
