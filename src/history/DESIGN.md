# `history`: the past tense

Up: the [root](../../DESIGN.md). Down: [history/tape](tape/DESIGN.md),
a market's record; [history/fold](fold/DESIGN.md), the algebra over it;
[history/replay](replay/DESIGN.md), the market rebuilt. Sideways:
[`events`](../events/DESIGN.md) for the vocabulary the tape speaks;
`feeds` for the present tense of the same; `transport` for the endpoint
the scans go through.

## Purpose

This module reads what already happened: every log a market, a beacon or
a token emitted over a block range of any length, through a provider
that caps how much it will return per request. It turns those logs into
the tape, a market's event history in chain order; the beacon's print
series; the token transfers between address sets; and the folds over
them that answer questions the chain itself cannot: who held a position
when, what a market's figures were, and what they are now.

It is the state half's counterpart: `client` reads what the chain holds
at a block, `history` reads what it emitted across blocks. Research is
built on this module; it is where a market's economics, its custody and
its series come from. This node is the aerial view, how the layers fit;
the three below it each own a layer.

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

**One contract above the readers.** Everything computed over the tape is
a `Fold`, in this crate and in the strategy layer, so the same code is a
batch computation over the past, a live one over the feed, and a parallel
one over segments. The readers produce rows; the folds consume them; the
handle never sees a fold.

## The mental model

Two pictures: how the record is read, and what is computed over it.

```text
   reading                                 History, the handle
                                           learned width · lag · in-flight bound · stats
                  ┌──────────────┬─────────────────┼─────────────────┐
                  ▼              ▼                 ▼                 ▼
               beacon        transfers           tape             recording
               prints        ERC-20 moves        the record       raw logs kept, decoded later
                  └──────────────┴────────┬────────┴─────────────────┘
                                          ▼
                                        scan        chunked eth_getLogs: learn the width, carve,
                                          │         narrow, deliver in range order
                                          ▼
                                 transport · events          the endpoint, the decoder

   computing
               tape ──── rows ────► fold ──── the algebra ────► replay
               TapeEvent in          Fold · Sequenced             nine folds, the reads' types,
               chain order           the shapes · Series          gaps counted; catch_up scans
                                     Arrivals · Reading           the tail through the handle
```

The scan is a search for the provider's limit, run while doing the work.
The range is carved into windows at the width the search currently
believes; a bounded number are in flight; each window narrows itself
when rejected and reports what it learned; results come back in range
order regardless. A newest-first scan is different on purpose: it exists
to stop early, so it sends one request at a time from the top and stops
the moment the caller has enough.

Over the scan sit the readers. Each is the same shape, a range, a filter,
the decoder, chain order. The **tape** is one market's logs from its three
addresses, merged on chain point; it has its own node,
[history/tape](tape/DESIGN.md). The **prints** are one beacon's
`IndexUpdated` logs as an index series. The **transfers** are one token's
`Transfer` logs between address sets. A **recording** is the tape's raw
logs kept on disk under a manifest, so a decoder fixed later reruns over
them and a market the cutover retires stays readable.

Over the tape sit the folds. The contract, the chain-order guard, the
shapes a fold's state takes and the series it keeps its history in are
[history/fold](fold/DESIGN.md). The market rebuilt from its events in the
types the pinned reads return, so the two compare with `==`, is
[history/replay](replay/DESIGN.md). Research's economics and
reconciliation are further folds over the same tape, and they live above
this crate because they apply a perspective.

`History` is the handle that owns the learned width, the block lag, the
in-flight bound and the telemetry, so that a long-lived scanner meters
its reads and a one-off tool gets the same correctness without the
state.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`History`](mod.rs#L133) | the scanning handle over a provider: learned width across scans; a block lag; bounded in-flight windows; cumulative stats. `market_tape` walks a range for three addresses at once; `record` is the same walk kept as raw logs | [`History::new`](mod.rs#L144) over any provider; [`ChainReader::history`](../client/DESIGN.md), the one handle a chain reader keeps so its scans share one learned width | [`Recording::check_tail`](recording.rs#L263), which rescans a recording's last blocks with the handle's learned width; otherwise its reads produce the rows the nodes below fold. The strategy layer's research sources take a reference to one so a run pays the width search once. |
| [`IndexPrint`](beacon.rs#L20) | one beacon update: index, chain point and timestamp. The index is a [`Price`](../units/DESIGN.md), the same type the live `IndexUpdated` carries, so a fold over an index series cannot tell which tense produced its samples; `index_f64` is the lossy view and the field was `index_x96` when it was a bare word | [`History::beacon_prints`](mod.rs#L218) and [`History::latest_beacon_prints`](mod.rs#L241) | nothing in the crate. The strategy layer's index series and estimator bootstrap; it keeps the chain point so a print can be joined to the fills after it. |
| [`TokenTransfer`](transfers.rs#L18) | one ERC-20 transfer between the address sets asked for; the sets are topic filters, not post-filters | [`History::token_transfers`](mod.rs#L328) | nothing in the crate. The strategy layer's fleet derivation and treasury ledger. |
| [`FakeNode`](test_support/mod.rs#L73), [`Mode`](test_support/mod.rs#L34) | an in-memory node that serves `eth_getLogs` under a chosen cap and answers as a provider would: accept, decline a too-wide range, or fail; behind the `test-utils` feature, beside the rows a fixture tape is built from and `assert_combine_law`, the fold law as a checker | [`FakeNode::new`](test_support/mod.rs#L86) from a set of logs and a span cap | every scan test in the crate, through the provider it hands out. The strategy layer's research tests scan against it too, which is why it is a feature and not a test module. |
| [`Recording`](recording.rs#L67), [`Manifest`](recording.rs#L33) | a market's raw logs over a range, each with its block's timestamp, under a manifest naming the chain, the three addresses, the range, the last block's hash, the recording crate's version and the undecodable count. Logs and never decoded rows, so a decoder fixed later reruns over them; a file short of a log is refused, not read as a shorter tape | [`History::record`](recording.rs#L105) over the handle; [`Recording::read`](recording.rs#L225) from a directory `Recording::write` made | [`Recording::tape`](recording.rs#L181), the decoded rows, with no node; [`Recording::check_tail`](recording.rs#L263). The strategy layer's forensic store: the markets a cutover retires are readable only from these. |
| [`TailCheck`](recording.rs#L75) | whether a recording's end still stands on the chain: the header at its last block has the manifest's hash, and a fresh scan of the last blocks returns the recorded logs. A reorg past the end or a node that served a short range fails one or both | [`Recording::check_tail`](recording.rs#L263) against a handle | nothing in the crate. The strategy layer's recording audit, which refuses to trust a file whose check fails. |
| [`ScanStats`](scan.rs#L241) | what a scan cost: requests, rejections, narrowings, the learned width; the number a collector meters. It also counts `undecodable`, logs of this vocabulary that would not decode, which the scan skips rather than dying over. A non-zero count is how a caller learns the tape it holds is short | [`History::stats`](mod.rs#L169) | nothing in the crate. The strategy layer's collector, and the benchmark suite, which asserts a scan's request count. |

The row, the three addresses and custody are in the tape's node; the
contract, the guard, the shapes and the series in the fold's; the replay
and its gaps in the replay's.

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
  milliseconds per hundred thousand events. The fold's own figures are
  in the replay's node.

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
- Down to [history/tape](tape/DESIGN.md): the handle's `market_tape`,
  `market_events` and `latest_market_events` are the readers there with
  the learned width, concurrency and telemetry added; `record` keeps the
  same walk.
- Down to [history/fold](fold/DESIGN.md): nothing directly. The handle
  produces rows and never sees a fold.
- Down to [history/replay](replay/DESIGN.md): `Replay::catch_up` scans
  the tail through the handle and the market's addresses.
- Out to the strategy layer: research sources, a market's tape, a walk
  over a wallet set's transfers, an index series, are scans through a
  `History` handle; an estimator bootstraps from `beacon_prints`; a
  treasury ledger reads `token_transfers`.

## Terminology

- **Scan**: a chunked `eth_getLogs` over a range. **Scan window**: one
  carve of the range at the believed width. **Width**: the span the
  provider currently accepts; **learn**: how the scan finds it.
- **Narrowing**: a scan window halving itself on rejection; the scan
  working, not the endpoint failing.
- **Print**: one beacon update. **Transfer**: one ERC-20 movement between
  address sets. **Recording**: a market's raw logs kept on disk under a
  manifest.
- **Newest-first**: a scan that stops early, sequential by design.
- **Lag**: blocks held back from the head so a scan's top is on every
  replica.

The tape's words, the row and chain order, are the tape node's; the
fold's words, the law, the shapes and the series, are the fold node's.

## Accepted structure

- **The free functions and the handle offer the same reads.** The handle
  adds memory, concurrency and telemetry, and a caller that scans once may
  not need them; a reader is written once and the handle wraps it with a
  shared width search and an in-flight bound.
- **The `latest_*` reads are sequential on every path**, because a
  request sent below the stopping point is waste, and parallelism there
  would be a bug dressed as a feature.

## Debts

- **Timestamps come per block when the provider omits them from logs**,
  read once per distinct block with bounded concurrency. A batched
  backfill is SDK #100.
- **The log source is `eth_getLogs` only.** An indexer-backed backfill
  behind the same interface is SDK #97; nothing above this module should
  care which source served the range.
- **Efficiency is measured by hand.** `ScanStats` reports what a scan
  cost, but nothing asserts a budget; the regression harness is SDK #99.
