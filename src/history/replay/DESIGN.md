# `history/replay`: a market rebuilt from its events

Up: [`history`](../DESIGN.md), for the tape this folds and the `Fold`
contract it implements. Sideways: [`client`](../../client/DESIGN.md) for
the reads it is compared against, [`events`](../../events/DESIGN.md) for
the vocabulary.

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

**The state is a monoid.** Every field is a latest-wins total, a sum, or
a first occurrence, so the fold of a tape cut at any block boundary equals
the merge of the folds of its pieces. That is what makes a prefix a
checkpoint, lets segments fold on separate cores, and lets a seed and a
tail meet in one value.

## The mental model

```text
          TapeEvent  ─── from the tape, the stamped feed, or an engine
              │
              ▼
   Replay = Sequenced< Market >        an event at or before the last point is refused and counted
              │
   ┌──────────┴─────── one event, eight independent folds ───────────────────────────┐
   │ Prices        pool price · index · stored EMAs             ← swaps, prints, touch │
   │ Rates         funding · utilization fees · cumulatives     ← the touch, accruals  │
   │ Utilization   capacity · open interest                     ← their updates        │
   │ Modules       the six addresses in force                   ← ModuleSet            │
   │ Solvency      margin and debt, stated, with what moved since ← transfers, swaps,  │
   │                 and the one live-build rule: when a swap's fees leave the total    │
   │ Pool          liquidity per tick (signed sums) · the tick  ← ModifyLiquidity,     │
   │                                                               TicksCrossed        │
   │ Positions     each position: kind · size or band · margin  ← every position event │
   │ OwnershipLog  custody over time                            ← PositionTransferred  │
   └─────────────────────────────────────────────────────────────────────────────────┘
              │
              ▼  the accessors return the reads' types, stamped with the caller's block
   capacity_at → MarketCapacity    mark_at → Mark    solvency → SolvencyState
   pool_ticks · pool_tick · pool_liquidity → what PoolSnapshot holds
   position(id) → PositionState { taker_size → PerpDelta, maker_band → MakerBand, margin }
   gaps → Gaps { silences, unknowns, faults }
              │
              ▼
          == StateAt, the pinned reads at the same block
```

Three starts, one driver. `from_genesis` is the market before its first
event: the totals that are zero before any event are zero and stated. A
`seeded` fold is the reads at a block: every total stated, the tick map
whole, every position's level and margin known, standing at the end of
that block. The trait's `fold` is a segment: it knows what moved and
nothing of where anything stands. From any start, `catch_up` applies the
tape from the block after the fold's to the lagged head.

Each component fold is built from the three shapes in `history::fold`:
`Latest` where the later segment wins, `First` where the earlier does,
`Stated` for a total with what accrued since it. Hand-written merge logic
remains in two places only, each because a value depends on more than its
own field: a position's kind, which a swap or a conversion changes, and a
maker's deposit price, which is the pool price at the open and so is known
to the segment that held the price, not the one that saw the open.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`Replay`](../replay.rs#L124) | the market as its events describe it, in the reads' types; every quantity a latest-wins total, a sum or a first occurrence, so the fold combines across segments; an event at or before the fold's point is refused, never applied | [`Replay::from_genesis`](../replay.rs#L184) before the first event; [`Replay::seeded`](../replay.rs#L213) from the reads on a [`StateAt`](../../client/DESIGN.md); the trait's `fold` for a segment; then `apply`, or `Replay::catch_up` over the tape | nothing in the crate. The strategy layer's monitor, live cache, research folds and backtests, each driving it from a different source. |
| [`Positions`](positions.rs#L367), [`PositionState`](positions.rs#L109), [`PositionKind`](positions.rs#L22) | every position the tape mentioned: a taker's size as the sum of its swaps, a maker's band as its first range and the sum of its liquidity changes, the margin a read supplied until an event touches it. A position met mid-life, or a kind the tape never named, is `Unknown` or unsized rather than guessed, so segments merge into the whole | folded inside `Replay`; read through [`Replay::positions`](../replay.rs#L378) and `Replay::position` | the strategy layer's live cache, whose own position fold this replaces, and its economics folds. A `MakerBand` here and one read from chain compare with `==`. |
| [`Gaps`](../replay.rs#L51), [`Silences`](../replay.rs#L65), [`Unknowns`](../replay.rs#L81), [`Faults`](../replay.rs#L96) | what the fold does not know, in three kinds with three cures: what the contract moved silently, cured by the cutover; what the fold's start did not supply, cured by a seed; what the driver did wrong, cured by `catch_up`. Decided at the read from the latest totals, so the counts combine like the rest | [`Replay::gaps`](../replay.rs#L411) | nothing in the crate: the strategy layer's gate, one alarm per part. A reading with a nonzero gap is forensic, not a decision's input. |

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
  `PositionState::deposit_pool_price` a `Price`, `Replay::pool_liquidity`
  an `LUnits`: the units the reads use, so `==` holds there too.

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

- **The feed carries two of the three addresses**, so a fold the feed
  drives would hold a stale tick map and stale maker bands with no count
  saying so. Until the feed carries the PoolManager, or the audited build
  puts the liquidity on the perp's events, a live fold polls through
  `catch_up`.
- **`Positions` never forgets.** A closed position stays in the map, so a
  long-lived fold's memory grows with the market's history. The live cache
  will want to retain only open positions; that is its call to make, not
  the fold's to guess.
- **A seeded fold is only ever the left operand of `combine`**, and nothing
  enforces it: merging a segment with a seed on the right would add a whole
  tick map onto partial sums. The sweep that cuts a tape into segments is
  the one caller, and the guard belongs with it.
- **The modules arrive as the contract's struct**, six bare addresses the
  fold keys by `ModuleKind`. A typed set keyed by kind, shared by the read
  and the `ModuleSet` event, is the right shape.
