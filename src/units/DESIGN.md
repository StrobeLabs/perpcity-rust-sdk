# `units`: what the numbers are

Up: the [root](../../DESIGN.md). Everything else depends on this module and
it depends on nothing above `errors` and `constants`, which is what lets
[`math`](../math/DESIGN.md), [`client`](../client/DESIGN.md) and
[`types`](../types/DESIGN.md) all speak these types. It also owns the
fixed-point arithmetic, since that is arithmetic over its own encodings.

## Purpose

This module answers one question for every number in the crate: what is
it? A market is denominated in two assets that are indistinguishable as
integers, and in prices that are indistinguishable from their own square
roots. Before these types the answer lived in a field's spelling, which
nothing checked. Here it is the type, so a function's signature is its
unit list and a wrong argument is a compile error.

It is deliberately not where arithmetic lives. Each type carries only the
operations its quantity admits, and the ported contract math takes these
types at its boundary and unwraps to primitives inside, so a port stays a
line-by-line transcription of the Solidity it is checked against.

## What matters

**A unit of account is not an encoding.** This is the distinction the
module is built on, and it decides every name. A count of something
indivisible *is* the quantity: on chain there is no continuous USDC, one
atom is a millionth of a dollar, and nothing smaller exists. So the unit
is in the name, `UsdcAtoms`, and the dollars a person reads are the
derived view. An encoding is how a continuous quantity is packed into an
integer: a price is a real number and Q96 is one representation of it, so
`Price` hides the representation and the accessor that hands it back names
it, `x96`. A call site then reads as the quantity, and the encoding
appears exactly where a value leaves the type.

**The dangerous pairs are the reason this exists.** Two of them. USDC and
the market's token are both six-decimal `u128` counts, so margin and
capacity were the same type with different field names, and the formula
that crosses between them is the protocol's whole valuation. A price and
its square root are both Q96 `U256`, so passing one where the other
belongs compiled and produced a number wrong by a squaring. Each pair is
now two types with exactly one legal crossing: `PerpAtoms::value_at` and
`SqrtPrice::squared`.

**Signed and unsigned are different quantities.** The chain stores margin
unsigned because it cannot go negative; a settle component and a
position's exposure both can. So each asset is two types, and the signed
twin drops the unit word, because *delta* is the contract's own word for
it in `BalanceDelta` and in a position's `delta`.

**Arithmetic is only what the quantity admits.** The signed twins add,
subtract, negate and sum, which is safe because every component the
protocol settles is bounded by the accounting-token supply, far inside
`i128`. The unsigned counts do not, because their subtraction can go
below zero, so they offer `checked_sub` and a `saturating_sub` for the
figures whose floor is the answer. Two amounts of different assets do not
combine at all.

**The wire format does not change.** Every type is `repr(transparent)`
and transparent to serde, so a value that was persisted or logged as a
bare number still reads back as one. A type that changed the JSON would
have made this a migration instead of a rename.

## The mental model

Fourteen types in four families, and one rule that names them all.

```text
                       is the integer the quantity itself?
                                      │
                 ┌────────────────────┴────────────────────┐
                yes                                        no
      a UNIT OF ACCOUNT                            an ENCODING of a real
      name it after the unit                       name it after the quantity
                 │                                           │
      UsdcAtoms / UsdcDelta                         Price / SqrtPrice
      PerpAtoms / PerpDelta                         Funding, Earnings, FeeGrowth
      LiqUnits  / LiqDelta                          Rate, Ratio
                 │                                           │
      accessor: .atoms(), .units()                  accessor: .x96(), .x128()

   the one crossing between the assets        the one crossing between the prices

      PerpAtoms ──× Price──► UsdcAtoms           SqrtPrice ──squared──► Price
```

This PR carries the left column's first two rows and the price pair. The
accumulators, the pool's own unit and the rates follow, and the diagram is
the plan for them.

Three things about the shape. The constructor names the unit it takes
(`UsdcAtoms::new(atoms)`), so a bare integer cannot enter without saying
what it is. The accessor names the representation it hands back, so
leaving the type is as explicit as entering it. And `f64` is never a unit:
it is the human view, reached by `usdc()`, `perp()` or `to_f64()`, lossy
and documented as such, which is why the exactness claim no longer needs a
spelling convention to carry it.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`UsdcAtoms`](amount.rs#L18) | USDC the chain holds, as a count of atoms: unsigned, because the contract stores a balance that cannot go below zero | [`PerpAtoms::value_at`](amount.rs#L119), the one crossing from the other asset; [`amounts_for_liquidity`](../math/DESIGN.md) and [`band_amounts`](../math/DESIGN.md), as a band's USDC leg; [`TakerQuote::amt1_limit`](../math/DESIGN.md); [`MakerEquityBreakdown::position_value`](../math/DESIGN.md), which is a value and so never negative | [`estimate_liquidity`](../math/DESIGN.md) and [`liquidity_for_target_ratio`](../math/DESIGN.md), which size liquidity from a margin; [`MakerState`](../math/DESIGN.md), as the position's stored margin. The strategy layer's treasury and sizing hold one wherever a dollar figure must be exact. |
| [`UsdcDelta`](amount.rs#L38) | the same atoms, signed: what a settle computes, positive toward the position | every component of [`MakerEquityBreakdown`](../math/DESIGN.md) and its derived sums; [`PerpDelta::value_at`](amount.rs#L142), an exposure valued at a price | [`TakerQuote`](../math/DESIGN.md), as the USDC a swap moved; [`MakerState`](../math/DESIGN.md), as the position's recorded USD delta. The strategy layer's pnl folds, which sum these and never a float. |
| [`PerpAtoms`](amount.rs#L28) | the market's own token, as a count of atoms; never interchangeable with [`UsdcAtoms`](amount.rs#L18) however alike the two look as integers | [`Capacity::on`](../math/DESIGN.md), what a band or a market backs on one side; [`MarketCapacity::headroom`](../math/DESIGN.md) and [`MarketCapacity::open_interest`](../math/DESIGN.md); [`amounts_for_liquidity`](../math/DESIGN.md), as a band's perp leg; [`PerpDelta::magnitude`](amount.rs#L47), an exposure without its sign | [`liquidity_for_capacity`](../math/DESIGN.md), as the capacity target to invert; [`AccrualInputs`](../math/DESIGN.md) and [`MakerState`](../math/DESIGN.md), as the capacity and open-interest legs of the accrual. |
| [`PerpDelta`](amount.rs#L47) | a signed exposure: positive long, negative short, as a position's `delta` stores it | nothing outside the module makes one: a caller names an exposure and the types carry it | [`PoolSnapshot::quote_perp`](../math/DESIGN.md), as the exposure to quote; [`TakerQuote`](../math/DESIGN.md), as the exposure a swap filled; [`MakerState`](../math/DESIGN.md), as the position's recorded perp delta. The strategy layer's trade parameters, where the sign is the side and nothing else carries it. |
| [`Price`](price.rs#L25) | USDC per unit of the market's token, Q96; the representation is the type's own and the accessor names it | [`Mark::fair_price`](../math/DESIGN.md), the price the contract values at; [`fair_price`](../math/DESIGN.md), the deployed port; [`SqrtPrice::squared`](price.rs#L131), the one crossing from a root | [`fair_price`](../math/DESIGN.md), as each of its four inputs; [`Mark::advanced`](../math/DESIGN.md), as the pool price and the index; [`PerpAtoms::value_at`](amount.rs#L119), as what values an amount; [`AccruedMakerSnapshot::with_mark`](../math/DESIGN.md) and [`StateAt::maker_equities_at_mark`](../client/DESIGN.md), as a what-if mark. |
| [`SqrtPrice`](price.rs#L34) | the square root of a price, Q96: what Uniswap stores and what every liquidity formula is linear in | [`get_sqrt_ratio_at_tick`](../math/DESIGN.md), the exact path from a tick; [`TickRange::sqrt_bounds`](../math/DESIGN.md), a range's two ends | [`get_tick_at_sqrt_ratio`](../math/DESIGN.md); [`band_capacity`](../math/DESIGN.md), [`band_amounts`](../math/DESIGN.md), [`amounts_for_liquidity`](../math/DESIGN.md), [`liquidity_for_capacity`](../math/DESIGN.md) and [`liquidity_for_target_ratio`](../math/DESIGN.md), every one of which is a formula in root prices; [`PoolSnapshot::quote_to_price`](../math/DESIGN.md), as the target to walk to. |

The two right-hand columns are where a unit travels. A link under
*Produced by* is the function that makes one; under *Consumed by*, a
function that takes it. A method on the type itself is neither, which is
why the constructors and accessors appear in neither column.

## Efficiency

Nothing here costs anything. Every type is `repr(transparent)` over its
primitive, so a value is the primitive at runtime and a `Vec` of them has
the same layout as a `Vec` of integers. The operations are the primitive's
own, except the two crossings, which are one 512-bit multiply-divide each
and were already being paid at the call sites that now go through them.

The one real cost is the `f64` views, and it is the cost they always had:
a price through `to_f64` is exact only to `Q96_PRECISION`, and an amount
through `usdc` or `perp` is exact only below 2^53 atoms, about nine
billion dollars. Both are documented on the method, and neither is on a
path the chain checks.

## Edges

- From the [root](../../DESIGN.md): the exactness principle, and the
  two-surface split these types replace the spelling convention for.
- Down to `errors` and `constants` only. Nothing else, which is the
  invariant that lets every other module depend on this one.
- Inward to the crate-internal `fixed_point` submodule, which is the
  arithmetic these encodings need and which `math` reaches into for its
  ports: the mul-divs are an encoding's multiplication, the WAD exponential
  is a rate's own function, and the checked add and subtract helpers are
  the pre-type form of operators the accumulator types will carry, so the
  submodule shrinks as those land.
- To [`math`](../math/DESIGN.md): every snapshot field, port parameter and
  port return is one of these types, and each port unwraps to primitives
  in its opening lines so the transcription stays readable beside the
  Solidity.
- To [`client`](../client/DESIGN.md): a read fills a typed field from a
  contract word, so the narrowing a chain value needs happens once, at
  the read, and is the only place a broken value can enter.
- To [`types`](../types/DESIGN.md): the exact door carries these; the
  human door carries `f64` and scales into them.
- Out to the strategy layer: a strategy holds these wherever a figure must
  be exact, and the `f64` views where a dashboard or a log wants a number.

## Terminology

- **Unit of account**: a count of something indivisible, where the integer
  is the quantity. **Encoding**: a packing of a continuous quantity into
  an integer.
- **Atom**: the smallest amount of either asset, a millionth. **Count**:
  an unsigned amount. **Delta**: a signed one.
- **Q96, Q128, WAD, e6**: the encodings, named only by the accessors that
  hand them back.
- **The crossing**: an operation between two unit types, of which there
  are exactly two, one per dangerous pair.
- **The human view**: the `f64` a type converts to for a person; lossy,
  and never arithmetic the chain will check.

## Accepted structure

- **Each asset's two types convert both ways, so the graph shows a cycle.**
  `UsdcAtoms` and `UsdcDelta` are one of them and `PerpAtoms` and
  `PerpDelta` the other. It is inherent to a signed and unsigned pair of
  the same unit: widening a count is one direction and taking a
  magnitude, or narrowing a non-negative delta, is the other, and both are
  needed where the chain stores unsigned and the math computes signed.
  A cycle usually means two types want to be one; here collapsing either
  side would lose the "cannot be negative" claim on a balance, which is
  the reason there are two.
- **`Price` and `Mark` also convert both ways.** `Mark` is three prices at
  a block and its `fair_price` is a fourth, so prices go in and a price
  comes out. The cycle is the relation itself.
- **`PricePair` holds two `u128` prices rather than two [`Price`](price.rs#L25).**
  It mirrors the contract's struct, whose cast to `uint128` is the check
  its constructor performs, and a `Price` does not remember that width.

## Debts

- **The accumulators, the pool's own unit and the rates are still
  primitives.** `Funding`, `FundingPerSqrtPrice`, `Earnings`, `FeeGrowth`,
  `LiqUnits`, `LiqDelta`, `Rate` and `Ratio` are named and planned; until
  they land, the fields behind them are bare `I256`, `U256`, `u128` and
  `u32` beside typed neighbours, which is where a new bare primitive could
  now go unnoticed.
- **The two reported invariants are about to become vacuous.** The graph
  still reports on `f64` behind a wire suffix and on a suffix matching its
  primitive. Both are structural once the last suffixed field is typed,
  and they are deleted then.
- **`convert` still exists as the `f64` doors.** Each function is one line
  over a type here, kept because the event vocabulary decodes through
  them; they go when the decoder speaks the units, leaving only the V4
  balance-delta packing.
