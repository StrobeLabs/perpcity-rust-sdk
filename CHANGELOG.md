# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

The changes below break the public API, so the next release is 0.12.0 (a minor bump, as for any breaking change before 1.0). The tape is a type: every reader returned a vector whose chain order every fold assumed and nothing checked. And a taker's health is exact: the deployed liquidation test ported, read in a batch at one block, and the replay keeping the leg the test prices. And a maker's distance to liquidation is a search along a price shock, since its equity along that path is a curve, not a line.

### Breaking

- **The readers return a `Tape`.** `market_events`, `market_tape`, `latest_market_events`, their `History` counterparts and `Recording::tape` return `history::Tape` in place of `Vec<TapeEvent>`. A `Tape` derefs to `TapeSlice` and on to `[TapeEvent]`, so a caller that indexed, sliced, iterated or folded the vector is unchanged; a caller that stored it in a `Vec<TapeEvent>` field takes `Tape`, or `into_vec()`.

- **`PositionKind::Taker` carries the USD leg.** The variant gains `usd: UsdcDelta`, the sum of its swaps' `usd_delta` beside `moved`, the sum of their `perp_delta`; a match that spells the variant's fields names it or takes `..`. `PositionState::taker_usd` reads it as `taker_size` reads the size, `None` exactly when the size is, and a seed fills it from the row's `amount1`.

- **`MakerEquityBreakdown::is_liquidatable` takes nothing.** It took the liquidation fee rate and deducted it from equity before the ratio, which is the legacy build's rule. The deployed contracts, `v0.2.2-upgradeable`, test `isHealthy(equity, posVal, liqMarginRatio)` and charge the fee only after the test passes, so the argument goes and with it a verdict that called positions liquidatable a fee's width too early. A caller that passed the rate drops the argument. The breakdown no longer implements `Default`, since it now carries a mark and a band, which have none, and a breakdown serialized by 0.11 does not deserialize: it lacks the prices and legs a revaluation needs, and a deserialized breakdown must reproduce its stored value and PnL from them.

- **`MakerEquityKind::Computed` boxes its breakdown.** The breakdown grew the prices and legs its distance search revalues, and most ids in a sweep are not makers, so an inline breakdown made every outcome its size. A match on `Computed(b)` reads the breakdown through the box unchanged; a caller that stores a `&MakerEquityBreakdown` from it takes `b.as_ref()`.

### Added

- **The maker's distance to liquidation.** A maker is liquidated along a price shock, takers trading the pool and the mark following, and along that path its equity is the LP curve, concave, so the price where the test turns is searched for rather than solved. The shock moves the pool price and the mark by one factor from where each stood, so the gap between them carries through and the search starts exactly at the preview. `MakerEquityBreakdown` keeps the mark and pool price it was priced at and the legs that move with them, and revalues itself with nothing else in hand: `equity_at`, `position_value_at` and `is_liquidatable_at` at any mark, the preview itself at its own, and `liquidation_prices`, a geometric bracket outward from the mark in each direction then a bisection to the Q96 atom. The answer is a `math::LiquidationPrices`, the one shape both roles answer "how far" in, so a reading never branches on which it holds: the `mark` it was measured from, `below` and `above`, `None` for a side no move within reach turns, both the mark when the position is liquidatable now, and `distance()`, the nearer side as a fraction of the mark. The search runs only when asked: per side, up to seventeen bracket steps, then a bisection to the atom whose length is the logarithm of the bracket's width, each step one valuation of the two price legs.

- **`math::taker`, the taker's health exact.** `TakerMarketSnapshot::taker_health(&TakerState)` is `liquidateTaker`'s eligibility test as the deployed contracts, `v0.2.2-upgradeable`, run it: the position's value and PnL at the mark, the funding and utilization owed since its checkpoints, its settled margin and equity, all in exact atoms, and `TakerHealth::is_liquidatable` in the contract's integers, the same `isHealthy` the maker's test now shares. No fee enters eligibility and the ratio is the one stored on the position, which is the deployed rule rather than the contracts repository's main. `liquidation_prices` is the first mark the integer test fails, solved in closed form and refined to the atom, in the same `LiquidationPrices` the maker search returns: a long turns below the mark and a short above it, with neither side turning for a long whose fixed legs cover it at any price; `margin_ratio` is the ratio the test compares.
- **`StateAt::taker_healths` and `MarketReader::get_taker_healths`, the taker batch.** One `RowOutcome<TakerHealth>` per id, in input order: the health of an open taker, `None` for a band, a closed position or an id never minted, an error for the one id whose read failed. The cumulatives and the mark are read once at the handle's block, then one row multicall per chunk of at most `MAX_ROW_BATCH` ids carries each position, its taker checkpoints and its maker row, the last to tell a taker from a band exactly as the contract does. The batch takes `RowOutcome` rather than a third outcome shape, as the client node's debts asked. An ignored live test sets the port's verdict against `liquidateTaker` simulated on every open taker of a market.
- **`history::Tape` and `history::TapeSlice`, the record as a type.** A `Tape` owns the invariant the vector only implied: rows in strict chain order, one hash and a monotone timestamp per block. `Tape::new` checks once and refuses at the first row that breaks it with `ValidationError::InvalidTape`, which names the row and the reason; `push` refuses a row that does not follow the last, as `Sequenced` refuses an event; `append` takes the segment after or refuses it whole; a collect of rows in any order, some repeated, sorts them, drops exact repeats and checks the rest, so two different rows at one point are refused rather than one of them chosen. `TapeSlice` is any run of a tape's rows, what the tape derefs to and what the groupings yield, so a segment answers as the whole does: `blocks()`, `transactions()`, `windows(window)` and `segments(n)`, the last cut only between blocks for a fold per core; `split_at(row)`, the cut the combine law is checked at; `at(point)`, `between`, `in_blocks` and `in_time`, each a binary search; and the lenses `swaps()`, `prints()` and `of_position(id)`, typed records with their points. The bench's own block-boundary cutter is `segments` now, and the property tests say a tape shuffled and doubled collects back into itself and a tape split at any row appends back into itself.
- **`Swap` and `SwapAction` are the tape's marks.** They move from the replay to the tape, since a tape yields them unaided through `Swap::of` and the `swaps` lens; the replay's activity fold keeps the same record as arrivals, so a question asked of a tape and of a replay is asked in one vocabulary. Their path at `history::` is unchanged.
- **`TapeEvent::arrival(mark)`**, a mark stamped with the event's point, time and transaction, as `sample` stamps a value.
- **`history::Wallets` and the custody filters.** `Wallets` is the scope a question is asked at, a set of addresses with no memory of how it was drawn: `Wallets::one(address)` for an agent's own, a collect of addresses for a cohort's. `Arrivals::by(&wallets, &custody)` returns the arrivals on positions the scope held when they arrived, the same `Arrivals` type again, so every reading over a market's swaps, liquidations or settlements is a cohort's or an agent's by one call; `Positions::held_by(&wallets, &custody)` is the scope's book now. Both take the custody fold as a parameter, which the replay carries as `Replay::custody`, so a bare `Arrivals` never holds a reference to a fold it does not own. `Positioned` names the position a mark is about; `Swap`, `Liquidation` and `Settlement` are. A position's mint lands before its opening trade in the same transaction, so an open is attributed to its minter; the recorded tapes confirm the order.

## [0.11.0] - 2026-10-05

The history release. The replay keeps history: what a market's figures
were is asked as often as what they are, and until now every question
about when had to re-read the events the replay had already interpreted.
Two shapes carry it, a series for a value that holds between updates and
arrivals for events that happen, each answering with its provenance and
trimmed to a retention; the replay keeps both, and a ninth fold records
the market's activity, one liquidation record per position per
transaction whichever build's events carried it. Around it the history
module's layers became its directories and its design nodes, its tests
were read and reorganized with the rows they share in one home, and the
law the whole program rests on was stated at any cut and found wanting in
one fold, which is fixed. Breaking, so a minor bump, as for any breaking
change before 1.0.

### Breaking

- **`Replay::pool_price`, `Replay::index` and `Replay::open_interest` return series**, `&Series<Price>` and `&Series<OpenInterest>`, whose `value()` is the latest value the accessors returned before and whose `latest()` is that value with its provenance. A caller that read the latest value adds `.value()`.

### Added

- **`history::Series` and `history::Arrivals`, the two shapes a market's history takes.** A `Series<T>` is a value that holds between updates: `at(point)` and `at_time` answer the last statement at or before, `change_over(window)` the value then and now, `peak(window)` the largest in a window. An `Arrivals<M>` is a point process with a mark per arrival: `count_in(window)`, `busiest(window)`, the arrivals in a window. Both are kept in chain order and refuse a push at or before their last point, as `Sequenced` does; both append when segments of a tape combine, so a fold holding them keeps the law `fold(a ++ b) == combine(fold(a), fold(b))`, which `tests/replay_properties.rs` now checks over them; both trim to a `Retention`, the last `Window` plus the sample before it, or everything. Every question returns a `Reading<T>`: the value in its units, the point it holds at, the timestamp, and the `Span` of samples it was computed from, so an alarm can name what moved it and a forensic bin can reproduce it. No bound anywhere: a bound is the caller's policy.
- **The replay keeps series.** `Replay::retaining(retention)` sets how much every series keeps; the default is everything. `pool_price` and `index` are series of every swap's price and every print; `capacity` and `open_interest` of every update; `margin_total` and `bad_debt` of what the contract stated, statement by statement, which append across segments where the fold's own running totals would not. `deposited` is the USDC that entered as margin since the fold's start, what bad debt is set against, since no trading loss can exceed what was ever deposited. `solvency()` and the block-taking accessors are unchanged.
- **The replay keeps arrivals**, through a ninth component fold: `swaps`, every taker swap with its `SwapInfo` and whether it opened, adjusted or closed; `liquidations`, one `Liquidation` per position per liquidating transaction whichever build's events carried it, a tailed close, a conversion, or the dedicated event after, with the pool price before and after and the index then; `settlements`, what a position paid or earned in funding, utilization fees and LP fees when it was touched; `prints`, the beacon's prints as arrivals. A cut inside a liquidating transaction, or before a segment's first price, is repaired at `combine` as the fold of both segments would have recorded it; the property tests cut everywhere and say so.
- **`PositionRole` is public**, maker or taker: the enum the liquidation twins were keyed on, now the side a liquidation or settlement record carries.
- **`TapeEvent::sample(value)`**, a value stamped with the event's point and time.
- **`history::test_support::tape`**, under `test-utils`: the row builders a fixture tape is written with, `row`, `price`, `swap`, `settle`, `modify` and `per_side`, and `assert_combine_law`, the fold law as a checker any implementor runs at every cut of a tape from a start of its choosing; `mined_event_log` beside `FakeNode`, a typed log as a node returns it. The replay tests, the property tests and the bench build their rows from them, so a fold in a crate built on the replay is tested over the same rows and held to the same law. `tests/replay_properties` requires the feature now, and its generator speaks the whole `MarketEvent` vocabulary, one transaction per block, held to it by a match with no wildcard: a new variant does not compile until the generator produces it.

### Changed

- **The history module's layers are its directories, and its design nodes mirror them.** `history/tape` holds the row, the three readers and custody; `history/fold` the contract, the chain-order guard, the three shapes and the series; `history/replay` the market rebuilt; the tests sit one file per module under test. No public path changed. The history node is the aerial view of how the layers fit, with a node for the tape and one for the algebra beneath it beside the replay's. The fold node states the law at any cut, between blocks or inside one, since the tests cut inside transactions and the first property run found what that catches.

### Fixed

- **The solvency fold kept its law only between transactions.** A withdrawal transfers after its swap's fees leave, so the fold removes a swap's fees only when no nonpositive `MarginTransferred` in the same transaction has been seen. A segment that began between the transfer and the swap had not seen it, removed the fees, and combined into a margin total short of the whole fold's by those fees. The fold now notes what it removed in its first transaction before any transfer, and `combine` takes it back when the earlier segment holds that transaction's withdrawal. Found by the property tests once the shared rows gave their adjusts and closes the fees the unit fixtures carry; the unit test with exactly that shape now checks the law at every cut. Nothing a one-pass fold returned changes.

## [0.10.0] - 2026-10-05

The replay release. The tape becomes sufficient for a fold, everything the
chain says about a market in one chain order from either tense, and the
first fold over it lands: a market rebuilt from its events, equal to the
market read from storage to the atom, with three starts, a chain-order
guard, a count of what it cannot know, and a file format that outlives the
decoder. Alongside it the second live contract build is read, traded and
decoded, and a review of `units` against its own standard found one path
that could wrap. Breaking, so a minor bump, as for any breaking change
before 1.0.

### Breaking

- **`SolvencyState` holds `UsdcAtoms`** (was `f64`). The figures it is set against — every position's margin summed, the USDC the contract holds, the same books folded from the tape — are exact, and a comparison to the cent needs both sides exact. A consumer that read the floats calls `.usdc()`.
- **`TapeEvent` carries `block_hash`.** Every log has one, and a state rebuilt from the tape must carry the same block identity a pinned read carries, or the two can only be compared approximately. A consumer that builds a `TapeEvent` by literal gains a field; one that reads them is unchanged.
- **`PerSide::on` returns `&T`** (was `T`, and only for `T: Copy`). The accessor is now the lookup it says it is, so a pair of anything can be read by side, not only a pair of `Copy` values. Every call on a `Copy` type still works through auto-deref for method calls; a site that compared or stored the value adds a `*`.
- **`SqrtPrice::from_price(f64)` is gone.** It took the root in `f64` and scaled, and disagreed in the last bits with `SqrtPrice::try_from(Price)`, which takes the exact integer root of the Q96 word; two doors to one place that did not agree. The route from a float is `SqrtPrice::try_from(Price::try_from(x)?)?`, and `convert::price_to_sqrt_price_x96` takes it.

### Added

- **`history::Fold`, the one contract every fold over a tape shares.** `apply` one `TapeEvent`; `combine` the fold of the segment that follows; `fold` the batch form. The law `fold(a ++ b) == combine(fold(a), fold(b))` at any cut between blocks is what makes a fold of any prefix a checkpoint and lets a tape fold on every core; `tests/replay_properties.rs` checks it over random tapes, with associativity and prefix-resumption beside it. `OwnershipLog` is the first instance, its inherent `fold` kept so the call needs no import.
- **`history::Replay`, a market rebuilt from its events.** Pool price from the last swap, the index from the beacon, the stored EMAs and rates from the last touch, capacity and open interest, the solvency books, the module in force for each of the six kinds, and custody through `OwnershipLog` in the same pass. The accessors return the read types — `MarketCapacity`, `Mark`, `Emas`, `SolvencyState`, `OpenInterest` — and those that carry a block take the caller's, so a rebuilt snapshot equals a pinned read at that block or the fold is wrong. `from_genesis(perp)` starts a market with its zeros stated; the trait's `fold` starts a segment with nothing stated. `tests/replay_live.rs` (ignored; `RPC_URL`, `PERPCITY_PERP`, `PERPCITY_TAPE_FROM`) runs the comparison against a live market, and its first run found the one silence the fold must know: every taker swap removes its protocol, creator and insurance fees from `totalMargin` with no event, and whether the transaction's `MarginTransferred` was emitted before or after that removal is the path's — a deposit is transferred before the removal, a withdrawal after, a liquidation's fee after the close event. The run read a margin total 22.010 USDC below the sum of its statements, exactly those fees since the last one; with the rule, the replayed books equal the read to the atom. The rule is the live build's (`v0.2.2`; the retired build ordered the paths differently and is not served), and the next build emits the total on that path (`SwapFeesRemovedFromMargin`), so the rule goes with the cutover.
- **`history::Gaps`** counts what may have moved without an event saying so: donations and bad-debt bookings since the last `MarginTransferred`, which move `totalMargin` without emitting it; swaps while debt stands, which remove less than their gross fees by what repaid the debt; and the same swaps' insurance fee repaying debt, which no event carries. A reading with a nonzero gap is forensic, not a decision's input; the pinned read is its check. Each count is decided from the latest total at the read so it combines across segments like everything else.
- **`Replay` rebuilds every position and the pool's liquidity.** `Replay::position` and `Replay::positions` answer for every id the tape mentioned with a `history::PositionState`: a taker's size as the sum of its swaps' perp deltas, which is the row's `amount0`; a maker's band as the range its first liquidity change named and the sum of the changes since, which is `makerDetails`; the pool price at the open, which is what the band's capacity was classified at and what #164 asked for the inverse of; the open, the close, the last touch, and how many liquidations landed on it, whether the build says so in a dedicated event or in the close's tail. `Replay::pool_ticks`, `pool_tick` and `pool_liquidity` are `PoolSnapshot`'s tick map, tick and active liquidity: the tick map as signed sums of the PoolManager's changes per tick, the tick from the last `TicksCrossed`, which carries the pool's own tick after the swap. The root itself is not rebuilt, since the emitted pool price is its floored square; the read's root squared equals the fold's price. What a segment cannot know it says: a position first seen mid-life has what moved and no level, a maker converted to a taker has a size no live event carries, and a segment's tick map is never whole, so `pool_ticks` is `None` off genesis. `tests/replay_live.rs` now checks every minted position's size or band and the whole tick map against the reads.
- **`history::Gaps` counts positions too**: `partial_positions`, open positions whose level the fold does not know, the open unseen and no read to supply it; `taker_size_unknown`, open takers first seen mid-life or converted from a maker; `margin_unknown`, open positions whose margin the fold does not know, which a seed supplies and any later event removes, since no live event carries margin. Read off the positions at the read, so the counts combine like the rest.
- **`Replay` is a composition of folds.** One per concern — the mark's inputs, the touch's rates, capacity and open interest, the solvency books, the modules, the pool's liquidity, the positions, custody — each a `Fold` in its own right with its own `combine`, and `Replay` a struct of them whose `apply` hands every event to each. The three algebraic shapes a market's state takes are public types in `history::fold` — `Latest`, a total the contract emits whole; `First`, a value fixed by its first occurrence; `Stated`, a total stated outright with what accrued since it — so a fold written above the crate takes the same shapes rather than hand-writing the merges. Otherwise behavior-identical; the law and the live equality run are the proof.
- **`Replay::seeded(&StateAt)` starts a fold from the reads at a block**, so a live cache boots without a scan: every total stated, the tick map whole, every position's level and margin known, from `capacity`, `mark`, `emas`, `rates`, `cumulatives`, `modules`, `solvency`, `pool`, the position rows and the maker bands at that block. What only events carry — a position's open, a band's deposit price, custody — starts unknown and fills in from the events that follow. The fold stands at the end of the seed's block. `PositionState::margin` is the read's margin until an event touches the position, since no live event carries margin; `PositionState::level_known` says whether the fold knows where a position stands, from the open it saw or the read that seeded it; `PositionState::last` is `None` for a seeded position no event has touched. A seeded fold continued over the tape answers every read-shaped question as the fold from genesis does, which the unit tests check over the fixture and `tests/replay_live.rs` checks against the chain: seeded 2,000 blocks behind the head, caught up, equal to the reads at the head.
- **`Replay::catch_up(&History, TapeAddresses, to_block)`** applies the market's tape from the block after the fold's to `to_block` or the lagged head, and returns how many events it applied: the driver for a fold that follows a market by polling, and the way a fold heals after a feed dropped or refused events.
- **Chain order is enforced in release builds, by `history::Sequenced<F>`.** A fold behind the chain-order guard: an event at or before the last point applied is refused and counted, never handed on. `Replay` is one inside, and any fold a driver feeds directly — `Positions`, `OwnershipLog`, a fold written above the crate — gets the same guard by wrapping. It was a debug assertion. A duplicate, a driver out of order, or a seed's own block delivered again all land on the count, and a consumer that sees it rise heals with `catch_up`.
- **`Gaps` has three parts with three cures.** `silences`, what the contract moved without saying so, cured by the cutover; `unknowns`, what the fold's start did not supply, cured by a seed; `faults`, what the driver did that the fold would not take, cured by `catch_up`. One flat struct hid which lever a consumer should pull; the monitor wires one alarm per part.
- **`StateAt` reads for every figure the fold holds**: `emas` (the stored pair and its touch, before any advance), `rates` (as a new `MarketRates`: funding, utilization fees, last touch), `cumulatives`, `modules`, and `maker_bands`, the batched `maker_band`. Each is the read side of a fold accessor, so every snapshot type keeps two producers. `CumulativesInfo` converts from the contract's struct once, for the event and the view alike.
- **A market's tape as a file: `History::record` and `history::Recording`.** A recording holds a market's raw logs over a block range, each stamped with its block's timestamp, under a `Manifest` that names the chain, the market's three addresses, the range, the hash of the range's last block, the crate version that recorded it, and how many logs of this vocabulary would not decode. It holds logs and never what was decoded from them: a decoder fixed after the fact reruns over the logs, where a file of decoded rows would carry the bug for good. `Recording::write` and `Recording::read` are `manifest.json` beside `logs.jsonl`, one log per line; the manifest carries the keccak-256 of the log file as written, so a log reordered, substituted or lost, or a manifest beside another recording's logs, is refused at read rather than decoded into a different tape, and the manifest is read through an accessor so no caller can write one that describes other logs; `Recording::tape` decodes with no node; `Recording::check_tail` asks a node whether the recording's end still stands, comparing the header hash at the last block and a fresh scan of the last blocks against what was recorded, so a reorg past the end or a node that served a short range is found rather than trusted. The `record` example writes one and checks it; the history benchmark reads one through `PERPCITY_RECORDING`; the `tape` example no longer writes decoded rows. `PerpCityError::Io` carries the filesystem's errors.
- **Design nodes nest, and have a length.** `cargo xtask design` reads every `DESIGN.md` under `src`, so a submodule has its own node: `src/history/replay/DESIGN.md` is the replay's, with the fold's diagram as its mental model, and the history node is about scans, the tape and the fold contract again. A row is held to 100 words and a node to 300 lines, as a ratchet: a new or changed row over the limit, or a node over it that grew, is a problem in `--diff`, and `--check` reports how many stand over each. A row states the invariant, the producer and the consumer; the node's prose teaches.
- **A history benchmark** (`cargo bench --features test-utils --bench history_bench`) times the stages of a replay apart over a synthetic three-address tape, or over a recording named by `PERPCITY_RECORDING`: the scan, the decode serial and one thread per core, a no-op match per row, `Replay` in one pass, and `Replay` as one segment per core cut between blocks and combined, asserted equal to the one pass. The replay node's efficiency section quotes it: the fold is not where a replay's time goes, the decode parallelizes, and the segments pay only when a tape is long relative to its positions.
- **`#[must_use]` on every value type in `units`**, so a dropped amount, price, rate, share or pair warns where it is dropped. Two sites in the crate were calling a function only for its error and now say so with `let _`.
- **`Hash` on `Price` and `SqrtPrice`**, which the counts already had.
- **Property tests over the exactness claims** (`tests/units_properties.rs`): a split returns every atom; a partition is exactly the whole; `value_at` and `perp_at` never gain an atom round-tripping; an implied price values back to at most the leg it came from; a price and its root round-trip within the documented floor; a human price reads back within a millionth; a six-decimal human amount is the atom count it names; a share and its complement lose at most one atom between them.

### Changed

- **`History::market_tape` reads a market's whole record in one chain order** (#101): the perp's own events, the beacon's prints and the PoolManager's liquidity changes for the market's pool, merged on chain point. The perp and the beacon share a filter by address; the PoolManager is every pool on the chain, so its filter names `ModifyLiquidity` and the pool id it indexes by, and the two scans share one learned width. `TapeAddresses` carries the three addresses and `MarketReader::tape_addresses` fills it, since the handle cannot know a market's beacon or pool id; `market_events` stays the one-address scan.
- **`MarketFeed::next_stamped`** hands back each event as the `TapeEvent` a scan would have built from the same log — block, block hash, log index, timestamp, transaction — so a fold that orders on chain point or pairs a transaction's logs never knows which tense fed it. One header read when the subscription's log omits its timestamp, through the scan's own path. The feed's event set is still the perp and its beacon; the PoolManager's liquidity changes the market tape carries are not on it yet, and a fold that needs the pool's liquidity live follows the lagged tail through the handle.
- **Governance events enter the vocabulary.** The six `Set*Module` events, absent from the bindings until now, decode as `MarketEvent::ModuleSet { module: ModuleKind, address }`; a fold that rebuilds a market from its events needs them to know which pricing, fee, funding, margin-ratio and price-impact rules were in force. Each topic is locked in the ABI test, and a real `SetPricingModule` log from HORMUZ-COUNT-PERP locks the indexed layout, which a signature hash cannot. The market tape scans the beacon it is given for the whole range; a tape spanning a beacon swap is short of the earlier beacon's prints, which no live market has done yet, and the history node records the two-phase scan that closes it.
- **`v0.2.2-upgradeable` markets read, trade, liquidate and decode.** The second live build (contracts tag `198559a`, factory `0x90C8cb83C4257156bf3eE0C9bB8f164B20A04da1`) shares every view and trade selector with build `58b42b7` and differs in three places the crate now covers. Liquidations: `v0.2.2` takes an amount (`liquidateTaker(uint256,address,uint128)` / `liquidateMaker(uint256,address,uint128)`), so `liquidate_*` and `simulate_liquidate_*` read the market's era once from its pool key (only a `v0.2.2` pool carries the guard hook) and, on the newer build, read the position's whole size first and send the 3-arg call; a position already at zero size is `ContractError::PositionNotFound` rather than a `ZeroLiquidity` revert. Events: the untailed `TakerClosed(posId, sr, funding, utilFees)` decodes to `MarketEvent::TakerClosed` with `liquidation_fee` zero and `is_liquidation` false, as the untailed maker closes already did; the fee is in the `TakerLiquidated` that follows (#167 tracks pairing the two). Errors: `NoSurplus`, `ZeroAddress`, `UnauthorizedPoolAction`, the ERC-1967 / UUPS / `Initializable` reverts of the proxy, `InvalidPerpImplementation`, `NotProtocolOwner` and the Solady ERC-721 reverts decode by name. The new bindings live in `contracts::PerpV022`; `Perp` is unchanged.
- **The stored EMAs come from storage slot 11, not `emas()`.** `v0.2.2` dropped the view and both builds keep the `PricePair` at the same slot (`ammPrice` low, `index` high), so `get_snapshot`, `StateAt::mark` and the maker-equity batch read the word with `eth_getStorageAt` at the pinned block, alongside the beacon's index, in place of the multicall row. Same block, same round-trip count; the `Perp::emas` binding stays so the public surface does not move. A transport failure on that read is a transient `StorageReadFailed`, as the batched storage reads already report; the node's own answer still classifies by block.
- **The liquidation twins are keyed by a position's role, not a "book".** The private enum behind `liquidate_maker`, `liquidate_taker` and their probes is `PositionRole { Maker, Taker }`, and the docs, the tracing field and the client node say role where they said book. The markets are pools; the word had no referent. No public signature changes.

### Fixed

- **Scaling could wrap.** `Factor::apply` for `f64` and `Share` multiplied in 256 bits with the primitive's `*`, which wraps in release, so a `Price` word times a WAD factor could come back as a plausible wrong price. Both now take the product in 512 bits through `mul_div`, as every crossing already did, and a quotient past `U256` panics as the bug it is rather than wrapping. `FundingRate::over` and `UtilizationRate::over` follow for the same reason.
- **`TryFrom<f64> for Price` claimed to keep the whole `f64` mantissa.** It keeps 48 fractional bits, which is the whole mantissa at 32 and above and every digit a six-decimal price has well below that; the doc now says so.
- **The human door's exactness claim had no upper bound.** 0.9.0 said a six-decimal amount arrives as the atom count it names "at any magnitude the `f64` holds exactly". The first run of the new property test found the bound: 2^32 units, about four billion dollars, past which a unit in the `f64`'s last place is more than half an atom and the sixth decimal is not in the number to read. The doc and the design node now say so. No behaviour changed; a claim did.

### Design

The `units` node's explanations are brought current: the crossing count (three corners on the asset pair, not two), the scaling rule (512 bits, not 256), the efficiency section (what the 512-bit product buys, where the allocations are), the `fixed_point` edge (it did not shrink, it became the one place a wide product is taken), and three new debts — the four macros against one generic, the width of the error type, and the odd name among the human readings. The `history` node is about scans, the tape and the fold contract; the replay has its own node under it, three pictures and an efficiency section with measured numbers; the client and feeds nodes lose their last order-book words.

## [0.9.0] - 2026-10-03

Two findings from the typed position views in the strategy layer: a fill's
two legs imply a price the language could not express, and the human door
to an amount was flooring numbers people wrote. Breaking, so a minor bump,
as for any breaking change before 1.0.

### Breaking

- **`TryFrom<f64>` for the amounts rounds to the nearest atom** (was floored). The door is for a number a person wrote or a strategy computed, and most decimals have no binary representation: `0.29` arrives as `0.28999999999999998`, so the floor made it `289_999` atoms, an order a strategy did not mean to send. Rounding reads every decimal of six places or fewer to the atom it names, at any magnitude the `f64` holds exactly; a seventh place rounds to the nearer atom, away from zero on the half. Two kinds of input see the change: a decimal of six places or fewer whose binary form sat just under it, which the floor read an atom low and which now reads exactly, and an input with more than six places, which now rounds to the nearest atom where it floored.

### Added

- **`UsdcAtoms::per(PerpAtoms) -> Price` and `UsdcDelta::per(PerpDelta) -> Price`**: the price two legs of a fill imply, USDC per token, exact in Q96 and floored; the third corner of the asset crossing, the inverse of both `value_at` and `perp_at`. A position's entry price is its cost basis over its exposure, and until now every consumer divided two floats to get it.
- **`Display` for the six amounts.** `UsdcAtoms`, `UsdcDelta`, `PerpAtoms` and `PerpDelta` write the exact decimal with trailing zeros trimmed — `5.8` for `5_800_000` atoms, `-1.000001`, `100` — a precision truncating the fraction and a width padding as an integer's does; `LUnits` and `LDelta` write the whole count, since the pool's unit has no fraction. A log line takes an amount without a lossy `.usdc()` first.

## [0.8.0] - 2026-10-03

The release the first consumer of the language asked for. It wrote its
orders in 0.7.0's types and found the three places the chain's own values
still arrived as floats: the market snapshot a cache seeds from, the stored
EMAs it marks with, and the maker side's missing exact door. Breaking, so a
minor bump, as for any breaking change before 1.0.

### Breaking

- **The market snapshot and the stored EMAs are prices.** `MarketSnapshot::{pool_price, index_price, mark}` and `MarketConfig::pool_price` are `Price` (were `f64`), and `Emas::{amm_price, index}` likewise, with `Emas::advanced` and `Emas::mark` now the exact advance and the exact fair price, returning `Result` where the contract's own arithmetic can refuse. The snapshot is what a live cache seeds from before it follows the feed, and the feed has carried `Price` since 0.6.0: a cache that seeded from a float and then stored the events' exact words held two kinds of number under one name, and the strategy layer was converting the float back at the seam. `fair_price_f64` remains the one float twin, for simulators. A `MarketConfig::pool_price` printed with `{}` still reads as the number it did, through `Price`'s new `Display`.
- **`close_maker` submits through the exact door.** It builds an `ExactAdjustMakerParams` from the negated depth rather than a human-unit `AdjustMakerParams` with a `0.0` margin; the transaction it sends is unchanged.

### Added

- **`ExactAdjustMakerParams` and `PerpClient::adjust_maker_exact`.** The maker adjustment in the chain's units — the margin change a `UsdcDelta`, the depth change an `LDelta`, the two limits as `PerpAtoms` and `UsdcAtoms` — with no float on the path, so the maker side has the single submission path the taker side has had since 0.5.0. `adjust_maker` scales into it and delegates. A strategy whose plan already holds the deltas as these types was converting the margin to a float to get here.
- **`Side::long_if(bool)`**: the side a boolean judgement names — buy or sell, mark below the index or above — for the `if buy { Long } else { Short }` every consumer wrote.
- **`Display` for `Price` and `SqrtPrice`**: the human reading, honouring the formatter's precision, so a price goes into a log line without a conversion; a price with no reading shows its Q96 word with an `x96` suffix.
- **`Emas::stored(PricePair, last_touch)` and `Emas::pair()`**, the two ways between the contract's stored words and the typed pair.

## [0.7.0] - 2026-10-03

A release about the language a strategy writes in. The units node now opens
with the sentences the types exist to make true — `band.liquidity * half`,
`mark * 0.5`, `margin.perp_at(mark)? * leverage`, `mark / index - 1.0` — and
after this release every one of them compiles. Breaking throughout, so a
minor bump, as for any breaking change before 1.0.

### Breaking

- **The exact trade parameters carry the units.** `ExactOpenTakerParams` is `{ margin: UsdcAtoms, perp_delta: PerpDelta, amt1_limit: UsdcAtoms }` and `ExactAdjustTakerParams` is `{ pos_id, margin_delta: UsdcDelta, perp_delta: PerpDelta, amt1_limit: UsdcAtoms }`, where every field was a bare `u128` or `i128`. The exact door exists so a market maker can size to the atom without a float on the path; with primitives on it, the one thing it could not tell you was which asset a figure was, and `TakerQuote::amt1_limit` has returned `UsdcAtoms` since 0.5.0 while the field that takes it did not. The human-unit parameters are unchanged.
- **`OpenInterest` is the contract's pair of counts.** `{ long: PerpAtoms, short: PerpAtoms }`, with `on(Side)` and `total()`, where it was `{ long_oi: f64, short_oi: f64 }` — the read divided the contract's atoms into a float, and the one consumer that needed the atoms was multiplying back. Open interest is now in the unit the capacity it draws on is counted in, which closes the `client` node's debt that said so, and `PerpAtoms::value_at` is how it becomes dollars.
- **`UsdcAtoms` → `UsdcDelta` and `PerpAtoms` → `PerpDelta` are `From`, not `TryFrom`.** Widening a count to its signed twin cannot fail for the reason the delta's `+` cannot: the supply bound keeps any count the chain produces far inside `i128`. The checked door was a door onto a case outside the type's domain, and every consumer wrapped it in an `expect`. The narrowings — a delta to a count — stay `TryFrom`, because a negative one is not a count. A `UsdcDelta::try_from(atoms)` still compiles through the blanket impl, with `Infallible` as its error.

- **Every long/short pair is one `PerSide<T>` field.** `Capacity` and `OpenInterest` are aliases of `PerSide<PerpAtoms>`; `MarketCapacity` holds `open_interest: PerSide<PerpAtoms>` where it held two fields and its `open_interest(side)` method is the pair's `on`; `AccrualInputs` holds `util_fee_per_day`, `open_interest` and `capacity` as pairs (was six fields); `MakerMarketSnapshot::util_earnings`, `MakerState::last_util_earnings` and `MakerState::capacity` likewise; `MakerEquityBreakdown::util_earnings()` returns the pair and the two per-side accessors are gone — `.total()` is what the settle credits. The event vocabulary follows: `CapacityUpdated { capacity }`, `OpenInterestUpdated { open_interest }`, `RatesAndEmasRefreshed::util_fee_per_day`, `CumulativesInfo::{util_payments, util_earnings}` and `MakerSettle::util_fees` are each one `PerSide`. The SDK's own structs spelled the pair five different ways (`long_oi`, `oi_long`, `cap_long`, `long_open_interest`, `long_util_earnings`), and a strategy that wanted "the side I'm on" had to branch on a boolean at every one. `PerSide` serialises as `{long, short}`, so the fields that were already named that way are wire-compatible; the ones that were two flat fields are not.
- **`Side` lives in `units`**, beside the quantities it keys, and gains `opposite()`, `sign()`, `BOTH`, and `exposure(PerpAtoms) -> PerpDelta`; `PerpDelta::side()` reads the sign back. The crate-root re-export is unchanged, so `perpcity_sdk::Side` and the prelude still work; `perpcity_sdk::math::capacity::Side` does not.

- **`Ratio::for_leverage` takes any `Factor`** (was `f64`): a plain `5.0` still works, and so does the strategy's own leverage type. `leverage_to_margin_ratio` in `convert` keeps the float signature as the human door, and refuses a float that is not a number where the trait would panic.

### Added

- **`Factor`: the one trait in `units`, and it is open.** Every count, delta and price multiplies with `*` by anything that implements it — `f64` (taken to WAD once, exact from there), `Share`, or a scalar the strategy defines for itself, a `Leverage` or a `Skew` whose invariant lives in its own constructor and which then multiplies the SDK's quantities directly. One method, `apply`, scales a magnitude exactly and truncates toward zero; `is_negative` says whether it flips a sign, which a delta takes and a count refuses with a panic, as the primitive does on an operation its type cannot hold. Scaling is the one verb that is the same on every quantity and the one place a consumer adds a type of its own, which is why it is a trait where everything else in the module is an inherent method whose name says the unit.
- **`Price` scales and divides.** `mark * 0.5` is a stop, `index * (1.0 - zone)` a landing zone, both exact in Q96; `mark / index` is the plain `f64` a basis is one less than, computed once in 256 bits rather than three ways in three crates. A negative factor on a price panics: a price has no sign.
- **The crossings have their inverses.** `UsdcAtoms::perp_at(Price) -> PerpAtoms` and `UsdcDelta::perp_at(Price) -> PerpDelta` size tokens from USDC, the inverse of `value_at`; `SqrtPrice::try_from(Price)` is the exact integer root of the Q96 word, the inverse of `squared`, which until now had none — so every route from a price to a tick went through `f64`.
- **`Price::tick()` and `Price::at_tick(i32)`** in `math::tick`: the exact route between a price and the grid, replacing `price_to_tick(f64)`/`tick_to_price(f64)` for a caller that holds a `Price`. The floor the root inherits can resolve an exact boundary to the tick below, which is why **`TickRange::between(Price, Price)`** aligns to the spacing — lower down, upper up — and is the band a strategy's two prices become. **`TickRange::geomean()`** is a band's centre, `√(P_lo · P_hi)`, exact.
- **`margin_for_liquidity(range, LUnits) -> UsdcAtoms`**: the margin a depth requires, the inverse of `estimate_liquidity`, rounded up so the round trip never comes back short. The strategy layer had ported this line of contract math for itself.
- **`FundingRate::over(Duration, UsdcAtoms) -> UsdcDelta`** and **`UtilizationRate::over(..) -> UsdcAtoms`**: what a notional pays or is charged at a rate across an interval, whole seconds as the contract accrues.
- **`Share`: a fraction the caller chose, and the exact arithmetic of applying it.** `u32` millionths in `[0, 1]`, built from a fraction (`Share::try_from(0.25)`, rounded to the nearest millionth) or from millionths, serialised as the fraction a configuration writes. It is not a `Ratio` — both are millionths, but a `Ratio` is a word the contract stores with a `uint24` domain that exceeds one, and a `Share` is the caller's own, bounded by the whole — so a function that takes one does not take the other. Every count and delta multiplies by one with `*` — and by a plain `f64`, for the literal in a strategy's hand, converted once to WAD and exact from there; both truncate toward zero, the chain's division, in 256 bits — and gains `split(NonZeroUsize)` (equal pieces that sum exactly, the remainder's atoms to the first pieces) and `split_weighted(&[Share])` (largest-remainder, sums exactly, the weights must make the whole); the counts also gain `share_of(whole)`. `Share::partition(&[f64])` turns any weights into shares that sum to exactly `ONE`, so `[1.0, 1.0, 1.0]` is `[333_334, 333_333, 333_333]` rather than a `999_999` that leaves an atom unassigned on every split. `(x as f64 * frac) as u128`, which every strategy wrote for itself and each rounded its own way, now has one home.
- **`ExactOpenMakerParams` and `PerpClient::open_maker_exact`.** The band as the pool stores it — a `TickRange`, an `LUnits`, the margin and the two deposit caps as the asset counts they are — with no float on the path. `open_maker` now aligns and scales into it and delegates, so the maker side has the single submission path the taker side had; a strategy that already holds a `TickRange` and an `LUnits` no longer converts both back to prices to place a band. The ticks' alignment to the pool's spacing is the one check left to the contract.
- **`ValidationError::InvalidShare`**, for a share past the whole or weights that do not make one.
- **`PerSide<T>`** (`units/side.rs`): `new`, `uniform`, `on`, `on_mut`, `map`, `zip`, `total`, `iter`, `Default`, and serde as `{long, short}`.

## [0.6.0] - 2026-10-03

Breaking throughout, so a minor bump, as for any breaking change before 1.0.

### Breaking

- **The event vocabulary carries units, not `f64`.** Every money, price and rate field on `MarketEvent`, `SwapInfo` and `MakerSettle` is now its unit's type: USDC as `UsdcAtoms` or `UsdcDelta` by whether the contract's word is signed, perp amounts as `PerpAtoms`/`PerpDelta`, prices as `Price`, and the two rates as `FundingRate` and `UtilizationRate`, which hold the contract's `int88` and `uint64` WAD exactly where the decoder used to divide them into a float. The decoder converted at decode time, so the exact word was gone upstream of every consumer and a fold over a million fee legs carried rounding it could not avoid; now it narrows and never scales, a fold sums atoms, and `.usdc()` or `.to_f64()` is called once by whoever wants a number for a person. `SwapInfo::amm_price` is `pool_price`, and the rename is the substance: it is the pool's price, not the one the contract marks at, and a basis taken against it is not the basis a liquidation uses. `OpenResult` and `AdjustTakerResult` carry the event's own `PerpDelta`/`UsdcDelta` for the same reason, and `history`'s `IndexPrint` holds `index: Price` (was `index_x96: U256`, accessor `index()` → `index_f64()`) so the two tenses of the index agree on a type. `TakerLiquidated::perp_amount` was a bare `u128` with no stated scale and is now `PerpAtoms`.

- **`decode_log` returns `Result<Option<MarketEvent>, ValidationError>`.** `Ok(None)` is a log of another vocabulary — admin, ERC20, pool-internal — and there is nothing wrong with it. `Err` is a log of *this* vocabulary that will not decode: the binding disagrees with the shape on chain, or a value is too wide to hold. Those two were one `None`, so a scan dropped the second kind alongside every admin log it skipped and the tape came back short with nothing said. The error names the field or the event signature. `MarketFeed::next` returns `Option<Result<MarketEvent, _>>`, where `None` still means the socket is gone, so a live consumer sees a hole as it happens; the feed reads on either way. A scan counts the log on the new `ScanStats::undecodable` and carries on — one unreadable log should not cost a scan of millions of blocks, and a non-zero count is how a caller learns its tape is short.
- **`types` is gone; every type in it is `client`'s.** The parameters and results are defined beside the trades that take and return them, the configuration and the market snapshot beside the now-reads that fill them, the margin ratios and solvency beside the pinned reads, `ChainDeployments` beside the handle it builds. The crate-root re-exports are unchanged, so `perpcity_sdk::OpenTakerParams` and the prelude still work; only a direct `perpcity_sdk::types::*` breaks. The module promised that inert data has one home, and the promise did not hold: it carved out every type that gained an invariant, its only importer was already `client`, and `Fees` holding a validated `Ratio` made the claim false while it was still written down. A type's home is the function that produces it.
- **`Side` moved from `types` to `math::capacity`**, and `ValidationError::NoBandCapacity` no longer carries one. The crate-root re-export is unchanged, so `perpcity_sdk::Side` and the prelude still work; only a direct `perpcity_sdk::types::Side` breaks. `Side` lives with the capacity math because that is what it keys, and because having it in the human surface made `errors` — the module everything else depends on — depend on a module above it. The error drops the field for the same reason: the caller passed the side in, so naming it back was both redundant and the thing holding the inverted edge in place.

### Added

- **`UsdcAtoms` and `PerpAtoms` add with an operator and sum from an iterator.** They had `checked_add` and no `+`, so summing the four fee legs of a swap — the commonest thing anyone does to an amount — meant `checked_add(..).unwrap()` or dropping back to `.atoms()` arithmetic. The reason given for withholding the operators was that a count's *subtraction* can go below zero, which is true and is why `checked_sub` and `saturating_sub` stay; it says nothing about addition, where a sum of balances can leave the width but never the domain and the accounting-token supply keeps anything the chain produces far inside it — the same argument that already made a signed delta's `+` infallible. `LUnits` deliberately does not get them: the pool has no supply bound to argue from, so a depth still adds through `checked_add`, which is why every `LDelta` operation is checked too. Found by the first consumer to fold the newly typed event vocabulary.

### Fixed

- **`convert`'s tests moved to the types they were testing.** The module was the most-tested in the crate — 57 tests over ten functions, eight of which are a single line delegating to a unit type — and the tests exercised the callee through the caller. Worse, a one-line delegation's only possible mistake is delegating to the wrong type, and no assertion on the output can catch it, since `UsdcDelta` and `PerpDelta` are the same six decimals. So the scaling block is gone (`units::amount` already pins the scale, the truncation, the `f64` range and the negative refusal), and the coverage that was *only* in `convert` moved to where its subject lives: the leverage ↔ margin-ratio table and its refusals to `units::rates`, which had no test for `Ratio::for_leverage` at all; the Q96 value tables, the 2^80 mantissa bound, the protocol's widest root and the price ↔ root round trips to `units::price`; the negative-floor direction of `atoms_from_f64`, whose docstring claims it is "pinned by a test", to `units::amount`. What remains in `convert` is the first test of the two functions it actually owns — the V4 balance-delta packing, now checked at both `i128` ends with independent signs, and the narrowing door that must name the field it refused. 476 tests where there were 525, and `convert.rs` is 364 lines rather than 739.

- **`UsdcDelta` no longer claims a settle's parts all point one way.** Its doc said every component of a settle preview is "positive toward the position and negative away from it, and they add" — but `MakerEquityBreakdown::funding_owed` is positive when the position *pays* and is subtracted, as is the event vocabulary's `MakerSettle::funding`. The field docs and the arithmetic were right; the type's doc was wrong in the one place a sign error costs twice the funding. It now says direction is the field's own.

- **The design graph attributes a binding's use to the type's own inherent methods**, not to the file it is declared in. A type with no behaviour calls nothing, so taking its declaration file handed it every binding its neighbours happened to use — 112 edges of the 316 the graph drew were that artefact. Trait impls are excluded for the same reason: a derive reports the declaration file and a `From` reports the other type's home. No designed edge changed, and the one structure the noise had been hiding is an island already recorded as a debt.
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

[Unreleased]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.11.0...HEAD
[0.11.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/StrobeLabs/perpcity-rust-sdk/releases/tag/v0.1.0
