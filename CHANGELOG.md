# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Breaking

- **`Side` moved from `types` to `math::capacity`**, and `ValidationError::NoBandCapacity` no longer carries one. The crate-root re-export is unchanged, so `perpcity_sdk::Side` and the prelude still work; only a direct `perpcity_sdk::types::Side` breaks. `Side` lives with the capacity math because that is what it keys, and because having it in the human surface made `errors` — the module everything else depends on — depend on a module above it. The error drops the field for the same reason: the caller passed the side in, so naming it back was both redundant and the thing holding the inverted edge in place.

### Fixed

- **`TransactionError::TooManyInFlight` is transient.** A full pipeline clears itself as receipts arrive, exactly as `NonceDesynced` does, but `is_transient` said otherwise — so a backoff loop gave up on the condition that resolves itself while retrying ones that do not. Nothing is signed or sent when it fires.

## [0.5.0] - 2026-10-02

A release about units. Every quantity the contract settles now carries its unit
as a type rather than as a name suffix, which is why the breaking list is long
and why none of it changes a value on the wire.

### Breaking

- **A read's block policy is the type it hangs off.** `MarketReader` reads the market as it is now: served from the cache within its TTL, or from the head at the moment of the call, each read on its own. `StateAt` reads it at one block: the handle resolves the header once, every read on it is by that hash, and two values read through one handle agree by construction. The three reads that pinned their own lagged block — capacity, margin ratios, and the pool — are now `StateAt::capacity()`, `StateAt::margin_ratios()` and `StateAt::pool()`; `get_capacity()`, `get_margin_ratios()` and `get_pool_snapshot()` remain as the single-read conveniences, each `state().await?.<read>()`. A caller that reads twice and needs the values to agree takes `state()` once. `get_pool_snapshot` is the renamed `load_taker_market_snapshot`.
- **The surface speaks of the market, not the perp.** `PerpData` is `MarketConfig` and `PerpSnapshot` is `MarketSnapshot`, joining `MarketReader`, `MarketCapacity`, `MarketEvent` and `MarketFeed`: `Perp` names the contract, the market is the concept, and what a caller reads is a market. The reads are named for what they return, so `get_perp_config` is `get_config` and `get_perp_snapshot` is `get_snapshot`. `get_perp_data` is removed: it returned a loose tuple of beacon address, tick spacing and pool price, had no caller in or outside the crate, and `get_config` returns all three in a type. `PerpClient` keeps its name, being a client of the contract.
- **The pool price is the pool's, the mark is the contract's.** Every read of `poolState().ammPrice` was named for the mark, which the contract computes as a different price (the fair price of the pool price, the index and the EMAs). `get_mark_price` is `get_pool_price`, the snapshot's `mark_price` and the configuration's `mark` are both `pool_price`, and the `StateCache` fast layer speaks of `pool_prices` (`get_pool_price`, `put_pool_price`). The contract's mark is `get_mark` (see *Added*).
- **The pool is a pool, not a book.** `TakerMarketSnapshot` is `PoolSnapshot`: the V4 pool at one block — its price, active liquidity, initialized ticks, and the bounds a taker swap runs within. Nothing else about the type changed.
- **`MarketSnapshot` carries its block as a `BlockContext`** (number, hash, timestamp): the lagged snapshot block every field was read at, so further reads can pin to it. **It carries the contract's mark**: `mark`, the fair price at that block, exact in X96 and converted once, and `emas`, the stored EMA pair as of the market's last touch, so a cache that follows the feed can keep marking at the contract's price between touches. **`MarketConfig` carries `ema_window`**, the seconds the contract smooths with. Struct literals must name the new fields; a `mark_price` downstream that held the pool price now has the number it meant to hold.
- **`math::ema` is gone; `PricePair` and `calculate_emas` live in `math::pricing`** beside `Mark` and the fair price, and are re-exported at the crate root. The pricing module states the model once: the pool price, the index, their contract-exact EMAs, and the mark as the fair price of all four. `exp_wad` is fixed-point arithmetic and is crate-private.
- **A pool whose tick map does not reproduce its active liquidity fails as `ContractError::StorageReadFailed`** (no source, not transient) rather than `MulticallFailed`. It was never a multicall failure, and at a pinned hash the mismatch is deterministic.
- **A maker's geometry is two types, and the maker math takes them.** `TickRange::new(lower, upper)?` is a tick interval valid by construction (`lower < upper`, both in the V4 domain; private fields, checked on deserialise), and `MakerBand { range, liquidity }` is a range holding liquidity — the shape `makerDetails` stores. `estimate_liquidity(&range, usd)`, `liquidity_for_target_ratio(margin, &range, sqrt_price, ratio)` and `liquidity_for_capacity(sqrt_price, &range, side, target)` take a `TickRange` instead of two loose ticks and no longer return `InvalidTickRange` — it can only come from `TickRange::new`; `band_capacity(sqrt_price, &band)` takes a `MakerBand`. `band_amounts(sqrt_price, &band)` is the typed form of `amounts_for_liquidity`. Callers build the range once at the boundary the ticks entered and pass it down.
- **`PriceImpactPoint` is removed.** Nothing in the SDK produced or consumed it; the local swap simulation (`PoolSnapshot::quote_to_price`) is the price-impact query.
- **`TransactionError::ReceiptTimeout` carries `tx_hash: FixedBytes<32>`.** The hash was only in `reason`, and a timeout whose last poll hit an RPC error left it out of the string too, so a caller could not look up the receipt. `reason` stays and now says only why polling stopped; `Display` reads `receipt timeout for 0x…: <reason>`. Patterns that use `..` are unaffected; exhaustive patterns and constructions must name the new field. `is_transient()` is unchanged (true), and the send path still never reuses the timed-out transaction's nonce.
- **A failed broadcast returns `TransactionError::BroadcastFailed { tx_hash, source }` instead of `PerpCityError::Rpc`.** The node can accept a transaction and still fail the request, so the outcome is unknown; the hash of the signed transaction lets the caller look it up. `source` is the same `TransportError` that `Rpc` carried. `is_transient()` is true for both, so retry loops are unchanged; code that matched `PerpCityError::Rpc` to detect a failed broadcast must match the new variant.
- **`close_taker(pos_id, urgency)` reads the delta it closes.** It took the caller's `f64` delta and scaled it to atoms, but the contract closes a taker only when the remaining perp delta is exactly zero: a delta tracked as `85.90838000000001` rounded one atom past the position, the close mined as `TakerAdjusted`, and the position stayed open with all its margin while the caller counted it closed. The `current_perp_delta` argument is gone; the call reads `positions(pos_id)` and reverses the perp amount to the atom. A close that still mines as an adjust (the position changed between the read and the block) fails with the new `TransactionError::TakerNotClosed { tx_hash, pos_id }`, which is transient and carries its hash.
- **`TransactionError::Reverted` carries `tx_hash: FixedBytes<32>`**, which was only in `reason`. Patterns that use `..` are unaffected; exhaustive patterns and constructions must name the new field. With `OutOfGas`, `ReceiptTimeout` and `BroadcastFailed`, every `TxBuilder::send` error that follows the broadcast now carries a typed hash.
- **`MAX_MAKER_EQUITY_BATCH` is `MAX_ROW_BATCH`.** The chunk size belongs to every batched row read on `StateAt`, not to the maker-equity batch alone. Same value, 500.
- **Every amount and price is its own type** (see *Added*), so the fields and signatures that carried a unit as a name suffix now carry it as a type and have lost the suffix. `PoolSnapshot.sqrt_price_x96` is `sqrt_price: SqrtPrice` and its four bound fields likewise; `TakerQuote`'s deltas are `PerpDelta` and `UsdcDelta` and its prices `SqrtPrice`; `Capacity` is `{ long, short }` of `PerpAtoms` with `on(side)` replacing `atoms(side)`, and `MarketCapacity` gains `open_interest(side)` and `headroom(side)` in place of the `_atoms` pair; `Mark` carries `pool_price` and `index` as `Price` and its mark is `fair_price()`; `MakerMarketSnapshot` carries `sqrt_price` and `mark`, `AccrualInputs` and `MakerState` carry `PerpAtoms` capacities and a typed margin and deltas. `MakerEquityBreakdown`'s twenty accessors become ten: `margin()`, `funding_owed()`, `lp_fees()`, `equity()` and the rest each return a `UsdcDelta` whose `usdc()` is the `f64` the `*_usd()` pair used to give, and `position_value()` returns a `UsdcAtoms` because a value cannot be negative. `get_sqrt_ratio_at_tick` returns a `SqrtPrice`, `get_tick_at_sqrt_ratio` takes one, and the sizing and capacity functions take typed amounts and prices throughout.
- **The exact `fairPrice` port is `fair_price` and the `f64` twin is `fair_price_f64`.** The exact one is the default and the lossy one is marked, which is the inverse of the old convention and the reason the suffix could leave the name.
- **The market's cumulative accumulators are four types**, and the `_x96`/`_x128` field names are gone: `Funding`, `FundingPerSqrtPrice`, `Earnings`, `FeeGrowth`. Twenty fields across `MakerMarketSnapshot`, `MakerState`, `TickFunding`, `CumulativesInfo` and `MarketEvent::TickInitialized` drop the suffix and take one of these, so `funding_x96` is `funding` and a read of it ends in `.x96()`. `maker_cuml_funding` returns the types instead of three bare `I256`, and `fee_growth_inside1` takes and returns `FeeGrowth`. Wire values are unchanged. A level is read only as `since(checkpoint)`, which is fallible on funding and earnings and infallible on fee growth, because Uniswap lets that word wrap.
- **Liquidity is `LUnits` and a change in it is `LDelta`.** `MakerBand`, `PoolSnapshot`, `TakerQuote`, `TickLiquidity`, `MakerState`, `OpenMakerParams::liquidity`, `AdjustMakerParams::liquidity_delta`, the two liquidity-bearing `MarketEvent` variants, `close_maker` and the four sizing functions all take or return them. Wire values are unchanged. **`estimate_liquidity` returns `LUnits`, not `U256`**: it handed back a width the pool cannot store and left callers to narrow it, which both examples did by clamping to 2^120 − 1; a size past `uint128` is now an `Overflow` error, as it is in `liquidity_for_target_ratio`. `LDelta` has checked methods and no operators, unlike the asset deltas, because nothing bounds a liquidity delta the way the accounting supply bounds a balance.
- **The ratios are `Ratio`**, the contract's `uint24` millionths, with `fraction()` as the human view. `Fees`'s four shares, `Bounds::liquidation_taker_ratio` and all of `MarginRatioTriple` hold one instead of an `f64`; `MarginRatioTriple::from_e6` and its three `*_e6` getters are gone, the struct now holding the integers rather than recovering them. `MakerState::liq_margin_ratio_e6` is `liquidation_margin_ratio: Ratio`, replacing two accessors on `MakerEquityBreakdown` with one. `liquidation_price` and `convert`'s leverage pair take and return one, and a `Ratio` checks the `uint24` domain on deserialisation too. **`is_liquidatable` takes a `Ratio`**: a liquidation's fee *rate* and the USDC it *settles* were both `f64` under the same name, and passing the amount made every position read as liquidatable. The five events carrying the amount now spell it `liquidation_fee`.
- **The two rates are `FundingRate` and `UtilizationRate`**, on `AccrualInputs`'s three `*_wad` fields. Two types rather than one because the contract's are `int88` signed and `uint64` unsigned, and one signed type would lose the fee legs' non-negativity.

### Fixed

- **The market snapshot pins to a block on the chain it reads.** It took its block from Multicall3's `blockAndAggregate`, whose `block.number` on Arbitrum is the L1 block number and whose `blockhash` of it is zero, so the beacon read pinned to a block that does not exist and every call failed with `BlockUnavailable` (every Legion centurion start, mainnet 2026-09-30). The snapshot now resolves the lagged snapshot block from the node's header, as `StateAt` and `get_mark` do, and pins the multicall and the index read to its hash. `MarketSnapshot.block` is that block, `SNAPSHOT_BLOCK_LAG` behind the head.
- **A request an endpoint declines no longer counts against its health,
  and a declined read is no longer retried.** `HftTransport` treated every
  non-200 as evidence the endpoint was failing, so a provider that answers
  a too-wide `eth_getLogs` with an HTTP client error — Alchemy returns
  `400` carrying `-32602 Log response size exceeded` — looked like an
  endpoint dying. Two consequences, both fixed: the read-retry loop resent
  the request twice for an answer that could not change, which tripled the
  cost of every rejected probe in the adaptive log scan; and three such
  answers opened the circuit, taking a healthy endpoint out of service for
  the recovery window. Since the retries counted separately, a single
  oversized request could open the circuit on its own, which meant a cold
  scan of a dense log range could not converge at all. The transport now
  distinguishes a request the endpoint declined (any client error but
  `401`, `403`, `429`) from a failure of the endpoint itself (no answer,
  timeout, rate limit, server error, auth refusal) and records only the
  latter. A narrowing rejection is the log scan's ordinary signal, so
  `history`'s width search can now narrow as often as it needs to.

### Changed

- **The maker-equity batch reads through `StateAt`.** It resolved its own lagged block and built its own mark from a nine-view multicall; it now takes the handle's block, the handle's mark, and the pool id from the market's immutables (two reads once per market per process, as `StateAt::pool` does). Same reads otherwise, same chunking, same golden vectors. Its market-wide and chunk failures are typed for the block like every other pinned read, so a replica behind the one that named the block fails as `BlockUnavailable` and a retry loop retries. One scoping is more precise than before: a chunk's fee-growth `extsload` failing now fails the chunk's open makers, not the ids its row multicall had already answered as not makers.
- **The design nodes are read from the repository, not from rustdoc.** Each module's doc links to its `DESIGN.md` on GitHub instead of inlining it, so the nodes' links point at files and design nodes rather than rustdoc paths. `cargo xtask design --check` resolves every name in every node against rustdoc's JSON, verifies each producer and consumer a type table claims against the real signature, and enforces the root node's invariants; `--open` draws the type graph the signatures give.
- **A design node answers the graph's questions in one of two sections, and which one is the answer.** The ratchet accepted a new island, dead end or two-cycle only if some node's *Debts* named it, so a shape the design keeps had to be filed as work owed. Nodes now also have *Accepted structure*, which the ratchet reads alongside the debts and `--report` counts as settled; a type is answered by the node that owns it, so two namesakes in two components stay two questions. The standing baseline is sorted into the two, leaving ten open: six bindings narrowed field by field, two surface types no row marks, and one pair of duplicated cache types.
- **The market snapshot is one block.** The batch and the beacon read were
  two calls at the head, so a trade between them could put the index one
  block after the pool state. The batch now runs as `blockAndAggregate`,
  the index is read at the block hash it reports, and the snapshot
  carries that block. A perp with no beacon is
  `ModuleNotRegistered` rather than a decode error from the zero address,
  and an index read that lands on a replica behind the one that ran the
  batch is `ContractError::BlockUnavailable` — transient, so a retry loop
  retries — rather than an opaque transport error. Every `StateAt` read
  types that answer the same way.
- **The pool snapshot is one multicall plus three pinned calls**, down
  from five separate calls and a raw storage read. The stored EMAs come
  from the `emas()` view rather than a hand-derived slot (verified equal
  on HORMUZ-TRAFFIC at block 510200629), the mark's inputs are the same
  `Mark` read the fair price and maker equity use, and the tick map's
  reconciliation is a pure function with its own tests. The deployment
  immutables are two reads on first load rather than three.
- **The event vocabulary moved to `events`, out from under `feeds`.**
  `MarketEvent`, `SwapInfo`, `MakerSettle`, `CumulativesInfo`,
  `decode_log` and `decode_raw` now live at `perpcity_sdk::events`:
  `feeds` streams the present tense and `history` replays the past, and
  both speak this one vocabulary, so it belongs under neither. The old
  path `feeds::events` still re-exports everything and `prelude` is
  unchanged, so no import breaks; new code should prefer the new path.
- **The two reported design invariants are gone.** They stood in for the unit types: no `f64` behind a wire-suffixed name, and a suffixed field holding the primitive its suffix named. With the last suffixed name typed they report nothing on any input, so `--report` no longer prints that section.

### Added

- **`units`: one type per unit the market is denominated in.** `UsdcAtoms` and `PerpAtoms` count the two assets' atoms, `UsdcDelta` and `PerpDelta` are their signed twins, `Price` is USDC per perp and `SqrtPrice` its square root. Each is `repr(transparent)` and transparent to serde, so a persisted or logged value still reads back as a bare number, and each carries only the operations its quantity admits. There are exactly two crossings, `PerpAtoms::value_at(Price)` and `SqrtPrice::squared()`, which are the two mistakes the module exists to prevent: both assets are six-decimal `u128` and both prices are Q96 `U256`, so each pair used to be one type with two field names. The naming rule: a unit of account carries its unit (`UsdcAtoms`) because the integer *is* the quantity, an encoding does not (`Price`, not `PriceX96`) because the accessor names it (`x96`). The Solidity-compatible fixed-point arithmetic moves here from `math`, still crate-private. `src/units/DESIGN.md` argues the whole of it.
- **`StateAt::positions(&[U256])`**: the raw contract state of many positions, one row multicall per chunk of `MAX_ROW_BATCH` ids, all at the handle's block. Every id comes back as a `RowOutcome<Position>` in input order: the row, `None` for an id the contract holds no row for (as `StateAt::position` reads it), or that id's own error. A row that reverts fails alone; a chunk whose multicall fails marks its ids with the shared cause, typed for the block (`BlockUnavailable` stays transient, `StateUnavailable` stays not) and the other chunks stand. Nothing fails the batch, so a sweep over `1..next_pos_id()` gets an answer for every id.
- **`StateAt::maker_equities(&[U256])` and `StateAt::maker_equities_at_mark`**: the maker-equity batch at the handle's block, so a caller can preview settles at a block it names. `get_maker_equities` and `get_maker_equities_at_mark` remain as the conveniences, each `state().await?.<read>()`, with the same signature and the same `MakerEquityOutcome` shape.
- **`Emas`: the stored EMA pair in human units with the touch it is current as of**, the f64 twin of `PricePair` at a `last_touch`. `Emas::advanced(pool_price, index, timestamp, ema_window)` moves it to a later time by the contract's exponential, and `Emas::mark(...)` is `fair_price` of the spot prices and the advanced pair: the contract's mark at that time. `MarketSnapshot` carries the stored pair and `RatesAndEmasRefreshed` carries each new one, so a cache that follows the feed holds one `Emas`, replaces it on every touch, and marks between touches with the prices it already tracks.
- **`StateAt`: a market's storage, every read pinned to one block.**
  `MarketReader::state()` pins the lagged snapshot block and
  `state_at(number)` a block the caller names; both resolve the header
  first, so an absent block is `ContractError::BlockUnavailable` and never
  a fall-back to the head. Every read on the handle — `solvency()`,
  `next_pos_id()`, `position(id)` (`None` for a closed or never-minted id,
  where `get_position` errs), `maker_band(id)`, `pool_tick()`,
  `collateral()` — is by that block's hash, and `block()` says which. It
  is the state half of what `History` is for logs, and the first public
  block-pinned read. A full (non-archive) endpoint keeps the header and
  prunes the state, so it hands out the handle and each read then fails
  with the new `ContractError::StateUnavailable`, which `is_transient()`
  refuses: an archive endpoint is the fix, not a retry.
- **`StateAt::capacity()`, `margin_ratios()`, `pool()` and `mark()`** —
  the market's taker capacity with the open interest drawing on it, the
  `IMarginRatios` thresholds, the pool a taker swap is quoted against, and
  what the contract marks from, each at the handle's block (see
  *Breaking*).
- **`SolvencyState { bad_debt, total_margin }`**, the contract's own
  struct in USDC, and **`convert::usdc_from_atoms`**, the one checked
  widening from a `uint128` or `uint256` into the `i128` that
  `scale_from_6dec` takes.
- **`history::OwnershipLog` and `history::ChainPoint`** — who held each
  of a market's positions, over time, folded from a tape:
  `OwnershipLog::fold(&tape)` walks the
  `MarketEvent::PositionTransferred` events and answers
  `owner_at(pos_id, ChainPoint)` (custody when an event happened — the
  attribution a measurement over past events wants, since a position
  handed between wallets mid-life has more than one owner),
  `latest_owner(pos_id)` (its final holder, for a caller that wants one
  address per position), plus `transfers`, `positions` and `len`. A mint
  is a transfer from the zero address and a burn is a transfer to it, so
  custody is `None` before the mint and after the burn. `ChainPoint
  { block, log_index }` is the chain's total order — two logs in one
  block still compare — and `TapeEvent::point()` returns an event's.
  Trade events carry a position id and never a wallet, so this fold is
  how a market's activity is attributed to the addresses behind it.
- **The market-event tape.** `history::market_events(provider, perp,
  from_block, to_block)` replays every event a perp emitted, in chain
  order, and `history::latest_market_events(.., limit)` the newest
  `limit`, reading backward (skipped logs do not count against the
  limit). Each row is a `TapeEvent { block_number, log_index, timestamp,
  tx_hash, event: MarketEvent }` — the same `MarketEvent` the live feed
  streams, decoded by the same `feeds::events::decode_log`, so replayed
  history and live subscription carry one vocabulary. Logs the decoder
  does not recognize (ERC-721 approvals, admin events) are skipped, as
  the live feed skips them. The tape includes
  `MarketEvent::PositionTransferred` — the position NFT's mint, burn and
  mid-life transfers — so a position id maps to its owner over time
  without a separate reader. `examples/tape.rs` is the worked example
  (read-only; no signer).
- **`history::History<P: Provider>`** — a handle over the historical
  readers that owns what one-shot calls cannot: the block-lag policy, the
  learned request width, and the concurrency budget. Every method takes
  `to_block: Option<u64>`; `None` reads to the head minus the handle's
  lag (`SNAPSHOT_BLOCK_LAG` blocks, or `with_lag`), so a lagging replica
  is never asked for a block whose logs it may not hold yet — previously
  each caller invented its own policy. The learned `eth_getLogs` width
  persists across the handle's scans, so a process that scans repeatedly
  pays the width search once instead of per call. Bulk scans keep up to
  `DEFAULT_IN_FLIGHT` (4, or `with_in_flight`) window requests
  outstanding, carved at the shared learned width and delivered in range
  order, so a latency-bound backfill pays one round trip per batch of
  windows instead of one per request; a window a provider rejects narrows
  its own requests without disturbing its siblings. Newest-first reads
  (`latest_*`) stay sequential on every path — they exist to stop early,
  and a request below the stopping point is waste. Built from any
  `Provider` (reading history needs no signer); `PerpClient::history()`
  wraps the client's own provider. The free functions are unchanged:
  sequential, learning per call.
- **`history::ScanStats` and `History::stats()`** — cumulative counters
  over every `eth_getLogs` request a handle's scans have sent: requests,
  rejections, logs returned, total time awaiting answers (summed across
  concurrent requests, so it can exceed wall time), and the width the
  search currently believes. Counted where the scan workers already
  synchronize, so it costs nothing measurable; no callbacks, no tracing
  dependency. Sample before and after a stretch of work and diff to
  meter it — a long-lived collector's answer to "is the provider
  degrading, did the learned width collapse, how much budget goes to
  rejections". The free functions report nothing: their width search,
  and these counters with it, live only for the call.
- **`history::test_support` under the new `test-utils` feature** — the
  in-memory JSON-RPC node the history readers' own tests run against
  (`FakeNode`: serves `eth_getLogs` from a fixed log set, rejects ranges
  the way a capped provider does, records every requested range), public
  so a crate building on the readers can test its scans against the same
  node. The `history` module is now a directory (`scan`, `beacon`,
  `transfers` submodules); its public API is unchanged and re-exported
  from `history` as before.
- **`TransactionError::tx_hash()`** — the signed transaction's hash for every failure from the broadcast onward (`BroadcastFailed`, `ReceiptTimeout`, `Reverted`, `OutOfGas`), `None` when nothing was sent. A caller with an unknown outcome reconciles by receipt instead of waiting out a fixed window.
- **`PerpCityError::tx_hash()`** — the same hash on the top-level error, `None` for every other variant. For an error from `TxBuilder::send`, `None` means nothing was broadcast: after this release an `Rpc` error from a send always comes before the broadcast.
- **`PerpClient::poll_receipt(tx_hash)` is public** — the send path's receipt wait without its initial 2 s delay (a hash to reconcile is rarely fresh): it polls at once, then every 2 s for up to 30 s, returning `ReceiptTimeout` for the same hash on timeout so the call can repeat. It does not touch nonce tracking: a send that returned a hash has already stopped tracking it, and a doubtful nonce resyncs from chain before the next send either way.
- **`constants::RECEIPT_TIMEOUT` (30 s) and `constants::RECEIPT_POLL_INTERVAL` (2 s)** — the receipt wait's bounds, public so a caller can size its own deadline around `poll_receipt`.
- **`history::get_logs_chunked(provider, filter, from_block, to_block)`** — every log matching a filter across a block range of any length, in chain order. Providers cap `eth_getLogs` by span, result count or response size and word the rejection differently, so the scan does not parse range messages: it halves a range the server rejects, doubles the span after each accepted range, and narrows in on the limit between the widest accepted and narrowest rejected span. A rejection of a span accepted before (a result cap in a dense stretch) drops what was learned, and a rejected span is retested after a run of accepted requests, so the scan widens again past a dense stretch. Failures no narrower range fixes are returned at once: no answer, a rate limit (HTTP 429/503, or a JSON-RPC error alloy's retry rules call one; `-32005` only when its message says so), a method or parse error (`-32601`, `-32700`), and HTTP 401/403. A single block that is still rejected, and a method, parse or auth refusal, is returned as `ContractError::LogsRejected`.
- **Beacon print series.** `history::beacon_prints(provider, beacon, from_block, to_block)` returns every `IndexUpdated` print in a range, and `history::latest_beacon_prints(provider, beacon, from_block, to_block, limit)` the newest `limit`, reading backward from `to_block` and stopping once it has them (so `from_block` is only a floor). Both return `IndexPrint { block_number, log_index, timestamp, index_x96 }` oldest first, with `index()` for the float value. Timestamps come from the log's `blockTimestamp` when the provider sends it; otherwise each distinct block's header is read once, with bounded concurrency. A print log that does not decode is `ValidationError::DecodeFailed`, not a silent gap. The deployed beacons have no last-update getter (`index()` returns the value alone), so `latest_beacon_prints(.., 1)` is how to read when a beacon last printed.
- **`history::token_transfers(provider, token, senders, recipients, from_block, to_block)`** — every ERC-20 `Transfer` of `token` whose sender is in `senders` and whose recipient is in `recipients` (each an `Option<&[Address]>`), in chain order, read with `get_logs_chunked`. The sets go to the node as topic filters, in one scan; a set of more than 1,000 addresses (geth's and so Nitro's per-position limit) is `ValidationError::InvalidConfig` before any request. `None` matches any address and `Some(&[])` matches none, so a set that comes out empty reads nothing (with no request) rather than every transfer; two `None` sets, or the zero token, are `ValidationError::InvalidConfig`, since the query would read every transfer the token made. Each result is a `TokenTransfer { block_number, log_index, tx_hash, from, to, value }`; `value` is raw token units. A log that does not decode as an ERC-20 `Transfer` (an ERC-721 `Transfer` shares the topic) is `ValidationError::DecodeFailed`, not a silent gap. Golden-tested against a real Arbitrum One USDC transfer. Two calls read every transfer between one address and a set of others, both ways.
- **`ContractError::LogsRejected { from_block, to_block, source }`** (new variant on the `#[non_exhaustive]` enum) — the server refused an `eth_getLogs` request that no narrower range fixes. Not `is_transient()`, so a retry loop keyed on it stops; rate limits and unanswered requests stay transient `PerpCityError::Rpc`.
- **`ValidationError::InvalidBlockRange { from_block, to_block }`** (new variant on the `#[non_exhaustive]` enum) — a range whose start is after its end. Not `is_transient()`.
- **`math::capacity`** — the taker capacity a maker band adds, exact to the deployed `PerpLogic.calcCapacity`, so strategy code does not port contract math. `band_capacity(sqrt_price_x96, tick_lower, tick_upper, liquidity)` returns a `Capacity { long_atoms, short_atoms }` (`atoms(side)` for one side; 6-decimal perp atoms; the band above the price backs longs, the band below backs shorts). `liquidity_for_capacity(sqrt_price_x96, tick_lower, tick_upper, side, target_atoms)` is the exact inverse: the least liquidity that reaches the target on one side. Both are re-exported at the crate root and in the prelude. Golden vectors: three HORMUZ-TRAFFIC maker opens on Arbitrum One (two bands across the price, one band below it) reproduce the stored `makerDetails(id).capacity` and the `capacity()` step across each open to the atom.
- **`MarketReader::get_capacity`** → `MarketCapacity { block, capacity, long_open_interest_atoms, short_open_interest_atoms }`: `capacity()` and `openInterest()` in one multicall, pinned to the lagged snapshot block (`StateAt::capacity()` for a block of the caller's choosing). `headroom_atoms(side)` is the open interest a side can still add before `Long/ShortUtilizationExceeded`; `utilization_e6(side)` is the value the Perp passes to the fees module (`oi · 1e6 / capacity`, floored), or `None` for a side with no capacity, where the contract passes `type(uint24).max`. Closes the "no way to get utilization" gap (#4).
- **`MarketReader::get_mark`** → **`Mark`**: what the contract marks from at the lagged snapshot block — `amm_price_x96`, `index_x96`, and the EMAs advanced to its timestamp — with `fair_price_x96()` the price it marks at (the deployed `fairPrice`). `StateAt::mark()` is the same read at a block of the caller's choosing; `get_maker_equities` prices at it, and `get_pool_price` keeps returning the pool price. `Mark::advanced` reproduces a live HORMUZ-TRAFFIC accrue to the digit (block 510213600, 300 s after the last touch). `Mark` and `MarketCapacity` are re-exported at the crate root and in the prelude.
- **`types::Side`** (`Long` / `Short`), re-exported at the crate root and in the prelude.
- **`ValidationError::NoBandCapacity { lower, upper, side }`** (new variant on the `#[non_exhaustive]` enum): a capacity target on a side the band cannot back at the price. Not `is_transient()`.
- **`math::liquidity::amounts_for_liquidity`** is public and re-exported at the crate root and in the prelude: Uniswap `LiquidityAmounts.getAmountsForLiquidity`, the `(perp atoms, USDC atoms)` a band's liquidity holds at a price, rounded down. It was the crate-internal helper behind the maker-equity valuation; it now also rejects a zero sqrt price or bound with `ValidationError::InvalidPrice`.

## [0.4.0] - 2026-09-08

### Breaking

- **Maker equities are priced at the deployed fair price, not the pool price.** `get_maker_equities` pinned `mark_price_x96` to `poolState().ammPrice`; the contract never prices there. `PerpLogic.accrue` sets `markPrice = pricing.fairPrice(ammPrice, index, emaAmmPrice, emaIndex)` with the EMAs advanced to the block, and every `valPnl`, health check, liquidation test and utilization accrual uses that mark — so a maker guard built on `is_liquidatable` disagreed with the chain whenever the EMA basis was open. The market-wide read now also takes `modules()`, `emas()`, `EMA_WINDOW()` and the beacon's `index()` at the pinned block and computes the contract's mark (`math::pricing::fair_price_x96`). Consumers see `position_value`, `unrealized_pnl`, `margin_ratio` and `is_liquidatable` move to the chain's numbers. `get_maker_equities_at_mark` keeps its role as the what-if override (the caller's mark applied post-replay), but it shares the new read path: its accrual replay also moves to the fair mark, and it now requires a registered beacon and successful beacon/EMA reads where it previously needed none. A failed beacon read fails the call, like any other market-wide read, and a perp with no beacon registered fails with `ContractError::ModuleNotRegistered` rather than an opaque ABI-decode error.

- **`MarketEvent::TakerClosed`, `MakerClosed` and `MakerConverted` carry the deployed liquidation tails**: new fields `liq_fee: f64` (USDC) and `is_liquidation: bool`. `Perp::TakerClosed` and the deployed-era `PerpDeployedEvents::MakerClosed`/`MakerConverted` declare them. The untailed maker shapes (contracts-repo main since #171, deployed on no live market) decode them as `0.0` / `false`; the taker binding is deployed-era only — the SDK targets the deployed contracts, and the untailed `TakerClosed` is deliberately not bound. Consumers destructuring with `..` are unaffected; exhaustive patterns must name the new fields.

### Added

- **`math::pricing`** — the deployed pricing module's `fairPrice` as `fair_price_x96` (exact X96, Solady overflow-safe average) and `fair_price` (f64, for simulators); both re-exported at the crate root and in `prelude`. Golden vectors from an `eth_call` to the live module (Arbitrum One, 2026-09-07).
- **`Perp::emas()`** binding (`0x6ab80a34`), the stored EMA pair as of `rates().lastTouch`; `math::ema::PricePair::try_from_x96` narrows X96 `uint256` observations to the contract's `uint128` pair with a typed overflow error.
- **`PerpClient::get_margin_ratios`** → `MarginRatios { maker, taker }` of `MarginRatioTriple { init, liquidation, backstop }` (fractions; `from_e6` / `*_e6()` for the on-chain `uint24` values). Previously only the taker liquidation ratio was reachable, through `Bounds`.
- **`MarketEvent::ModifyLiquidity`** (Uniswap V4 PoolManager, `salt == posId` for perp pools; declared on `IPoolManagerState`) and **`MarketEvent::PositionTransferred`** (the Perp's ERC721 `Transfer`; a mint has `from == Address::ZERO`). An ERC20-shaped `Transfer` log, which shares the topic0, returns `None`. `MarketFeed` is unaffected — it filters by Perp + beacon address, so the PoolManager log only arrives to a consumer that subscribes to it.
- **`PerpClient::transfer_position(to, pos_id, urgency)`** — hands an open position to another account via the Perp's ERC721 `safeTransferFrom`. A position owns its margin and its whole accrual history, so the recipient inherits the live position: nothing settles, no margin moves, and the new owner adjusts or closes it exactly as the sender could have. Ownership is read before broadcasting (`ownerOf`), so calling it for someone else's position returns the new **`ContractError::PositionNotOwned { pos_id, owner, caller }`** instead of burning a transaction on the contract's `TransferFromIncorrectOwner`. The zero address and the sender's own address are rejected as recipients. `safeTransferFrom` rather than plain `transferFrom`: the SDK cannot know the destination is an EOA, and the receiver check is the difference between a rejected transfer and a position stranded in a contract with no code path to adjust it. The binding and Solady's ERC721 error set were probed on the deployed HORMUZ-TRAFFIC market (0x137E0048, Arbitrum One, 2026-09-08) — a non-owner `safeTransferFrom` returns `TransferFromIncorrectOwner()` where an undefined selector returns empty revert data — and the selector is locked in `abi_lock`. A send surfaces as `MarketEvent::PositionTransferred` on the feed.

- **`TransactionError::OutOfGas { tx_hash, gas_used, gas_limit }`** (new variant on the `#[non_exhaustive]` enum) — a broadcast transaction that was mined having consumed its gas limit with no revert data. Distinct from `Reverted`: the call was never disproved, only the limit was too small. Not `is_transient()` — the send path evicts the cached estimate before returning, so a retry re-estimates rather than repeating the limit, but whether to retry is the caller's decision.

### Fixed

- **Cached gas estimates are floored at the `GasLimits` constants for the Perp entrypoints.** `GasLimitCache` keys estimates by 4-byte selector, so one cheap `adjustTaker`/`adjustMaker` seeds the limit an expensive tick-crossing one inherits, and Arbitrum charges execution costs `eth_estimateGas` does not model — leaving the cache able to produce a limit below the constant it replaced. `eth_call` does not reproduce the shortfall, so the capped pre-flight in `simulate()` could not catch it. The constants are now a lower bound on the estimate rather than a fallback it can undercut.
- **A mined out-of-gas evicts its cached estimate.** Previously only a failed pre-flight evicted, so an estimate that passed simulation and failed on-chain kept killing every send for that selector until the 1-hour TTL expired.

## [0.3.0] - 2026-09-02

### Breaking

- **`Perp::liquidateMaker` / `liquidateTaker` bindings target the deployed 2-argument selectors** (`(posId, liquidationFeeRecipient)`) — bindings match deployed bytecode, not contracts-repo HEAD.
- **`TakerMarketSnapshot` block fields moved into `snapshot.block`** (`block_number`/`block_hash`/`block_timestamp` → `BlockContext`), and `TakerQuote`'s flat `block_number`/`block_hash` likewise became `quote.block: BlockContext`.
- **`ContractError`, `ValidationError`, and `TransactionError` are `#[non_exhaustive]`** (matching `PerpCityError`), and `TransactionError::SimulationReverted { selector }` is now a `FixedBytes<4>` (was a hex `String`; it still displays as `0x…`). Unknown selectors now decode to `"UnknownContractError(0x…)"` with the selector preserved instead of one collapsed name.
- **New `ContractError` variants** on the previously exhaustive enum: `BlockUnavailable` (a pinned header missing from a lagging replica is a failed read, never a silent degrade) and `StorageReadFailed` (typed raw-storage failures carrying the transport error as `source`). Both classify as `is_transient()` when a transport cause is present.
- **`TransactionError::SimulationFailed { reason }`** (new variant on the `#[non_exhaustive]` enum): a pre-flight `eth_call`/`eth_estimateGas` the node answered with an execution failure but no decodable revert — an empty revert (a selector the deployed contract lacks, a bare `revert()`) or out-of-gas inside the pinned limit. It is **not** `is_transient()`; `GasUnavailable` now means only that the simulation got no answer (transport failure, rate limit) and stays transient. Code that retried on `is_transient()` no longer loops on an empty revert.
- **`MakerState` gained `liq_margin_ratio_e6`** (`positions(id).liqMarginRatio`, 1e6-scaled) so the settle preview can carry the position's own liquidation threshold. Struct-literal constructors must supply it.
- **The storage-slot module (`crate::storage`, a sibling of `contracts`) and `math::maker_equity::fee_growth_inside1` are crate-internal.** The storage-slot fns encode deployed contract layouts, which are not semver surface; `convert::unpack_balance_delta` stays public.

### Added

- **`prelude` module.** `use perpcity_sdk::prelude::*;` bundles the everyday public surface (client/transport, well-known chain ids/addresses, errors, gas/urgency, feeds, client-facing params/results, maker-equity types, liquidity sizing, tick/price conversion) behind one import. Every item is also still available individually at the crate root; lower-level ABI/contract-interface types and the fine-grained `math::swap`/`convert` helpers are deliberately not included.
- `math::liquidity::{estimate_liquidity, liquidity_for_target_ratio}` are now re-exported at the crate root (and in `prelude`), matching `math::tick`/`math::swap`'s existing top-level exposure.
- **Maker-equity primitives.** `PerpClient::get_maker_equities(pos_ids)` computes, pinned to one lagged block, the exact settle preview for every open maker position in the batch: margin, accrued range funding, utilization earnings, uncollected V4 LP fees, and inventory PnL — priced at the pinned `poolState().ammPrice` mark (`get_maker_equities_at_mark` for what-if X96 marks). Reads are batched (one market multicall, chunked row multicalls, one PoolManager `extsload` with distinct band ticks read once, one `eth_getProof` for the Perp tick-funding slots with an `eth_getStorageAt` fallback); failures degrade per position, surfaced as `MakerEquityKind::Failed` on that id's `MakerEquityOutcome`. The result is exactly one `MakerEquityOutcome` per input id in input order, its `kind` one of `Computed(MakerEquityBreakdown)`, `NotAMaker`, or `Failed(PerpCityError)`; inputs above the public `MAX_MAKER_EQUITY_BATCH` (500) are chunked internally, all chunks pinned to one block. The math lives in the new `math::maker_equity` module — `MakerMarketSnapshot` (whose consuming `accrued()` replay of `PerpLogic.accrue` returns an `AccruedMakerSnapshot`, the only type that can compute equities; `with_mark` reprices it for what-if marks), `MakerState`, `AccrualInputs`, `TickFunding`, and `MakerEquityBreakdown`, exact signed 6-decimal atoms behind `*_atoms()` getters (funding as `funding_owed_atoms()`, positive = position pays) with `*_usd()`/`equity()`/`settled_margin()`/`accrued_income()` accessors; deserialization enforces the per-component bound `MAX_COMPONENT_ATOMS` so the i128-sum invariant holds. Validated end-to-end against the CHINA-PC pos-54 on-chain liquidation settle. The breakdown also carries the contract's health inputs — `position_value_atoms()`/`position_value_usd()` (`posVal`, the band's liquidity value at the mark), `liq_margin_ratio_e6()`/`liq_margin_ratio()` (the ratio stored on the position), `margin_ratio()` (equity over value as `PerpLogic.isHealthy` computes it), and `is_liquidatable(liquidation_fee)` mirroring `isHealthy(equity − posVal·liqFee, posVal, liqMarginRatio)` as a screening gate. Batches above one chunk read their chunks concurrently (bounded), and within a chunk the fee-growth `extsload` and the tick-funding read run concurrently. New `examples/maker_equity.rs` (dry-run by default) screens with `is_liquidatable` and confirms with the contract.
- **Liquidations.** `PerpClient::liquidate_maker` / `liquidate_taker` send with the fixed `GasLimits::LIQUIDATE` bound and an `Urgency`; `simulate_liquidate_maker` / `simulate_liquidate_taker` gate on the contract's own health check via `eth_call` from the client's address at the same pinned gas limit, decoding reverts into `TransactionError::SimulationReverted { error_name, .. }` so callers can tell `NotLiquidatable` (retry) from `NonMakerPosition` (drop) from transport failures (transient). The zero address is rejected as a fee recipient (it would burn the liquidation reward).
- `math::BlockContext { number, hash, timestamp }` — the shared block reference embedded in both `TakerMarketSnapshot` and `MakerMarketSnapshot`.
- `constants::SNAPSHOT_BLOCK_LAG` — the shared 8-block lag both snapshot loaders pin behind the head.
- `TransactionError::is_revert::<E: SolError>()` — typed matching of decoded simulation reverts by 4-byte selector (`err.is_revert::<Perp::NotLiquidatable>()`), replacing `error_name` string comparison.
- `futures-util` is now a direct dependency (already in the tree via alloy): the tick-funding fallback uses bounded buffered streaming.
- **`aws` feature — AWS KMS signing.** Enables alloy's `signer-aws` so bots can sign with a KMS asymmetric secp256k1 key that never leaves AWS (`alloy::signers::aws::AwsSigner`). New `examples/aws_kms_signer.rs` shows the full flow: AWS credential chain → KMS client → `AwsSigner` → `PerpClient`.
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

### Changed

- **An explicit `TxBuilder::with_gas_limit` now always preflights — at the pinned limit.** Fixed-gas sends previously skipped simulation entirely; they now run the `eth_call` preflight with the pinned gas limit attached, so both would-be reverts and out-of-gas-at-the-limit fail before broadcast (the gas-cache-hit path likewise preflights at the cached limit). Consequently `liquidate_maker`/`liquidate_taker` are documented as directly callable in the liquidation race — reverts surface pre-broadcast at no gas cost — with `simulate_liquidate_*` repositioned as the batch/scanner probes.
- `load_taker_market_snapshot` now pins `SNAPSHOT_BLOCK_LAG` blocks behind the head — the same stale-replica policy as the maker-equity reads — and returns `BlockUnavailable` instead of an untyped error when the header is missing.
- **Generic signer support.** `PerpClient::new`, `new_arbitrum`, and `new_arbitrum_sepolia` now accept any `S: TxSigner<Signature> + Send + Sync + 'static` instead of a concrete `PrivateKeySigner`. Existing callers compile unchanged (`PrivateKeySigner` satisfies the bound); remote signers (AWS KMS, GCP, Ledger, …) can now be used directly.

- **Error restructuring.** The monolithic `PerpCityError` enum is now composed from per-module error types: `TransactionError` (simulation, signing, gas, pipeline), `ValidationError` (prices, margins, ticks, leverage, config), and `ContractError` (perps, positions, events, quotes, multicall). `PerpCityError` composes all three via `#[from]` conversions. Internal functions return the narrowest error type they can produce (e.g. `pipeline::prepare()` returns `Result<_, TransactionError>`).
- **Pre-flight simulation.** `simulate()` replaces `resolve_gas_limit()` as the unified entry point for gas resolution and transaction validation. On cache miss, `eth_estimateGas` provides both the gas estimate and simulation. On cache hit, `eth_call` verifies the transaction is still valid before broadcast. Every code path guarantees the transaction has been simulated — no more on-chain reverts from warm gas cache.
- **Transaction builder.** `send_tx()` and `send_tx_with_value()` replaced by `TxBuilder`, created via `PerpClient::tx(to, calldata)`. Optional setters: `.with_value()`, `.with_gas_limit()`, `.with_urgency()`. Defaults: `value = 0`, `gas_limit = None` (triggers simulation), `urgency = Normal`. Adding new transaction parameters no longer changes existing call sites.
- **Tracing discipline.** All SDK logging demoted from `info` to `debug`. The SDK is a library — consumers own the `info`-level narrative. SDK logs are for infrastructure debugging via `RUST_LOG=perpcity_sdk=debug`. Warnings and errors unchanged.
- **Client module restructured.** `client.rs` (1500+ lines) split into `client/mod.rs`, `client/trades.rs`, `client/queries.rs`, `client/transactions.rs`; the maker-equity batch read later split out of `queries.rs` into `client/maker_equity.rs` the same way. Each submodule implements `impl PerpClient` for its methods.
- **Manual receipt polling.** Replaced Alloy's `pending.get_receipt()` with direct `get_transaction_receipt` polling (2s initial delay, 2s interval, 30s timeout). Avoids triggering Alloy's background `eth_blockNumber` poller that persists for the provider's lifetime. Retries on transient RPC errors during polling.

### Fixed

- **Burned-token classification in `get_positions_by_owner` is provider-dependent no more.** `ownerOf` reverts for burned positions; some RPC providers wrap that revert as a raw JSON-RPC error response, which the scan classified as a transport failure and propagated — aborting the enumeration on any market with a burned position. Reverts are now recognized by their revert data regardless of how the provider encodes them; only genuine transport failures propagate.
- **ETH transfer gas on Arbitrum.** `transfer_eth` hardcoded `GasLimits::ETH_TRANSFER` (21,000), the Ethereum L1 intrinsic. Arbitrum folds an L1 data component into intrinsic gas, so the node rejected these transfers as "intrinsic gas too low", breaking cohort wallet funding. Transfers now estimate gas via `eth_estimateGas` (like other operations); `simulate()` handles the empty calldata of a plain transfer. `GasLimits::ETH_TRANSFER` remains as a reference value.
- **In-flight transaction eviction.** Failed `poll_receipt` and on-chain reverts now evict the transaction from the pipeline's in-flight map, preventing permanent slot consumption. Previously, 16 failed transactions would jam the pipeline and block all new transactions including closes and retreats.
- **Stale cached gas estimates self-heal.** A cached per-selector gas limit that had gone too small failed the capped preflight on every send until its TTL expired; a preflight the node fails without a contract revert (`SimulationFailed`) now evicts the estimate and re-estimates. A preflight that never reached the node (a timeout, a rate limit) keeps the cached estimate and surfaces the transient error instead of paying for a re-estimate.
- **`MakerMarketSnapshot::accrued` rejects an accrual target behind `last_touch`** (`ValidationError::InvalidConfig`) instead of silently returning the un-accrued snapshot — the chain cannot touch a market after the snapshot block, so the inputs came from different blocks.

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

[Unreleased]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/releases/tag/v0.1.0
