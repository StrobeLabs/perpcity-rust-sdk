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
prices at. The f64 `fair_price` is the twin for simulators.

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

| Type | What it is | The invariant it carries |
|---|---|---|
| [`TickRange`](range::TickRange) | a tick interval `[lower, upper)` | `lower < upper`, both in the V4 domain; private fields, checked at construction and on deserialise |
| [`MakerBand`](range::MakerBand) | a range with liquidity | the shape `makerDetails` stores; the one type for a band wherever it appears |
| [`PricePair`](pricing::PricePair) | the contract's `(amm, index)` pair | `uint128` each, spot or EMA; narrowed from X96 with the contract's overflow rule |
| [`Mark`](pricing::Mark) | what the contract marks from at a block | pool price, index and EMAs advanced to that block's timestamp; [`fair_price_x96`](pricing::Mark::fair_price_x96) is the mark |
| [`Capacity`](capacity::Capacity) | one band's or the market's backing | long and short in perp atoms, as `calcCapacity` computes them |
| [`MarketCapacity`](capacity::MarketCapacity) | capacity and its draw at a block | both from one block; headroom and utilization derived, never stored |
| [`PoolSnapshot`](swap::PoolSnapshot) | the pool at a block | price, active liquidity, a tick map that reconciles with it, and the swap bounds |
| [`TakerQuote`](swap::TakerQuote), [`QuoteLimit`](swap::QuoteLimit), [`QuoteConstraints`](swap::QuoteConstraints) | a swap's outcome and its stopping rule | the limit that bound the trade is named, not inferred |
| [`MakerMarketSnapshot`](maker_equity::MakerMarketSnapshot), [`MakerState`](maker_equity::MakerState), [`AccrualInputs`](maker_equity::AccrualInputs) | the settle's inputs | market-wide state and per-position rows from one block |
| [`AccruedMakerSnapshot`](maker_equity::AccruedMakerSnapshot) | a market snapshot advanced to its block | the accrual replay has run; a what-if mark applies only after it |
| [`MakerEquityBreakdown`](maker_equity::MakerEquityBreakdown) | a settle previewed | exact atoms in the contract's units; f64 only in accessors |
| [`BlockContext`] | one header | number, hash, timestamp; what every snapshot carries |

Two conventions carry the exactness claim in the names. A function or
field suffixed `_x96`, `_x128`, `_atoms` or `_e6` is a wire unit and is
exact; its unsuffixed twin is f64 and is not. And every port names the
contract function it transcribes in its doc, with the commit it was
transcribed from, so that a contract change is a search rather than a
hunt.

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
