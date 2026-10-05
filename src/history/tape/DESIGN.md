# `history/tape`: a market's record

Up: [`history`](../DESIGN.md), for the scan that reads the record and the
handle that owns the scan. Sideways: [`events`](../../events/DESIGN.md)
for the vocabulary each row speaks; [history/fold](../fold/DESIGN.md)
for what is computed over the rows; [history/replay](../replay/DESIGN.md)
for the fold that rebuilds the market from them.

## Purpose

The tape is everything the chain said about one market, in the order it
said it. A market's record is spread over three addresses: its own
contract, the beacon it reads its index from, and the chain's PoolManager,
which logs every pool's liquidity under the pool's id. This module names
the row, `TapeEvent`; the coordinate rows are ordered and joined by,
`ChainPoint`; the three addresses, `TapeAddresses`; the readers that
produce rows from a block range; and custody, `OwnershipLog`, the first
fold over them.

## What matters

**One row, every tense.** A scan's row, a feed's and a recording's are
built through one constructor, so a fold written against the past is
correct in the present, and a file decodes to the tape the chain would
have given. The row carries its block's hash, so a state rebuilt from the
tape can carry a pinned read's block identity, and its transaction, so a
fold can pair one call's logs.

**Chain order is the join key.** A print and a fill in the same block are
ordered by log index; custody at the moment of a trade is the transfer
before it. Every row carries its chain point, every reader returns rows in
that order, and the folds that depend on it assert it.

**A short tape says so.** A log of this vocabulary that will not decode is
counted on the scan's stats and skipped, never dropped in silence and
never fatal: one unreadable log should not cost a scan of millions of
blocks, but a consumer must be able to learn that the tape it holds is
short.

**Trades name positions, not wallets.** No trade event carries an address.
Who did what is a join through the position NFT's transfers, which is why
custody is a fold of the tape and lives beside the row rather than above
the crate.

## The mental model

```text
   the perp                the beacon                the PoolManager
   every market event      IndexUpdated              ModifyLiquidity, for this pool id
         └─────────────────────┬──────────────────────────────┘
                               │  two filters over one range, one learned width
                               ▼
                    logs, merged on (block, log index)
                               │  the feed's decoder; stamped with block hash and time
                               ▼
                    TapeEvent, in chain order                      the tape
                               │
                 ┌─────────────┴─────────────┐
                 ▼                           ▼
           OwnershipLog                    Replay                  the folds over it
           who held each position when     the market rebuilt
```

A reader is a range, a filter, the decoder, chain order. `market_events`
is the one-address scan, the perp alone. `market_tape` is the whole
record: the perp and the beacon share a filter by address, the PoolManager
is filtered by the liquidity event and the pool id it indexes, and the two
scans merge on chain point. `latest_market_events` reads newest-first and
stops when it holds enough. The `History` handle offers the same three
reads with the learned width, concurrency and telemetry added, and a
`Recording` keeps the raw logs so the decode can run again with no node.

Custody is the tape's own fold. A mint is a transfer from the zero address
and a burn a transfer to it, so `owner_at(pos, point)` is the recipient of
the last transfer at or before the point, `None` before the mint and after
the burn. That is the attribution a measurement wants, who held the
position when the trade happened, and `latest_owner` is the naive question
kept for compatibility.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`ChainPoint`](mod.rs#L63) | where an event sits: block number and log index; the total order events are joined on | [`TapeEvent::point`](mod.rs#L90) | [`OwnershipLog::owner_at`](custody.rs#L78), custody at that point. The strategy layer's joins, a print against the fills after it, are on this key, which is why it is a type and not two fields. |
| [`Tape`](mod.rs#L181), [`TapeSlice`](mod.rs#L275) | a market's record: rows in strict chain order, one hash and timestamp per block, timestamps never decreasing; checked once at `Tape::new`. `TapeSlice` is a run of its rows, what a `Tape` derefs to and a segment is; it groups by block, transaction and window, cuts between blocks, looks up by point, block and time, and lenses its swaps, its prints and one position's rows | [`History::market_tape`](../DESIGN.md), [`History::market_events`](../DESIGN.md) and [`History::latest_market_events`](../DESIGN.md); [`Recording::tape`](../DESIGN.md), with no node; [`Tape::new`](mod.rs#L190) from rows a caller holds | every fold, through `Fold::fold`; `Replay::catch_up`. The strategy layer's sources hand one out and its interpreters take a slice. |
| [`TapeEvent`](mod.rs#L72) | a [`MarketEvent`](../../events/DESIGN.md) at a chain point, with its block's hash and timestamp and its transaction. Every tense builds it through one constructor, so a scan's row, a feed's and a recording's are the same row; `sample` stamps a value with its point and time, `arrival` a mark with its transaction too | the rows of a [`Tape`](mod.rs#L181), which every reader returns; [`MarketFeed::next_stamped`](../../feeds/DESIGN.md), the present tense | [`OwnershipLog::fold`](custody.rs#L66). The strategy layer's economics, classification and series are folds over a slice of these. |
| [`Swap`](lenses.rs#L28), [`SwapAction`](lenses.rs#L17) | a taker's swap as the event settled it: the position, whether it opened, adjusted or closed, and the `SwapInfo`. The one mark a tape yields unaided, so the lens and the replay's activity fold speak one record | [`Swap::of`](lenses.rs#L39) from a taker's event; the `swaps` lens, as arrivals | `Replay::swaps`, the arrivals the replay keeps. The strategy layer's flow readings, over either. |
| [`TapeAddresses`](mod.rs#L161) | the three addresses a market's record is spread across: its own contract, the beacon it reads, and the chain's PoolManager keyed by its pool id. The PoolManager is every pool on the chain, so it is filtered by the liquidity event and the pool id it indexes, never by address alone | [`MarketReader::tape_addresses`](../../client/DESIGN.md), which knows all three; or a caller that does | [`History::market_tape`](../DESIGN.md) and [`market_tape`](read.rs#L85). |
| [`OwnershipLog`](custody.rs#L33) | custody over time: a fold of transfers in chain order; owner at a point, not merely latest. The first instance of [`Fold`](../fold/DESIGN.md): `apply` appends a transfer, `combine` appends a later segment's timelines | [`OwnershipLog::fold`](custody.rs#L66) over the tape, the trait's fold kept inherent so it is reachable without the import | [`Replay`](../replay/DESIGN.md), which folds it in the same pass. The strategy layer's attribution, which asks who held a position when a trade happened, not who holds it now. |

## Edges

- Up to [`history`](../DESIGN.md): the readers run on its scan and share
  its learned width through the handle; `History::market_tape` is the
  read the strategy layer takes, and `History::record` keeps the same
  walk as raw logs.
- From [`events`](../../events/DESIGN.md): every log a reader returns is
  decoded there. Nothing here interprets a log.
- From [`client`](../../client/DESIGN.md): `MarketReader::tape_addresses`
  fills the three addresses, which only the client knows.
- Down to [history/fold](../fold/DESIGN.md): `OwnershipLog` implements
  the trait, and `TapeEvent::sample` is how an event's value enters a
  series.
- To [history/replay](../replay/DESIGN.md): the rows it folds, the
  custody it folds in the same pass, the addresses `catch_up` scans.
- Out to the strategy layer: economics, classification and attribution
  are folds over a slice of rows, joined to custody by chain point.

## Terminology

- **Tape**: one market's events in chain order; **row**: one `TapeEvent`.
- **Chain point**: block and log index; **chain order**: the order they
  induce, the join key for everything.
- **Print**: one beacon update, which the tape carries as `IndexUpdated`.
- **Custody**: who holds a position; **mint** and **burn**: the transfers
  from and to the zero address that begin and end it.
- **Undecodable**: a log of this vocabulary the decoder refused, counted
  by the scan and skipped by the tape.

## Accepted structure

- **The market's tape is two filters over one range.** The perp and its
  beacon share a filter by address; the PoolManager, being every pool on
  the chain, is filtered by the liquidity event's signature and the pool
  id it indexes. The two scans share the learned width and their rows
  merge on chain point. One filter would either miss the pool's liquidity
  or pull every pool's on the chain. The PoolManager filter is compensation
  for the live builds, whose maker events carry no geometry: the next
  contracts emit a band's range, liquidity and every change to it on the
  perp's own events, so the pool's liquidity becomes a fold of one address
  and the second filter goes with the cutover.
- **`ChainPoint` goes into `OwnershipLog` and comes back out of it.** It
  is the coordinate the fold is sorted by: `owner_at` takes one and
  searches, and `transfers` hands back the point of every change. A sorted
  index takes its key and returns it, so the cycle is the index relation
  rather than a conversion with two homes.
- **`Tape` and `TapeEvent` flow both ways by design.** `Tape::new` takes
  rows in, and `into_vec` and the slice hand them back: one row type, one
  collection, as `Series` and `Sample` in the fold node. `TapeSlice::to_owned`
  is how a run of rows becomes a tape of its own, as `str` to `String`.

## Debts

- **The market tape reads one beacon: the one named at the call.** A
  market's beacon is governance's to swap, and a swap's `ModuleSet` is on
  the tape, but the earlier beacon's prints are not scanned, so a tape
  spanning a swap is short of index prints before it and nothing counts
  the gap. No mainnet perp has emitted `SetBeacon`, so no tape is short
  today. The fix is a two-phase scan, the perp first and each beacon over
  the span the swaps give it; until then a fold that meets a swap to a
  beacon other than the one it was given counts the prints before it as a
  gap rather than reading an empty index.
