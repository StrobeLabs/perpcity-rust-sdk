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

Over the tape sit the folds. `OwnershipLog` folds the position NFT's
transfers into custody over time, so `owner_at(pos, point)` answers who
held a position when an event happened, which is the attribution a
measurement wants, and `latest_owner` answers the naive question for
compatibility. Research's economics and reconciliation are further folds
over the same tape, and they live above this crate because they apply a
perspective.

`History` is the handle that owns the learned width, the block lag, the
in-flight bound and the telemetry, so that a long-lived scanner meters
its reads and a one-off tool gets the same correctness without the
state.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`History`](mod.rs#L115) | the scanning handle over a provider: learned width across scans; a block lag; bounded in-flight windows; cumulative stats | [`History::new`](mod.rs#L126) over any provider; [`ChainReader::history`](../client/DESIGN.md), the one handle a chain reader keeps so its scans share one learned width | nothing takes it; its reads produce the series below. The strategy layer's research sources take a reference to one so a whole run pays the width search once. |
| [`ChainPoint`](tape.rs#L47) | where an event sits: block number and log index; the total order events are joined on | [`TapeEvent::point`](tape.rs#L71) | [`OwnershipLog::owner_at`](tape.rs#L136), custody at that point. The strategy layer's joins, a print against the fills after it, are on this key, which is why it is a type and not two fields. |
| [`TapeEvent`](tape.rs#L56) | a [`MarketEvent`](../events/DESIGN.md) at a chain point: the same vocabulary as the feed, plus its position and timestamp | [`History::market_events`](mod.rs#L247) and [`History::latest_market_events`](mod.rs#L270), and their handle-less twins | [`OwnershipLog::fold`](tape.rs#L112). The strategy layer's economics, classification and series are folds over a slice of these; every fold that depends on order can assert it. |
| [`OwnershipLog`](tape.rs#L100) | custody over time: a fold of transfers in chain order; owner at a point, not merely latest | [`OwnershipLog::fold`](tape.rs#L112) over the tape | nothing in the crate. The strategy layer's attribution, which asks who held a position when a trade happened, not who holds it now. |
| [`IndexPrint`](beacon.rs#L20) | one beacon update: index, chain point and timestamp. The index is a [`Price`](../units/DESIGN.md), the same type the live `IndexUpdated` carries, so a fold over an index series cannot tell which tense produced its samples; `index_f64` is the lossy view and the field was `index_x96` when it was a bare word | [`History::beacon_prints`](mod.rs#L200) and [`History::latest_beacon_prints`](mod.rs#L223) | nothing in the crate. The strategy layer's index series and estimator bootstrap; it keeps the chain point so a print can be joined to the fills after it. |
| [`TokenTransfer`](transfers.rs#L18) | one ERC-20 transfer between the address sets asked for; the sets are topic filters, not post-filters | [`History::token_transfers`](mod.rs#L287) | nothing in the crate. The strategy layer's fleet derivation and treasury ledger. |
| [`FakeNode`](test_support.rs#L68), [`Mode`](test_support.rs#L29) | an in-memory node that serves `eth_getLogs` under a chosen cap and answers as a provider would: accept, decline a too-wide range, or fail; behind the `test-utils` feature | [`FakeNode::new`](test_support.rs#L81) from a set of logs and a span cap | every scan test in the crate, through the provider it hands out. The strategy layer's research tests scan against it too, which is why it is a feature and not a test module. |
| [`ScanStats`](scan.rs#L241) | what a scan cost: requests, rejections, narrowings, the learned width; the number a collector meters. It also counts `undecodable` — logs of this vocabulary that would not decode, which the scan skips rather than dying over, since one such log should not cost a scan of millions of blocks. A non-zero count is how a caller learns the tape it holds is short | [`History::stats`](mod.rs#L151) | nothing in the crate. The strategy layer's collector, and the benchmark suite, which asserts a scan's request count. |

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
  regression harness (SDK #99) will assert.

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
- **Fold**: a pass over a tape that produces a derived view; the
  ownership log is the one that lives here.
- **Newest-first**: a scan that stops early, sequential by design.
- **Lag**: blocks held back from the head so a scan's top is on every
  replica.

## Accepted structure

- **`ChainPoint` goes into `OwnershipLog` and comes back out of it.**
  It is the coordinate the fold is sorted by: `owner_at` takes one and
  searches, and `transfers` hands back the point of every change. A
  sorted index takes its key and returns it, so the cycle is the index
  relation rather than a conversion with two homes.

## Debts

- **A tape is one address's logs.** The beacon's prints and the pool's
  liquidity changes are separate scans over the same range; a
  market-shaped scan that walks the range once for several addresses is
  SDK #101, and research's access pattern will decide its shape.
- **Timestamps come per block when the provider omits them from logs**,
  read once per distinct block with bounded concurrency. A batched
  backfill is SDK #100.
- **The log source is `eth_getLogs` only.** An indexer-backed backfill
  behind the same interface is SDK #97; nothing above this module should
  care which source served the range.
- **Efficiency is measured by hand.** `ScanStats` reports what a scan
  cost, but nothing asserts a budget; the regression harness is SDK #99.
