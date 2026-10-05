# `history/fold`: the algebra

Up: [`history`](../DESIGN.md). Sideways: [history/tape](../tape/DESIGN.md)
for the rows a fold advances by; [history/replay](../replay/DESIGN.md)
for the fold that composes everything here into a market.

## Purpose

A fold is a state that advances by one event and merges with the fold of
the segment after it. This module is the contract, `Fold`; the guard that
holds every fold to chain order, `Sequenced`; the shapes a fold's state
takes so that merging is one line per field; and the two shapes a fold's
history takes, a `Series` for a value that holds between updates and
`Arrivals` for events that happen, with the `Reading` a question on either
returns.

## What matters

**One law.** `fold(a ++ b) == combine(fold(a), fold(b))` at any cut,
between blocks or inside one. That is what makes a fold of any prefix a
checkpoint, lets a seed and a tail meet in one value, and lets segments
fold on separate cores. The unit tests check it at every cut of a named
tape and the property tests at random cuts of random tapes; the first run
of the latter found a liquidation recorded twice when the cut fell inside
a transaction, which is why the law is stated at any cut and not only
between blocks. The checker is `assert_combine_law` in the history
node's test support, beside the rows a fixture tape is built from, so a
fold in the strategy layer is held to the same law over the same rows.

**Shapes make the law local.** Every field of a fold's state is a total
the contract emits whole, a value fixed by its first occurrence, a stated
total with what accrued since, a sum, or a history that appends. Each
shape carries the `combine` its law needs and nothing else, so a fold
built from them has a one-line merge per field and the law is checked
field by field. The merges written by hand are the few where a value
depends on another field, and the replay's node names them.

**History is kept, not re-read.** A fold's state answers what is; a
question about a market is as often about when. So a fold keeps the
history of what it already tracks, in chain order, appending across
segments so the law still holds, and trims it to a `Retention`, which is
how a monitor keeps hours and a forensic fold keeps a life.

**A reading carries its provenance.** A question on a series or arrivals
returns the value, the point it holds at, the timestamp, and the span of
samples it was computed from, so an alarm can name what moved it and a
forensic bin can reproduce it from the tape. It carries no bound: a bound
is the caller's policy.

## The mental model

```text
   a tape, cut anywhere

   ├──── segment a ────┤├──── segment b ────┤├──── segment c ────┤
         fold(a)              fold(b)              fold(c)          on three cores, or on three days
            └──── combine ───────┘                    │
                        └─────────── combine ─────────┘
                                     ‖
                              fold(a ++ b ++ c)

   the shapes, each one line of combine
     Latest<T>      later wins                        a total the contract emits whole
     First<T>       earlier wins                      an open, a range, a deposit price
     Stated<T, S>   a later statement replaces both;  a stated total and what accrued since
                    otherwise the accruals add
     sums           add                               liquidity per tick, size moved, counters
     Series<T>      append                            a value over time: a price, a stated total
     Arrivals<M>    append                            events over time: swaps, liquidations, prints

   the guard
     Sequenced<F>   an event at or before the last point applied is refused and counted
```

A `Series` holds a value between updates: `at(point)` and `at_time` answer
the last statement at or before, `change_over(window)` the value then and
now, `peak(window)` the largest in the window. `Arrivals` is a point
process with a mark per arrival: `count_in(window)`, `busiest(window)`,
the arrivals in a window. Conflating the two would make a nonsense query
representable, which is why they are two types. Both refuse a push at or
before their last point, as `Sequenced` refuses an event, and both answer
by binary search over samples kept in chain order.

A `Retention` is everything, or the last window plus the one sample before
it, so the window's start can be answered. A trimmed series says `None`
for what it dropped rather than answering with its earliest, since the
earliest kept is not the earliest that was.

## Efficiency

- **`apply` allocates nothing on the common path.** A shape's `set` or
  `state` is a field write; a series push is an append. What allocates
  is the first sight of something: a position, a tick, a custody record,
  and each sample or arrival kept.
- **Keeping history has a price, measured in the replay's node**: a
  quarter of the fold's throughput on a synthetic tape, a third on a
  recorded one. Retention bounds what it costs in memory.
- **Combining appends.** A series or arrivals of the later segment is
  moved onto the earlier one, so a combine costs the later segment's
  samples and nothing of the earlier's.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`Fold`](mod.rs#L34) | the one contract every fold over a tape shares: `apply` one event, `combine` the fold of the segment after, and the law `fold(a ++ b) == combine(fold(a), fold(b))` at any cut. An implementation allocates nothing on the common path of `apply` | the crate's two folds implement it, and the strategy layer's interpreters implement the same trait rather than a sibling | nothing takes it: a trait with one open verb, like `Factor` in `units`. `OwnershipLog` and `Replay` are the crate's instances; the strategy layer's folds are the rest, so one contract serves both repositories. |
| [`Latest`](shapes.rs#L10), [`First`](shapes.rs#L45), [`Stated`](shapes.rs#L85) | the three shapes a fold's state takes, each with the `combine` its law needs and nothing else: a total the contract emits whole, where the later segment wins; a value fixed by its first occurrence, where the earlier wins; a total stated outright with what accrued since, where a later statement replaces both | `Default` for the unknown; `Latest::stated` and `Stated::with` for a value a read supplies; `set` and `state` as events arrive | every component fold of `Replay`, and the strategy layer's folds: a fold built from these has a one-line `combine` per field and a law local to each. |
| [`Sequenced`](mod.rs#L59) | a fold behind the chain-order guard: an event at or before the last point applied is refused and counted, never handed on, in release builds as in debug. Chain order is the one assumption every sum in a fold rests on | [`Sequenced::new`](mod.rs#L68) over any fold; [`Sequenced::standing_at`](mod.rs#L78) for a fold seeded at a block, which refuses that block's own events as already in | `Replay`, which is one inside; the strategy layer's drivers, which wrap the fold they feed. |
| [`Series`](series.rs#L152), [`Sample`](series.rs#L73), [`Change`](series.rs#L143) | a value that holds between updates, in chain order: a push at or before the last point is refused; `at(point)` and `at_time` answer the last statement at or before; `change_over` yields the `Change` then and now; `peak` the largest in a window. Appends when segments combine, so a fold holding one keeps the law | `Series::stated` from a read's sample; `push` of each event's `sample` as events arrive, inside the component folds of `Replay` | the readings the strategy layer takes: a shock, a drawdown, a value at the moment of another event. |
| [`Window`](series.rs#L31), [`Retention`](series.rs#L62) | a length of time a question is asked over; and how much history a series keeps: everything, or the last window plus the sample before it, so the window's start can be answered and what was dropped is `None` | the caller: `Replay::retaining` for the fold, a window per question | every question on a series or arrivals; `retain` on each. |
| [`Arrivals`](series.rs#L322), [`Arrival`](series.rs#L309) | a point process in chain order, a mark per arrival: `count_in(window)`, `busiest(window)`, the arrivals in a window. Refuses disorder, appends across segments, trims to a retention, like [`Series`](series.rs#L152) | `push` of an `Arrival` inside `Replay`'s activity fold: swaps, liquidations, settlements, prints | the readings over arrivals: an intensity, a run, a sweep. |
| [`Reading`](series.rs#L116), [`Span`](series.rs#L107) | a value with its provenance: the units type, the point it holds at, the timestamp, the span of samples it was computed from. No bound: a bound is the caller's policy | every question on [`Series`](series.rs#L152) and [`Arrivals`](series.rs#L322) | the strategy layer's monitor, which judges it against a bound and names what moved it; a forensic bin, which reproduces it from the span. |

## Edges

- Up to [`history`](../DESIGN.md): nothing; the handle never sees a fold.
  The two meet in the replay's `catch_up`, which scans through the handle
  and applies to the fold.
- From [history/tape](../tape/DESIGN.md): the row a fold advances by,
  the point a series is ordered by, and `TapeEvent::sample`, which stamps
  a value with an event's point and time.
- To [history/replay](../replay/DESIGN.md): every component fold is
  built from the shapes and keeps its history in a series or arrivals;
  `Replay::retaining` carries one retention to all of them.
- Out to the strategy layer: its folds implement the trait and are built
  from the same shapes; its monitor takes readings and judges them
  against bounds of its own.

## Terminology

- **Fold**: a state that advances by one event and merges with the fold of
  the segment after it; **apply** and **combine**, the two verbs.
- **Segment**: a contiguous run of a tape; **cut**: where two segments
  meet, at any event; **checkpoint**: a fold of a prefix, which any later
  segment can continue from.
- **Shape**: what a field of a fold's state is, which decides its one-line
  `combine`; **the law**: that the fold of a concatenation is the
  combination of the folds.
- **Series**: a value that holds between updates, kept in chain order.
  **Arrivals**: a point process, a mark per arrival. **Sample** and
  **Arrival**: one element of each.
- **Window**, the type: a length of time a question is asked over, in
  block time. The scan window in the `history` node is a span of blocks,
  and is always called that.
- **Retention**: how much history is kept, everything or the last window.
  **Reading**: a value with its provenance; **Span**: the samples it was
  computed from; **Change**: a value then and now.

## Accepted structure

- **`Fold` names no other type in its signature, by design.** The trait
  takes a `TapeEvent` and returns `Self`; what a fold produces is the
  implementor's to say, so the trait itself connects to nothing in the
  graph and its instances, `OwnershipLog::apply` and `Replay::apply`, carry
  the edges. The same shape as `Factor` in `units`: one open verb, the
  types on the implementations.
- **`Series` and `Sample`, `Arrivals` and `Arrival`, flow both ways by
  design.** The collection takes its element in by `push` or `stated` and
  hands it back from `since`, `window` and `busiest`: one element type,
  one collection, no conversion between them to move. `TapeEvent::sample`
  and `TapeEvent::arrival` are how an event becomes a `Sample` or an
  `Arrival`; `change_over` is how a `Series` yields a `Change`;
  `Replay::retaining` is how a `Retention` reaches every series at once.
- **`Sequenced` takes a `ChainPoint` and hands one back.**
  `Sequenced::standing_at` takes the point a seeded fold stands at, and
  `Sequenced::point` says where the guard stands now. The point is the
  cursor, remembered, not converted, the same relation `OwnershipLog` has
  with its key in the tape's node.

## Debts

- **The random tapes live in one test file.** The property tests draw
  from a generator beside them; a fold in the strategy layer that wants
  random tapes writes its own. Sharing it would make `proptest` a
  dependency of the `test-utils` feature, which is the cost that has kept
  it where it is.
- **A trim is a drain from the front of a `Vec`.** Under a retention, a
  push that drops a sample shifts every kept sample. At a monitor's
  windows this is not measurable; a ring buffer is the fix if a long
  window over a busy market ever makes it so.
