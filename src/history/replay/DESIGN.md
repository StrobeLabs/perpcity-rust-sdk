# `history/replay`: a market rebuilt from its events

Up: [`history`](../DESIGN.md), for the scan `catch_up` runs through.
Sideways: [history/tape](../tape/DESIGN.md) for the rows this folds;
[history/fold](../fold/DESIGN.md) for the contract it implements, the
shapes it is built from and the series it keeps its history in;
[`client`](../../client/DESIGN.md) for the reads it is compared against;
[`events`](../../events/DESIGN.md) for the vocabulary.

## Purpose

A `Replay` is the market as its events describe it: the same quantities
the pinned reads return, rebuilt from the tape one event at a time instead
of read from storage. It exists so that one fold is the market's state in
every tense — the past from the tape, the present from the feed, a
counterfactual from an engine — and so that the fold can be checked: a
market rebuilt here and a market read from the chain at the same block
compare with `==`, to the atom.

## What matters

**Equality with the read is the test.** Every accessor returns the type
the corresponding read returns, and the two must be equal at a block or
the fold is wrong. That comparison found the one rule the fold needed
that no event states, and it is the audit the monitor runs on every
market.

**What the tape cannot carry is counted, never guessed.** A total no
event stated is `None`. A move no event carried is a count. A position
the fold met mid-life knows what moved and not where it stands, and says
so. A consumer gates on the counts, and the counts say which lever cures
each: the cutover, a seed, or the tape.

**The state is a monoid.** Every field is one of the shapes the
[fold node](../fold/DESIGN.md) names, so the fold of a tape cut anywhere
equals the merge of the folds of its pieces. That is what makes a prefix
a checkpoint, lets a seed and a tail meet in one value, and lets segments
fold on separate cores when a tape is long enough to want it.

## The mental model

Three pictures: where the events come from and where the answers go; what
the fold is made of; and the three places a fold can start.

### One fold, every tense

```text
   the tape                      the stamped feed                 an engine
   History::market_tape          MarketFeed::next_stamped         synthetic events
   the past, over a block range  the present, as each log lands   a counterfactual
             └──────────────────────────┬──────────────────────────────┘
                                        ▼
                             TapeEvent, in chain order
                                        │  apply
                                        ▼
                                     Replay
                                        │  the accessors, at the caller's block
                                        ▼
        MarketCapacity · Mark · SolvencyState · the pool's tick map and tick
        PositionState per id · Gaps
                                        │
                                        ▼
                     ==  StateAt, the pinned reads at the same block
```

One `apply` serves all three sources, which is why a fold written against
the past is correct in the present: the feed and the tape build the same
row through one constructor. The accessors return the reads' types and
take the caller's block, so the comparison at the bottom is `==` and not a
tolerance. That comparison is the test the type exists to pass, and the
monitor runs it on every live market.

### A composition of folds

```text
   Replay
   └─ Sequenced< Market >       an event at or before the last point is refused and counted
      └─ Market                 hands each event to nine folds; merges each with its twin

         fold           holds                                 shape            fed by
         ────────────   ───────────────────────────────────   ──────────────   ──────────────────────────
         Prices         pool price · index · stored EMAs      Series · Latest  swaps, prints, the touch
         Rates          funding · utilization fees · cumuls   Latest           the touch, accruals
         Utilization    capacity · open interest              Series           their updates
         Modules        the address in force, per kind        Latest, six      ModuleSet
         Solvency       margin · bad debt, and as stated      Stated · Series  transfers, swaps, bookings
         Pool           liquidity per tick · the tick         sums · Latest    ModifyLiquidity, TicksCrossed
         Positions      kind · size or band · margin, per id  First/Latest/sums  every position event
         OwnershipLog   custody over time                     append           PositionTransferred
         Activity       swaps · liquidations · settlements    Arrivals         the trades, the closes,
                        · prints                                               the prints
```

Nine folds because the market has nine concerns, and a concern's rule
belongs with its state. Where a concern's history is asked for as often
as its state, the fold keeps it: the prices, the capacity and the open
interest, and the stated books are `Series`, whose `latest()` is the old
answer; the trades, the liquidations, the settlements and the prints are
`Arrivals`. All of them append when segments combine, so the law holds
with them, and all trim to the replay's `Retention`. The one rule the live build needs that no event
states, how a swap's fees leave the margin total, lives in `Solvency` and
nowhere else. The three joins the live build's events force, a maker's
band from `ModifyLiquidity` by salt, its deposit price from the fold's own
pool price, a liquidation paired with the action before it, live in
`Positions` and `Pool`, so the cutover deletes them by removal. `Market`
itself is a struct of folds whose `apply` and `combine` are nine lines
each, and `Sequenced` is the chain-order guard any fold a driver feeds
directly can wear.

### Three starts

```text
   three starts, three left operands
     from_genesis(perp)      zeros stated, tick map whole, modules unknown      ─┐
     seeded(&StateAt)        the reads at block B, standing at the end of B     ─┼─► apply · combine · catch_up
     Fold::fold(segment)     what moved, nothing of where anything stands        ─┘
```

The law the [fold node](../fold/DESIGN.md) states holds here because
every field has one of its shapes. The hand-written merges that remain
are three, each because a value depends on more than its own field: a
position's kind, which a swap or a conversion changes; a maker's deposit
price, which is the pool price at the open and so is known to the segment
that held the price, not the one that saw the open; and a liquidation's
record, which a cut inside its transaction splits between the segment
that saw the close and the one that saw the dedicated event.

A start is a left operand. `from_genesis` is the market before its first
event; a `seeded` fold is the reads at a block, every total stated and
every level known, standing at the end of that block so the block's own
events delivered again are refused; the trait's `fold` is a segment that
knows what moved and nothing of where anything stands. From any of them,
`catch_up` applies the tape from the block after the fold's to the lagged
head and stands the fold at the end of that block, events or none, so
every series is known through it; `combine` takes a segment folded
elsewhere.

## Efficiency

The algebra is a cost model as much as a correctness one. The benchmark
in `benches/history_bench.rs` measures the stages apart, over a synthetic
tape of 200,000 events on a twelve-core laptop:

| stage | events per second |
|---|---|
| scan, through the fake node with no network under it | 0.77 M |
| decode, one thread | 31 M |
| decode, one thread per core | 170 M |
| fold, a no-op match per row | 87 M |
| `Replay`, one pass | 13 M |
| `Replay`, one segment per core, combined | 9.8 M |

**The fold is not where time goes.** `apply` is nine matches, a few
field writes and the pushes that keep the series; it allocates only for a
position first seen, a tick first touched, a custody record, and each
sample or arrival kept. On the recorded HORMUZ-SHIPS tape it runs at
62 M events per second, and a 10 M-event market folds in under a second
on one core. Keeping the series cost a quarter of the fold's throughput
on the synthetic tape and a third on the recorded one, which is the price
of answering "when" without re-reading the events; retention bounds what
it costs in memory. The scan is three orders slower with no network at
all, and a provider adds its latency on top, so the work a consumer
should avoid is re-scanning, not re-folding. That is what the checkpoint
half of the law buys: a fold of any prefix is a checkpoint, a seed is a
checkpoint the chain supplies, and `catch_up` scans only the blocks
since.

**The decode parallelizes; the fold's segments do not yet pay.** The
decoder is pure, so one thread per core gives five and a half times the
throughput. Folding segments on separate cores is slower than one pass
today, because `combine` on `Positions` merges one map into another,
proportional to the later segment's positions, and the synthetic tape
opens a position every four events, so the merge repeats the inserts; on
a short tape the threads cost more than the fold. Segments pay when a
tape is long relative to its positions, and the shape is there for that
day, asserted equal to the one pass in the benchmark. Measure before
reaching for it.

**Memory is per position, plus the tick map.** Each position is a
fixed-size state in a map keyed by id, each touched tick a pair of sums,
each custody record a point and an address. A closed position stays, which
is the first debt below. Position ids are sequential from the contract's
counter, so a `Vec` indexed by id is the layout to reach for if a
benchmark ever asks; one dispatch by event family in place of nine
matches is the other, and at these throughputs neither is measurable.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`Replay`](mod.rs#L143) | the market as its events describe it, in the reads' types, with the history of each figure kept as a series or as arrivals; every field combines across segments, the series by appending; an event at or before the fold's point is refused, never applied | [`Replay::from_genesis`](mod.rs#L225), [`Replay::seeded`](mod.rs#L254) from a [`StateAt`](../../client/DESIGN.md), or the trait's `fold`; `Replay::retaining` sets the `Retention`; then `apply` or `Replay::catch_up` | nothing in the crate; `Replay::deposited`, what bad debt is set against, is the one bare sum. The strategy layer's monitor, live cache and research folds, each driving it from a different source. |
| [`Positions`](positions.rs#L392), [`PositionState`](positions.rs#L114), [`PositionKind`](positions.rs#L22) | every position on the tape: a taker's size and USD leg, the sums of its swaps' deltas; a maker's band, its first range plus its liquidity changes; the margin a read supplied until an event touches it. Met mid-life or unnamed, a position is `Unknown` or unsized, never guessed, so segments merge | folded inside `Replay`; read through [`Replay::positions`](mod.rs#L473) and `Replay::position` | the strategy layer's live cache and economics, which price a taker's two legs at a mark; a `MakerBand` here and one read compare with `==`. |
| [`Liquidation`](activity.rs#L21), [`Settlement`](activity.rs#L41) | the marks the replay alone can make. A liquidation is one record per position per liquidating transaction, whichever build's events carried it, with the pool price before and after and the index then; a cut inside the transaction or before the first price is repaired at `combine`. A settlement is what a position paid or earned when touched. Both are `Positioned`, so custody scopes them; the swap mark is the tape's | the activity fold inside `Replay`, read through `Replay::liquidations` and `Replay::settlements` | the readings over arrivals in the strategy layer: intensity, a cascade's run, a sweep. |
| [`Gaps`](mod.rs#L63), [`Silences`](mod.rs#L77), [`Unknowns`](mod.rs#L93), [`Faults`](mod.rs#L108) | what the fold does not know, in three kinds with three cures: what the contract moved silently, cured by the cutover; what the fold's start did not supply, cured by a seed; what the driver did wrong, cured by `catch_up`. Decided at the read from the latest totals, so the counts combine like the rest | [`Replay::gaps`](mod.rs#L506) | nothing in the crate: the strategy layer's gate, one alarm per part. A reading with a nonzero gap is forensic, not a decision's input. |

## Accepted structure

- **The snapshots are the reads' types, with the caller's block.**
  `Replay::capacity_at`, `Replay::mark_at`, `Replay::solvency`,
  `Replay::emas`, `Replay::open_interest`, `Replay::cumulatives`,
  `Replay::funding_per_day` and `Replay::util_fee_per_day` each produce a
  type `StateAt` or the decoder also produces, so every snapshot type has
  two producers that must agree. The block is the caller's because the
  mark advances the EMAs to the block it is asked for, and a fold that
  stamped its own would compare unequal for the wrong reason. One level
  down, `PositionState::taker_size` is a `PerpDelta`,
  `PositionState::taker_usd` a `UsdcDelta`, `PositionState::deposit_pool_price`
  a `Price`, `Replay::pool_liquidity` an `LUnits`: the units the reads
  use, so `==` holds there too.

- **Three starts, one driver.** `Replay::from_genesis`, `Replay::seeded`
  on a `StateAt`, and the trait's `fold`; `Replay::catch_up` over the
  `History` and the market's `TapeAddresses` continues any of them. A seed
  stands at the end of its block, so that block's events delivered again
  are refused rather than counted twice, and a seeded fold continued over
  the tape equals the fold from genesis on every read-shaped question.

- **Three parts of the pool's liquidity are compensation.** `maker_band`
  comes from `ModifyLiquidity` joined to the maker by its salt, which the
  live build sets to the position id; the deposit price is the fold's own
  pool price at `MakerOpened`; a liquidation is paired with the action
  before it in the same transaction by the dedicated event that follows.
  The audited build emits range, liquidity and mark on `MakerOpened`, every
  change on the maker's own events, and liquidations as their own events,
  so the join, the recovery and the pairing are deleted at the cutover. They
  sit in the positions fold and the pool fold so the deletion is a removal.

## Debts

- **`Positions` never forgets.** A closed position stays in the map, so a
  long-lived fold's memory grows with the market's history. The live cache
  will want to retain only open positions; that is its call to make, not
  the fold's to guess.
- **The feed carries two of the three addresses**, so a fold the feed
  drives would hold a stale tick map and stale maker bands with no count
  saying so. Until the feed carries the PoolManager, or the audited build
  puts the liquidity on the perp's events, a live fold polls through
  `catch_up`.
- **A seeded fold is only ever the left operand of `combine`**, and nothing
  enforces it: merging a segment with a seed on the right would add a whole
  tick map onto partial sums. The sweep that cuts a tape into segments is
  the one caller, and the guard belongs with it.
- **The modules arrive as the contract's struct**, six bare addresses the
  fold keys by `ModuleKind`. A typed set keyed by kind, shared by the read
  and the `ModuleSet` event, is the right shape.
