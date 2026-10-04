# `history`: the past tense

Up: the [root](../../DESIGN.md). Sideways: [`events`](../events/DESIGN.md)
for the vocabulary the tape speaks; `feeds` for the present tense of the
same; `transport` for the endpoint the scans go through.

## Purpose

This module reads what already happened: every log a market, a beacon or
a token emitted over a block range of any length, through a provider
that caps how much it will return per request. It turns those logs into
the tape, a market's event history in chain order; the beacon's print
series; the token transfers between address sets; and the folds over
them that answer questions the chain itself cannot, chiefly who held a
position when.

It is the state half's counterpart: `client` reads what the chain holds
at a block, `history` reads what it emitted across blocks. Research is
built on this module; it is where a market's economics, its custody and
its series come from.

## What matters

**The provider's limits are unknown and change.** Every provider caps
`eth_getLogs` by span, by result count, or by response size, words the
rejection differently, and moves the cap with the density of the range.
A scan that hard-codes a chunk size is wrong on some provider on some
day. So the scan learns: it halves on rejection, doubles on acceptance,
narrows in on the limit, and re-learns when a dense stretch moves it.
The learned width is kept on the handle so a process that scans
repeatedly pays the search once.

**A gap is worse than a failure.** A tape with a missing window is a
lie about the market. So a scan returns results in range order, delivers
a window only when it is complete, and returns a typed rejection for the
one block a provider will not serve rather than skipping it. A log of
this vocabulary that will not decode is an error in
[`events`](../events/DESIGN.md) rather than an omission, and a scan
counts it on `ScanStats::undecodable` and reads on: one unreadable log
should not cost a scan of millions of blocks, but a tape that is short
must say so.

**Never on the trading path.** A scan is throughput work: thousands of
requests, minutes of wall time, its own timeout. It shares the transport's
health accounting with the trading loop, so a narrowing rejection, which
is the scan working, is kept off the endpoint's health record. The
research tooling builds its own transport for scans so a slow log request
cannot open the circuit breaker the agents trade through.

**Chain order is the join key.** A print and a fill in the same block are
ordered by log index; custody at the moment of a trade is the transfer
before it in chain order. So every event carries its `ChainPoint`, and
the folds that depend on order say so and assert it.

## The mental model

A scan is a search for the provider's limit, run while doing the work.
The range is carved into windows at the width the search currently
believes; a bounded number are in flight; each window narrows itself
when rejected and reports what it learned; results come back in range
order regardless. A newest-first scan is different on purpose: it exists
to stop early, so it sends one request at a time from the top and stops
the moment the caller has enough.

Over the scan sit the series. The **tape** is one market's logs, decoded
through the same decoder the feed uses, each stamped with its chain
point. The **prints** are one beacon's `IndexUpdated` logs as an index
series. The **transfers** are one token's `Transfer` logs between address
sets. Each is the same shape: a range, a filter, the decoder, chain order.

Over the tape sit the folds, every one an instance of one trait. `Fold`
is a state that advances by one event and merges with the fold of the
segment after it, so the same code is a batch computation over the tape,
a live one over the stamped feed, and a parallel one over segments cut at
block boundaries. `OwnershipLog` folds the position NFT's transfers into
custody over time, so `owner_at(pos, point)` answers who held a position
when an event happened, which is the attribution a measurement wants, and
`latest_owner` answers the naive question for compatibility. `Replay`
is a composition of folds, one per concern — the mark's inputs, the
touch's rates, capacity and open interest, the solvency books, the modules
in force, the pool's liquidity by tick, the positions, custody — each
producing the same types the pinned reads return, so a market rebuilt from
its tape is compared to a market read from storage with `==`; what the tape
cannot carry it counts in `Gaps` rather than guessing. Research's
economics and reconciliation are further folds over the same tape, and
they live above this crate because they apply a perspective.

`History` is the handle that owns the learned width, the block lag, the
in-flight bound and the telemetry, so that a long-lived scanner meters
its reads and a one-off tool gets the same correctness without the
state.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`History`](mod.rs#L125) | the scanning handle over a provider: learned width across scans; a block lag; bounded in-flight windows; cumulative stats. `market_tape` is the one read that walks a range for three addresses at once, the market's whole record in one chain order; the one-address reads stay for a caller that wants one series | [`History::new`](mod.rs#L136) over any provider; [`ChainReader::history`](../client/DESIGN.md), the one handle a chain reader keeps so its scans share one learned width | nothing takes it; its reads produce the series below. The strategy layer's research sources take a reference to one so a whole run pays the width search once. |
| [`ChainPoint`](tape.rs#L54) | where an event sits: block number and log index; the total order events are joined on | [`TapeEvent::point`](tape.rs#L81) | [`OwnershipLog::owner_at`](tape.rs#L210), custody at that point. The strategy layer's joins, a print against the fills after it, are on this key, which is why it is a type and not two fields. |
| [`TapeEvent`](tape.rs#L63) | a [`MarketEvent`](../events/DESIGN.md) at a chain point: the same vocabulary as the feed, plus its position, its block's hash and timestamp, and its transaction. The block hash is what lets a state rebuilt from the tape carry the same block identity a pinned read carries, so the two can be compared rather than approximately agreed; the transaction is what lets a fold pair the logs of one call. Both tenses build it through one constructor, so a feed's row and a scan's are the same row, though the feed does not yet carry the tape's whole event set | [`History::market_tape`](mod.rs#L280), [`History::market_events`](mod.rs#L257) and [`History::latest_market_events`](mod.rs#L303), and their handle-less twins; [`MarketFeed::next_stamped`](../feeds/DESIGN.md), the present tense of the same row over the perp and beacon | [`OwnershipLog::fold`](tape.rs#L198). The strategy layer's economics, classification and series are folds over a slice of these; every fold that depends on order can assert it. |
| [`TapeAddresses`](tape.rs#L133) | the three addresses a market's record is spread across: its own contract, the beacon it reads, and the chain's PoolManager keyed by its pool id. The PoolManager is every pool on the chain, so it is filtered by the liquidity event and the pool id it indexes, never by address alone | [`MarketReader::tape_addresses`](../client/DESIGN.md), which knows all three; or a caller that does | [`History::market_tape`](mod.rs#L280) and [`market_tape`](tape.rs#L316). |
| [`Fold`](fold.rs#L26) | the one contract every fold over a tape shares: `apply` one event, `combine` the fold of the segment after, and the law `fold(a ++ b) == combine(fold(a), fold(b))` at any cut between blocks, which makes a fold of any prefix a checkpoint and lets segments fold on separate cores. An implementation allocates nothing on the common path of `apply` | the two folds here implement it, and the strategy layer's interpreters implement the same trait rather than a sibling | nothing takes it: a trait with one open verb, like `Factor` in `units`. `OwnershipLog` and `Replay` are the crate's instances, and the strategy layer's folds are the rest, so that one contract serves both repositories and the sweep that cuts a tape into segments is written once. |
| [`OwnershipLog`](tape.rs#L165) | custody over time: a fold of transfers in chain order; owner at a point, not merely latest. The first instance of [`Fold`](fold.rs#L26): `apply` appends a transfer, `combine` appends a later segment's timelines | [`OwnershipLog::fold`](tape.rs#L198) over the tape, the trait's fold kept inherent so it is reachable without the import | [`Replay`](replay.rs#L88), which folds it in the same pass. The strategy layer's attribution, which asks who held a position when a trade happened, not who holds it now. |
| [`Replay`](replay.rs#L88) | a market rebuilt from its events: every quantity a total the contract emitted (the latest wins), a sum of its deltas, or a first occurrence, so the fold combines across segments. Its accessors return the read types — [`MarketCapacity`](../math/DESIGN.md), [`Mark`](../math/DESIGN.md), [`Emas`](../math/DESIGN.md), [`SolvencyState`](../client/DESIGN.md), [`OpenInterest`](../client/DESIGN.md) — and the ones whose read carries a block take the caller's [`BlockContext`](../math/DESIGN.md), so a rebuilt snapshot equals a pinned read at that block or the fold is wrong; that equality is the test the type exists to pass, run against a live market. From genesis the totals that are zero before any event — capacity, open interest, the books — are zero, while what only the factory's creation log carries — the modules, the first price, the first EMAs — is unknown until the market's own events state it. The margin total is the last `MarginTransferred` less the swap fees removed since, the one rule here that is the live build's rather than the vocabulary's: the removal is silent, and whether it came before or after the transaction's statement is the path's — a deposit is transferred before it, a withdrawal after, a liquidation's fee after the close event — so the fold reads the statement's sign and transaction to know which. Beside the totals it holds every position the tape mentioned, as [`Positions`](replay/positions.rs#L291), and the pool's liquidity: the tick map as signed sums of the PoolManager's changes per initialized tick, the tick from the last `TicksCrossed`, which carries the pool's own tick after the swap, and the active liquidity as the net at or below it, each what `PoolSnapshot` reads. The root itself is not rebuilt, since the emitted pool price is its floored square; the read's root squared equals the fold's price. The type is a struct of folds, one per concern, each a `Fold` with its own `combine`, so the law is local to each and `Replay::combine` only composes them | [`Replay::from_genesis`](replay.rs#L112) for a market, then `apply` over the tape's rows or the stamped feed's; the trait's `fold` for a segment, whose totals are unknown until stated and whose tick map is never whole | nothing in the crate. The strategy layer's monitor runs it on every market it watches; its live cache seeds from a read and follows the feed through it; its backtests drive it with an engine's events. |
| [`Positions`](replay/positions.rs#L291), [`PositionState`](replay/positions.rs#L101), [`PositionKind`](replay/positions.rs#L21) | every position the tape mentioned, by id, and what the tape said about each: a taker's size as the sum of its swaps' perp deltas, which is the row's `amount0`; a maker's band as the range its first liquidity change named and the sum of the changes since, which is `makerDetails`; the pool price at the open, which a deposit's capacity was classified at; the open, the close, the last touch, and the liquidations that landed on it, whether a dedicated event or the close's tail says so. Every field is a sum, a first occurrence or a latest, so a segment that did not see a position's open knows what moved and not where it stands, and the accessors say so: `taker_size` and `maker_band` are `None` until the level is known, a maker converted to a taker has a size no live event carries, and a position a segment met only through a liquidation or a backstop is of `Unknown` kind rather than a guessed one, which is what lets two segments' answers merge into the whole's | folded by [`Replay`](replay.rs#L88) in the same pass as the totals; the map is read through [`Replay::positions`](replay.rs#L220), one position through `Replay::position` | the strategy layer's live cache, whose own position fold this replaces, and its economics folds, which take the lifecycle from here and the settlements from the events. A `MakerBand` here and one read from chain are the same type, so the two are compared with `==`. |
| [`Gaps`](replay.rs#L52) | how many times a figure may have moved without an event saying so, since the last event that stated it — donations and bookings that move the margin total silently, swaps whose insurance fee repaid debt silently — and how many positions stand on a figure no event stated: their open unseen, their size unemitted, their margin never carried. Counted, never guessed, and decided from the latest total and the positions at the read so the count itself combines across segments | [`Replay::gaps`](replay.rs#L253) | nothing in the crate: the strategy layer's gate. A reading with a nonzero gap is forensic, not a decision's input, and the pinned read is its check. Every silence is the live builds'; the next build emits what repaid the debt on every swap, the margin total on every path, and a position's margin, size and band on every event that changes them, and the type shrinks with it. |
| [`IndexPrint`](beacon.rs#L20) | one beacon update: index, chain point and timestamp. The index is a [`Price`](../units/DESIGN.md), the same type the live `IndexUpdated` carries, so a fold over an index series cannot tell which tense produced its samples; `index_f64` is the lossy view and the field was `index_x96` when it was a bare word | [`History::beacon_prints`](mod.rs#L210) and [`History::latest_beacon_prints`](mod.rs#L233) | nothing in the crate. The strategy layer's index series and estimator bootstrap; it keeps the chain point so a print can be joined to the fills after it. |
| [`TokenTransfer`](transfers.rs#L18) | one ERC-20 transfer between the address sets asked for; the sets are topic filters, not post-filters | [`History::token_transfers`](mod.rs#L320) | nothing in the crate. The strategy layer's fleet derivation and treasury ledger. |
| [`FakeNode`](test_support.rs#L68), [`Mode`](test_support.rs#L29) | an in-memory node that serves `eth_getLogs` under a chosen cap and answers as a provider would: accept, decline a too-wide range, or fail; behind the `test-utils` feature | [`FakeNode::new`](test_support.rs#L81) from a set of logs and a span cap | every scan test in the crate, through the provider it hands out. The strategy layer's research tests scan against it too, which is why it is a feature and not a test module. |
| [`ScanStats`](scan.rs#L241) | what a scan cost: requests, rejections, narrowings, the learned width; the number a collector meters. It also counts `undecodable` — logs of this vocabulary that would not decode, which the scan skips rather than dying over, since one such log should not cost a scan of millions of blocks. A non-zero count is how a caller learns the tape it holds is short | [`History::stats`](mod.rs#L161) | nothing in the crate. The strategy layer's collector, and the benchmark suite, which asserts a scan's request count. |

Two decisions shape the surface. The free functions and the handle offer
the same reads; the handle adds memory, concurrency and telemetry, and a
caller that scans once may not need them. And the `latest_*` reads are
sequential on every path, because a request sent below the stopping
point is waste, and parallelism there would be a bug dressed as a
feature.

## Efficiency

A scan is throughput work and its currency is requests, spent
deliberately off the trading path.

- **Width.** The first request asks for 100,000 blocks; the span doubles
  on acceptance up to 10,000,000, halves on rejection, and narrows in on
  the provider's limit. A provider with a fixed cap costs a few rejected
  requests at the start and a logarithmic number after; a cap on results
  or bytes, which moves with density, costs a re-learn when a dense
  stretch is hit. The learned width lives on the `History` handle, so a
  process that scans repeatedly pays the search once; the free functions
  pay it every call.
- **Concurrency.** A handle keeps 4 windows in flight by default,
  results delivered in range order regardless. The free functions are
  sequential. Newest-first reads are sequential on every path, because
  they exist to stop early and a request sent below the stopping point
  is waste.
- **Timestamps.** When a provider omits `blockTimestamp` from logs, one
  header read per distinct block, with bounded concurrency. This is the
  cost that dominates a sparse scan and is the reason for SDK #100.
- **Rejections are free of health.** A narrowing rejection goes back
  to the scan without touching the endpoint's record and without a
  retry, so the search costs exactly the rejected requests and nothing
  in breaker state.
- **Measured.** `ScanStats` on the handle counts requests, rejections
  and narrowings across every scan it has run. It is the number the
  regression harness (SDK #99) will assert. The pipeline's own cost,
  with no network under it, is `benches/history_bench.rs`: over a
  200,000-log three-address tape served by the fake node, the scan
  (windows, JSON, decode into rows) runs at about 750,000 logs a second,
  the decoder alone at 31 million a second serially and 170 million
  across twelve cores, and a one-`match` fold at 84 million rows a
  second. So of a replay's time against a real provider, essentially all
  of it is the provider; the decode and the fold together are
  milliseconds per hundred thousand events.

A research process builds its own transport for scans so that a slow
or refused log request cannot open the circuit breaker a trading loop
depends on. That separation is a cost rule, not a convenience.

## Edges

- From the [root](../../DESIGN.md): the two tenses of events; chain order;
  never on the trading path.
- From [`events`](../events/DESIGN.md): every log the scan returns is
  decoded there. The tape is `MarketEvent`s with chain points; nothing
  here interprets a log.
- To `transport`: scans go through `HftTransport`, which keeps narrowing
  rejections off the health record and does not retry them, so a search
  can narrow as often as it needs. Backoff for rate limits lives there,
  not here.
- To `errors`: a refusal a smaller range cannot fix is `LogsRejected`,
  not transient; a range the caller got backwards is `InvalidBlockRange`.
  The caller owns the retry policy.
- Sideways to `feeds`: the same decoder, the other tense. A consumer that
  bootstraps from the tape and then follows the feed sees one vocabulary.
- Out to the strategy layer: research sources, a market's tape, a walk
  over a wallet set's transfers, an index series, are scans through a
  `History` handle; an estimator bootstraps from `beacon_prints`; a
  treasury ledger reads `token_transfers`.

## Terminology

- **Scan**: a chunked `eth_getLogs` over a range. **Window**: one carve
  of the range at the believed width. **Width**: the span the provider
  currently accepts; **learn**: how the scan finds it.
- **Narrowing**: a window halving itself on rejection; the scan working,
  not the endpoint failing.
- **Tape**: one market's events in chain order. **Print**: one beacon
  update. **Transfer**: one ERC-20 movement between address sets.
- **Chain point**: block and log index; **chain order**: the order they
  induce, the join key for everything.
- **Fold**: a state that advances by one event and merges with the fold
  of the segment after it; the trait of that name, and the ownership log
  and the replay are the two that live here. **Replay**: the fold that
  rebuilds a market's own state. **Gap**: a figure that may have moved
  without an event saying so, counted since the event that last stated it.
- **Newest-first**: a scan that stops early, sequential by design.
- **Lag**: blocks held back from the head so a scan's top is on every
  replica.

## Accepted structure

- **`Fold` names no other type in its signature, by design.** The trait
  takes a `TapeEvent` and returns `Self`; what a fold produces is the
  implementor's to say, so the trait itself connects to nothing in the
  graph and its instances, `OwnershipLog::apply` and `Replay::apply`, carry
  the edges. The same shape as `Factor` in `units`: one open verb, the
  types on the implementations.

- **`ChainPoint` goes into `OwnershipLog` and comes back out of it.**
  It is the coordinate the fold is sorted by: `owner_at` takes one and
  searches, and `transfers` hands back the point of every change. A
  sorted index takes its key and returns it, so the cycle is the index
  relation rather than a conversion with two homes.

- **The market's tape is two filters over one range.** The perp and its
  beacon share a filter by address; the PoolManager, being every pool on
  the chain, is filtered by the liquidity event's signature and the pool
  id it indexes. The two scans share the learned width and their rows
  merge on chain point. One filter would either miss the pool's liquidity
  or pull every pool's on the chain; this was SDK #101, and the consumer
  that decided its shape is a fold that rebuilds a market from its events.
  The PoolManager filter is compensation for the live builds, whose maker
  events carry no geometry: the next contracts emit a band's range,
  liquidity and every change to it on the perp's own events, so the pool's
  liquidity becomes a fold of one address and the second filter goes with
  the cutover.

- **The replay's snapshots are the reads' types, with the caller's block.**
  `Replay::capacity_at` produces a `MarketCapacity`, `Replay::mark_at` a
  `Mark`, `Replay::emas` an `Emas`, `Replay::solvency` a `SolvencyState`,
  `Replay::open_interest` an `OpenInterest`, `Replay::cumulatives` a
  `CumulativesInfo`, and `Replay::funding_per_day` and
  `Replay::util_fee_per_day` the two rates as a `FundingRate` and a
  `PerSide` of `UtilizationRate`, every one a type `StateAt`, `MarketReader`
  or the decoder also produces, so every snapshot type now has two
  producers. That is the point: the two must agree at a block, and `==` is
  how they are compared. The block on a
  snapshot is the caller's because the fold's last event may be blocks
  before the read, and the mark advances the EMAs to the block it is asked
  for; a fold that stamped its own block would compare unequal for the
  wrong reason. The same holds one level down: `PositionState::taker_size`
  is a `PerpDelta`, the row's `amount0`; `PositionState::deposit_pool_price`
  a `Price`, the one the swap event carries; `Replay::pool_liquidity` an
  `LUnits`, the pool's own unit; so a position or a tick map rebuilt here
  is set against its read with `==` too.

- **The pool's liquidity is a fold of the PoolManager's changes, and three
  of its parts are compensation.** `Replay::maker_band` on a position comes from
  `ModifyLiquidity` joined to the maker by its salt, which the live builds
  set to the position id; `Replay::pool_ticks` is the same changes summed
  per tick; the deposit price is the fold's own pool price at the
  `MakerOpened`; and a liquidation is paired with the adjust, convert or
  close before it in the same transaction by the dedicated event that
  follows. The audited build emits a band's range, liquidity and mark on
  `MakerOpened` and every change on the maker's own events, and names
  liquidations in their own events, so the salt join, the deposit-price
  recovery and the pairing are deleted at the cutover and the pool's
  liquidity becomes a fold of one address. They sit together in the
  positions fold and the pool fold so the deletion is a removal, not a
  rewrite.

## Debts

- **The market tape reads one beacon: the one named at the call.** A
  market's beacon is governance's to swap, and a swap's `ModuleSet` is on
  the tape, but the earlier beacon's prints are not scanned, so a tape
  spanning a swap is short of index prints before it and nothing counts
  the gap. No mainnet perp has emitted `SetBeacon`, so no tape is short
  today. The fix is a two-phase scan — the perp first, each beacon over
  the span the swaps give it — and until then a fold that meets a swap to
  a beacon other than the one it was given counts the prints before it as
  a gap rather than reading an empty index.
- **Timestamps come per block when the provider omits them from logs**,
  read once per distinct block with bounded concurrency. A batched
  backfill is SDK #100.
- **The log source is `eth_getLogs` only.** An indexer-backed backfill
  behind the same interface is SDK #97; nothing above this module should
  care which source served the range.
- **Efficiency is measured by hand.** `ScanStats` reports what a scan
  cost, but nothing asserts a budget; the regression harness is SDK #99.
