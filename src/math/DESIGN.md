# `math`: the contract's arithmetic, without the contract

Up: the [root](../../DESIGN.md). Sideways: [`client`](../client/DESIGN.md)
fills the snapshots defined here; `contracts` and `storage` are the
shapes the ports were transcribed from; `types` and `convert` are the
human surface the f64 twins feed.

## Purpose

This module is the market's arithmetic as a pure function of its inputs:
what the contract would compute, computed here, from values a read
already fetched. It exists so that a strategy can ask "what would this
trade cost", "what is this maker worth", "how much can this band back",
"what is the mark right now" without sending anything and without
trusting a number the chain did not produce.

It is deliberately provider-free. Nothing in `math` sees a network, a
block, or a cache; a function here takes a snapshot and returns a result,
and the same call with the same snapshot returns the same result on any
machine at any time. That is what makes it testable against the chain: a
port is right when it reproduces a real on-chain outcome from the chain
state before it, and the golden tests are those reproductions.

## What matters

**Exactness is binary.** The contract settles in integers with defined
rounding. A port that is off by an atom is wrong: it disagrees with the
health check, the liquidation test, or the settle the chain will actually
perform. So the contract's arithmetic is transcribed one-to-one in `U256`
and `I256`, with the contract's rounding, the contract's overflow
behaviour, and the contract's order of operations, and every port is
anchored to a golden vector from real chain state: a maker settle
reproduced to the atom, a fair price matched against the deployed
module's `eth_call`, an EMA advance matched against a live accrue. The
f64 twins exist for simulators and dashboards and are named so that
nothing exact can be built on them by accident.

**A snapshot is a contract between a read and a computation.** The read
fills it at one block; the math trusts it completely. Every input a port
needs is a field on the snapshot, so the port's signature is its
dependency list and a missing input is a compile error, not a stale
value.

**Invariants are checked once, at the boundary.** A tick range with
`lower >= upper` is not a range, and every function that took two loose
ticks used to check it, or forgot to. `TickRange` is constructed once,
where the ticks entered, and the math downstream takes it and trusts it.
The same principle applies to every validated value: the check has one
home, the constructor, and the types make an unchecked value unavailable.

**Geometry is in pool-price space; value is at the mark.** A band's
capacity, a swap's path through the tick map, and the token amounts a
band holds are all functions of the pool price and the tick grid. A
position's value, its health and its settle are functions of the mark.
The module keeps the two apart by type: `PoolSnapshot` and the capacity
math take the pool price; `Mark` and the settle math take the mark, and
nothing here converts one into the other silently.

## The mental model

There are five kinds of computation here, and each is a submodule.

**Geometry** (`tick`, `range`, `liquidity`): the pool's price grid and a
maker's place on it. Ticks map to sqrt prices exactly as Uniswap defines
them; a `TickRange` is an aligned, validated interval; liquidity is the
pool's own unit `L`, sized from collateral and converted back to the
token amounts a band holds at a price.

```text
   tick        lower                        current                        upper
               │      spacing 30              │                              │
   ────────────┼──┼──┼──┼──┼──┼──┼──┼──┼──┼──┼──┼──┼──┼──┼──┼──┼──┼──┼──┼──┼──┼─────►
   price       1.0001^lower                pool price                 1.0001^upper
   sqrt × 2^96 √P_lower                       √P                        √P_upper

               │◄──────── L held as USD ─────►│◄────────── L held as perp ─────►│
               │  usd  = L · (√P − √P_lower)  │  perp = L · (1/√P − 1/√P_upper)  │
               │  backs shorts                │  backs longs                     │
               └──────────────────────────────┴──────────────────────────────────┘
                          MakerBand { range: TickRange [lower, upper), liquidity: L }
```

Three coordinate systems for one axis, and the geometry lives in the
third. A tick is an integer on a grid of spacing 30; its price is
`1.0001^tick`; the pool stores the square root of that price times
`2^96`, and every amount formula is linear in that square root or in
its reciprocal. That is why liquidity `L` is the pool's own unit rather
than a token amount: for a fixed `L`, the USD a band holds below the
price is a difference of square roots and the perp it holds above is a
difference of their reciprocals, so moving the price just slides the
boundary between the two legs. The lower leg is USD waiting
to buy, so it backs shorts; the upper leg is perp waiting to be sold, so
it backs longs. A `MakerBand` is the range and the `L`, and every sizing
and capacity function is one of these two formulas or its inverse.

**Capacity** (`capacity`): what a band can back. The perp its liquidity
spans above the pool price backs longs, below it backs shorts, fixed at
the moment the liquidity was placed and never re-evaluated. The market's
capacity is the running sum of those moments, which is why it can drift
from the sum of the live bands. Open interest draws on it; headroom is
the remainder; utilization is the ratio the fees module prices from.

**Pricing** (`pricing`): the three prices and the one relation. The pool
price and the index are observations; the EMAs are the contract's
smoothing of both, advanced from the last touch by the exact exponential
the contract uses; the mark is the fair price of all four. `Mark` is
those inputs at one block, and its fair price is what every health check
prices at. `fair_price_f64` is the lossy twin for simulators, marked
because the exact one is the default, and `Emas` is the f64 twin of the
stored pair, for a cache that follows the feed and must keep marking
between touches.

```text
      the pool                      the beacon
      poolState().ammPrice          index()
      ──────────┬─────────          ────┬────
                │ observed              │ observed
                ▼                       ▼
          ┌────────────┐          ┌───────────┐
          │ pool price │          │   index   │        at one block
          └─────┬──────┘          └─────┬─────┘
                │                       │
                │     stored EMAs       │
                │  (as of last touch)   │
                │     ┌───────────┐     │
                ├────►│ ema(pool) │◄────┤   advanced to the block's timestamp
                │     │ ema(index)│     │   by exp(−Δt / EMA_WINDOW)
                │     └─────┬─────┘     │
                │           │           │
                ▼           ▼           ▼
          ┌──────────────────────────────────┐
          │ fairPrice(pool, index,           │
          │           ema_pool, ema_index)   │   = the MARK
          └────────────────┬─────────────────┘
                           │
           ┌───────────────┴───────────────────┐
           ▼                                   ▼
     priced AT the mark                  lives IN pool-price space
     ──────────────────                  ────────────────────────
     health checks, valPnl               band geometry, capacity
     liquidation, backstop               the tick map, the quote
     utilization accrual                 what a trade moves
     maker equity                        where a band sits
```

Two things the picture says that the sentence cannot. The mark has a
time input: the stored EMAs are the contract's smoothing as of the last
touch, and the contract advances them to the block it is valuing at, so
between touches the mark drifts even when nothing trades, and a mark
computed from the last events alone is stale. And the fork at the bottom
is the whole reason the two prices have two names. Everything on the
left is a valuation the contract performs, and it uses the mark.
Everything on the right is geometry on the pool's own grid, and it uses
the pool price. `Mark` is the four inputs at one block;
`Mark::fair_price` is the price, exact, and `fair_price_f64` is the lossy
twin for simulators.

**The swap** (`swap`): what a taker trade does, computed by walking the
pool's tick map exactly as V4 does, in Q64.96 with V4's rounding, for the
two paths Perp City uses (exact-output buy, exact-input sell). A
`PoolSnapshot` is the pool at a block with its tick map; a quote is the
trade's deltas, the price it ends at, and which constraint stopped it.
The tick map reconciles with the pool's active liquidity or the snapshot
is rejected, because a map that does not add up is not this pool's.

**Settlement** (`maker_equity`): what the contract would credit a maker
if it were touched now. Funding accrued through the tick checkpoints,
utilization earnings from the capacity-weighted checkpoints, LP fees from
the pool's fee growth, and inventory marked at the mark, all in exact
atoms, from a `MakerMarketSnapshot` and per-position `MakerState` rows.
This is the port that reproduces a real liquidation to the atom, and it is
the port the next contract era makes unnecessary.

**Position arithmetic** (`position`): the taker-side derived values,
entry price, size, value, leverage, liquidation price, as plain functions
over the position's deltas. Older than the rest and shaped differently:
no snapshot, no exactness claim, f64 out.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`BlockContext`](mod.rs#L51) | one header: number, hash, timestamp; what every snapshot carries | [`StateAt::block`](../client/DESIGN.md), the handle's resolved header | every snapshot here carries one, so a caller can pin further reads to its hash: [`PoolSnapshot`](swap.rs#L71), [`MarketCapacity`](capacity.rs#L78), [`Mark`](pricing.rs#L114), [`TakerQuote`](swap.rs#L113), [`MakerMarketSnapshot`](maker_equity.rs#L65), and [`MarketSnapshot`](../types/DESIGN.md) in `types`; [`Mark::advanced`](pricing.rs#L134) advances the EMAs to its timestamp. |
| [`TickRange`](range.rs#L25) | a tick interval `[lower, upper)`: `lower < upper`, both in the V4 domain; private fields, checked at construction and on deserialise | [`TickRange::new`](range.rs#L37), the only door; deserialising runs the same check | [`MakerBand::new`](range.rs#L124), as a band's geometry; [`estimate_liquidity`](liquidity.rs#L30), [`liquidity_for_target_ratio`](liquidity.rs#L71) and [`liquidity_for_capacity`](capacity.rs#L166), which size liquidity for a range and would each re-check two loose ticks without it. The strategy layer's bands, corridors and ladders hold one rather than two ticks, which is the reason it is a type. |
| [`MakerBand`](range.rs#L115) | a range with liquidity: the shape `makerDetails` stores; the one type for a band wherever it appears | [`MakerBand::new`](range.rs#L124) from a [`TickRange`](range.rs#L25); [`StateAt::maker_band`](../client/DESIGN.md), one position's band at a block | [`band_capacity`](capacity.rs#L147), what the band can back at a pool price; [`band_amounts`](liquidity.rs#L207), the tokens standing in it. The strategy layer's maker views and discovered positions carry one, so a band read from the chain and a band a strategy plans are the same type. |
| [`PricePair`](pricing.rs#L41) | the contract's `(amm, index)` pair, `uint128` each, spot or EMA; narrowed from X96 with the contract's overflow rule | [`PricePair::try_from_x96`](pricing.rs#L56) from two X96 prices; [`calculate_emas`](pricing.rs#L71), the pair advanced | [`calculate_emas`](pricing.rs#L71), as the stored and the spot pair; [`Mark::advanced`](pricing.rs#L134), as the stored EMAs. The pair is one storage word on chain and advances as one value, so it is one type here. |
| [`Mark`](pricing.rs#L114) | what the contract marks from at a block: pool price, index and EMAs advanced to that block's timestamp; [`fair_price`](pricing.rs#L218) is the mark | [`Mark::advanced`](pricing.rs#L134) from the stored [`PricePair`](pricing.rs#L41) and a [`BlockContext`](mod.rs#L51); [`StateAt::mark`](../client/DESIGN.md) and [`MarketReader::get_mark`](../client/DESIGN.md), which read the views and advance them | the impact bounds inside the pool read and the maker-equity batch, which price at the mark; the strategy layer, whose health checks and basis must value at the mark and not the pool price. It exists so the three prices travel as one value from one block. |
| [`Emas`](pricing.rs#L169) | the stored EMA pair in human units with the touch it is current as of: the f64 twin of a [`PricePair`](pricing.rs#L41) at a `last_touch`; advancing it moves the touch | [`Emas::advanced`](pricing.rs#L186), the pair at a later time against the spot prices; the snapshot read in `client` and the `RatesAndEmasRefreshed` event carry one | nothing in the crate. The strategy layer's live cache, which holds one, replaces it on every touch the feed reports, and marks with it between touches; it is why a now-tense cache can price at the contract's mark and not the pool price. |
| [`Capacity`](capacity.rs#L49) | one band's or the market's backing: two [`PerpAtoms`](../units/DESIGN.md) legs, as `calcCapacity` computes them; a side is read with [`on`](capacity.rs#L58) rather than by naming its field | [`band_capacity`](capacity.rs#L147) for a band; the market-wide read narrows the contract's struct | [`MarketCapacity`](capacity.rs#L78), as its capacity leg; the strategy layer's sizing, which asks what a planned band would back before placing it. |
| [`MarketCapacity`](capacity.rs#L78) | capacity and its draw at a block: both from one block, both in [`PerpAtoms`](../units/DESIGN.md); headroom and utilization derived by [`headroom`](capacity.rs#L102) and [`utilization_e6`](capacity.rs#L117), never stored | [`StateAt::capacity`](../client/DESIGN.md), one multicall; [`MarketReader::get_capacity`](../client/DESIGN.md) as the convenience | nothing in the crate. The strategy layer's capacity gauges and utilization tiers, which need both legs from one block for the ratio to mean anything. |
| [`PoolSnapshot`](swap.rs#L71), [`TickLiquidity`](swap.rs#L25) | the pool at a block: price, active liquidity, a tick map of gross and net liquidity per initialised tick that reconciles with it, and the swap bounds | [`StateAt::pool`](../client/DESIGN.md); [`MarketReader::get_pool_snapshot`](../client/DESIGN.md); [`with_liquidity_delta`](swap.rs#L281), the same pool with a band added or removed | its own quotes, [`quote_perp`](swap.rs#L201) and [`quote_to_price`](swap.rs#L220); [`LiveTakerMarket::from_snapshot`](../feeds/DESIGN.md) and [`LiveTakerMarketPublisher::publish`](../feeds/DESIGN.md), which share it block-atomically. The strategy layer's depth probes and in-memory quoting hold one, which is why a quote needs no provider. |
| [`QuoteConstraints`](swap.rs#L51) | a quote's stopping rules: max size, max impact, the bounds | the caller; `Default` is no constraint | [`quote_to_price`](swap.rs#L220), which stops at whichever rule binds first and names it. |
| [`TakerQuote`](swap.rs#L113), [`QuoteLimit`](swap.rs#L34) | a swap's outcome and the rule that stopped it: the limit that bound the trade is named, not inferred | [`quote_perp`](swap.rs#L201) and [`quote_to_price`](swap.rs#L220) on a [`PoolSnapshot`](swap.rs#L71) | nothing in the crate. The strategy layer turns [`amt1_limit`](swap.rs#L148) into a trade's limit and reads the `QuoteLimit` it carries to know why a size was cut. |
| [`MakerMarketSnapshot`](maker_equity.rs#L65) | the settle's market-wide inputs from one block: the four accumulators as [`Funding`](../units/DESIGN.md), [`FundingPerSqrtPrice`](../units/DESIGN.md) and two [`Earnings`](../units/DESIGN.md), beside the tick, the pool price and the mark | the market-wide multicall of the maker-equity batch on the state handle in `client` | [`accrued`](maker_equity.rs#L391), which advances each accumulator by the growth the elapsed seconds imply. It is separate from the accrued snapshot so that a what-if mark cannot be applied before the replay. |
| [`AccrualInputs`](maker_equity.rs#L93) | what the accrual replay needs: the rates and EMAs the contract would use | the same multicall | [`MakerMarketSnapshot::accrued`](maker_equity.rs#L391), and nothing else. |
| [`AccruedMakerSnapshot`](maker_equity.rs#L473) | a market snapshot advanced to its block: the accrual replay has run; a what-if mark applies only after it | [`MakerMarketSnapshot::accrued`](maker_equity.rs#L391); [`with_mark`](maker_equity.rs#L491) for a what-if | [`AccruedMakerSnapshot::maker_equity`](maker_equity.rs#L504), once per position. The order accrue, then mark, then settle is the contract's, and the two types make it the only order available. |
| [`MakerState`](maker_equity.rs#L125), [`TickFunding`](maker_equity.rs#L50) | one position's settle inputs: its band, margin, and its checkpoint of every accumulator the market keeps — four [`Funding`](../units/DESIGN.md) counting the two ticks', a [`FundingPerSqrtPrice`](../units/DESIGN.md), two [`Earnings`](../units/DESIGN.md) and two [`FeeGrowth`](../units/DESIGN.md). A checkpoint is only ever read as the thing the market's level is measured against | the batch's maker rows and the two storage reads on the state handle in `client` | [`AccruedMakerSnapshot::maker_equity`](maker_equity.rs#L504), the settle preview, which takes each difference with that accumulator's own rule. |
| [`MakerEquityBreakdown`](maker_equity.rs#L189) | a settle previewed: exact atoms in the contract's units; f64 only in accessors | [`AccruedMakerSnapshot::maker_equity`](maker_equity.rs#L504) | [`MakerEquityKind`](../client/DESIGN.md), as the `Computed` payload of an outcome; the strategy layer's equity audits and liquidation decisions, which key on `is_liquidatable` and the margin ratio. Exact atoms so that a preview can be checked against a real settle to the atom. |

Two conventions carry the exactness claim. The type is the first: a
quantity's unit is its type, taken from [`units`](../units/DESIGN.md), and
the `f64` twin is the one that names itself so — `fair_price_f64` beside
`fair_price`, `usdc()` beside `atoms()`. Where a quantity has no unit type
yet, a wire suffix (`_x96`, `_x128`, `_atoms`, `_e6`) stands in and means
the same thing, exact in the contract's own encoding, which is why those
suffixes disappear as the types land. And every port names the contract
function it transcribes in its doc, with the commit it was transcribed
from, so that a contract change is a search rather than a hunt.

## Efficiency

This module spends no requests; its currency is time on the caller's
thread, and everything here is integer arithmetic in `U256` and `I256`.

- **A swap quote walks the tick map.** Cost is linear in the initialized
  ticks the trade crosses, each step a few 512-bit multiply-divides. The
  map itself is fixed by the tick spacing: at spacing 30 the whole
  domain is 36 bitmap words, so a `PoolSnapshot` is small and a quote
  over it is microseconds, which is what lets a taker quote in memory on
  every event.
- **The mark is one exponential.** Advancing the EMAs is a Solady
  `expWad` and two weighted sums; the fair price is an average. Cheap
  enough to compute at every read rather than cache.
- **A settle preview is constant per position** over its chunk's rows:
  a fixed number of checkpoint differences and one valuation at the
  mark. The batch's cost is in the reads that fill the snapshot, not in
  the arithmetic.
- **Geometry is closed-form.** Capacity, band amounts and liquidity
  sizing are single formulas in square-root prices; the inverse for a
  capacity target is a division, not a search.

Nothing here allocates on the hot path beyond the tick map a snapshot
already owns, and nothing reads a clock.

## Edges

- From the [root](../../DESIGN.md): exactness, the unit boundary, the
  snapshot shape.
- From [`client`](../client/DESIGN.md): every snapshot here is filled by
  a read there. The contract between the two is the snapshot type; `math`
  never learns where its inputs came from.
- To `contracts`: the Solidity each port transcribes, and the ABI structs
  a snapshot's fields are narrowed from. `storage` derives the slots the
  settle's inputs are read from; the math does not know a slot exists.
- To `types` and `convert`: the f64 twins and the unit conversions live
  at the surface. `math` produces exact values and offers f64 accessors;
  `convert` is where a caller's human input becomes a wire unit.
- Out to the strategy layer: a strategy's band is a `MakerBand`, its
  sizing goes through `estimate_liquidity`, its quoting through
  `PoolSnapshot`, its maker valuation through `MakerEquityBreakdown`. The
  layer above ports no contract math; where a consumer once did, that was
  the defect this module fixes.

## Terminology

- **Range**: a validated tick interval, half-open. **Band**: a range with
  liquidity. **Liquidity**: the pool's unit `L`.
- **Sqrt price**: `sqrt(price) × 2^96`. **Tick**: `price = 1.0001^tick`,
  aligned to the spacing.
- **Capacity**: what a band can back, fixed when placed. **Headroom**:
  capacity less open interest. **Utilization**: open interest over
  capacity.
- **Pool price, index, EMAs, mark, fair price**: as the root defines them;
  this module is their home.
- **Advance**: moving the stored EMAs from the last touch to a timestamp
  by the contract's exponential.
- **Tick map**: the pool's initialized ticks with their gross and net
  liquidity; **reconcile**: the check that the map's net up to the current
  tick equals the pool's active liquidity.
- **Quote**: a simulated swap's outcome; **limit**: the constraint that
  stopped it.
- **Settle**: what the contract credits a maker on a touch; **preview**:
  the same, computed here; **accrual**: the replay of funding and fees
  from the last touch to the block.
- **Golden vector**: a real on-chain outcome a port must reproduce
  exactly.

## Debts

- **`position` is from an earlier era of the crate.** It takes loose
  deltas, returns f64, claims no exactness and has no snapshot. It should
  either become exact and snapshot-fed like its siblings or be moved to
  the f64 surface where its claims are honest.
- **`liquidity_for_target_ratio` cannot express the deployed maker
  case.** Makers on the deployed markets are fully collateralised, a
  ratio of 1.0, which the function's domain excludes; callers size makers
  through `estimate_liquidity` and a buffer instead.
- **The settle preview is deployed-era compensation.** The next contract
  era exposes `previewPosition`; when it ships, `maker_equity` and the
  storage reads behind it are deleted rather than migrated.
- **`PoolSnapshot` carries the swap bounds as four loose fields.** The
  protocol's terminal prices and the price-impact module's bounds are
  two pairs with different provenance and should be two validated values.
