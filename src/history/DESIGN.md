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
one block a provider will not serve rather than skipping it. A log the
decoder recognises but cannot decode is an error, not an omission.

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

| Type | What it is | The invariant it carries |
|---|---|---|
| [`History`] | the scanning handle over a provider | learned width across scans; a block lag; bounded in-flight windows; cumulative stats |
| [`ChainPoint`] | where an event sits | block number and log index; the total order events are joined on |
| [`TapeEvent`] | a `MarketEvent` at a chain point | the same vocabulary as the feed, plus its position |
| [`OwnershipLog`] | custody over time | a fold of transfers in chain order; owner at a point, not merely latest |
| [`IndexPrint`] | one beacon update | index, chain point and timestamp |
| [`TokenTransfer`] | one ERC-20 transfer | between the address sets asked for; the sets are topic filters, not post-filters |
| [`ScanStats`] | what a scan cost | requests, rejections, narrowings; the number a collector meters |

Two decisions shape the surface. The free functions and the handle offer
the same reads; the handle adds memory, concurrency and telemetry, and a
caller that scans once may not need them. And the `latest_*` reads are
sequential on every path, because a request sent below the stopping
point is waste, and parallelism there would be a bug dressed as a
feature.

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
- Out to Legion: the research crate's sources, the tape, the fleet walk,
  the index series, are scans through a `History` handle; the estimator
  bootstraps from `beacon_prints`; the treasury ledger reads
  `token_transfers`.

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
