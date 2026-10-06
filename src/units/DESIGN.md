# `units`: what the numbers are

Up: the [root](../../DESIGN.md). Everything else depends on this module and
it depends on nothing above `errors` and `constants`, which is what lets
[`math`](../math/DESIGN.md) and [`client`](../client/DESIGN.md) both
speak these types. It also owns the
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
now two types with exactly one legal crossing in each direction:
`PerpAtoms::value_at` and `UsdcAtoms::perp_at`, `SqrtPrice::squared` and
`SqrtPrice::try_from(Price)`. The asset pair has a third corner, because
three quantities are related by one equation: `UsdcAtoms::per(PerpAtoms)`
is the price a fill's two legs imply, the inverse of both.

**Signed and unsigned are different quantities.** The chain stores margin
unsigned because it cannot go negative; a settle component and a
position's exposure both can. So each asset is two types, and the signed
twin drops the unit word, because *delta* is the contract's own word for
it in `BalanceDelta` and in a position's `delta`.

**Arithmetic is only what the quantity admits, and the asymmetry is the
quantity's.** Adding and subtracting are not the same risk, so they do not
get the same door. Two amounts of one asset *add* with an operator and sum
from an iterator — a sum of balances can leave the width but never the
domain, and the accounting-token supply keeps anything the chain can
produce far inside it, which is the same argument that makes a signed
delta's `+` infallible. Subtracting is different: a negative count is not a
count, so it stays `checked_sub`, with a `saturating_sub` for the figures
whose floor is the answer. The signed twins also negate.

Liquidity is the count that does not get the operator. The pool has no
supply bound to argue from, so `LUnits` adds through `checked_add` and the
caller answers for the overflow — which is the same reason every `LDelta`
operation is checked. Withholding `+` from the asset counts as well was the
mistake the first consumer found: summing fee legs is the commonest thing
done to an amount, the reason given for withholding it was about
subtraction, and the fold had nowhere to go but back to primitives.

Two amounts of different assets do not combine at all.

**The wire format does not change.** Every quantity is `repr(transparent)`
and transparent to serde, so a value that was persisted or logged as a
bare number still reads back as one; `PerSide` serializes as the two
named fields it replaced, and `Side` as its word. A type that changed
the JSON would have made this a migration instead of a rename.

## The mental model

Sixteen numbers in four families, and one rule that names them all —
and three types that are not quantities: the key, the pair, and the one
trait, below.

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
      LUnits    / LDelta                            FundingRate, UtilizationRate, Ratio
                                                    Share
                 │                                           │
      accessor: .atoms(), .units()                  accessor: .x96(), .x128(), .e6()

   the crossings, one per pair that looks alike as a primitive, each with
   its inverse

      PerpAtoms ──× Price──► UsdcAtoms           SqrtPrice ──squared──► Price
      UsdcAtoms ──÷ Price──► PerpAtoms           Price ──root──► SqrtPrice
      UsdcAtoms ──÷ PerpAtoms──► Price
      Funding ──÷ SqrtPrice──► FundingPerSqrtPrice

   and the scalars, on the right of every `*`

      Share (≤ 1, exact partition) · f64 (the literal in hand) · the
      strategy's own — all through the one trait, Factor
```

All sixteen have landed: both asset pairs, the price pair, the four
accumulators, the pool's own unit, the two rates, the stored ratio and the
caller's share. The diagram is the rule a seventeenth would be named by
rather than a plan for anything outstanding.

**The one trait is the one open verb.** Everything a quantity does is an
inherent method, because the method's name is part of the language —
`atoms`, `usdc`, `value_at`, `perp_at` say the unit at the call site, and
a generic `.raw()` or `.human()` would erase the one thing they say. The
exception is scaling. It is the same verb on every quantity, and it is the
one place a strategy adds a type of its own: a `Leverage` or a `Skew` with
its invariant in its own constructor, multiplying the SDK's quantities
directly. So `Factor` is a trait, open, with one method; `Share` and `f64`
implement it, and so does whatever a strategy wraps its own scalar in.
There is deliberately no SDK type for "a validated factor": the bound a
factor needs is the strategy's — a leverage between one and twenty, a zone
under five percent — and a generic one would say nothing about it. A
second SDK trait arrives with the first consumer who needs to be generic
over the counts, and it will sit beside the inherent methods rather than
replace them.

**The side is a key, and a pair is two of anything under it.** `Side` is
not a quantity: it is what every directional figure answers to — capacity,
open interest, a utilization rate, the sign of an exposure — and it lives
here because those figures do. `PerSide<T>` is one `T` per side, with the
contract's own field names, so a struct that carried `long_x` and
`short_x` as two fields nothing related carries one field a side indexes.
Before this a strategy wrote `is_long` and branched, and the SDK's own
structs spelled the pair five different ways. The two are the reason a
strategy can say `capacity.on(side)` and `Side::Long.exposure(size)` and
never name a direction as a boolean or a sign.

**A `Share` is not a `Ratio`, and the difference is provenance.** Both are
`u32` millionths on the same scale, and the types refuse to be confused
because of where each comes from. A `Ratio` is a word the contract stores —
a margin threshold, a fee share — and its domain is the `uint24` the
modules hold it in, which can exceed one. A `Share` is a fraction the
caller chose — of a budget, of a band's depth, of a fleet — and its domain
is `[0, 1]` because that is what a fraction of a whole is. A function that
takes one does not take the other, so a strategy cannot hand its own
quarter to a port expecting the contract's fee, or read a fee back as if
it were a slice of something. The share also carries the one arithmetic a
fraction of an exact amount needs: `*` truncates toward zero as the
chain does, `split` and `split_weighted` return every atom they were given,
and `partition` turns any weights into shares that sum to exactly one, so
the `(x as f64 * frac) as u128` that every strategy wrote for itself, each
rounding its own way, has one home.

**An accumulator is read as a difference, never as a level.** The four of
them are levels the contract only adds to, and what a position owes or has
earned is the growth since its own checkpoint. The rule for taking that
difference is not shared: funding and the earnings are ordered, so a
checkpoint ahead of the market's level is two reads that disagree and must
fail, while Uniswap's fee growth is modular, wraps by design, and is still
correct across the wrap. That is why `since` returns a `Result` on three
of them and a plain value on the fourth. Before this the three rules were
three different helpers a call site reached for, and the modular one was a
comment above a `wrapping_sub`.

Three things about the shape. The constructor names the unit it takes
(`UsdcAtoms::new(atoms)`), so a bare integer cannot enter without saying
what it is. The accessor names the representation it hands back, so
leaving the type is as explicit as entering it. And `f64` is never a unit:
it is the human view, reached by `usdc()`, `perp()` or `to_f64()`, lossy
and documented as such, which is why the exactness claim no longer needs a
spelling convention to carry it.

## The language

The types exist so that a strategy reads as what it means. This is what
one writes, and every operator in it is this module's to provide:

```rust
// A long's stop, half the mark below.
let stop = mark * 0.5;
if pool_price <= stop { close(pos) }

// The landing zone: a band around the index, on the pool's spacing.
let range = TickRange::between(index * (1.0 - zone), index * (1.0 + zone))?;

// A 5x long on 100 USDC, at the mark.
let order = Side::Long.exposure(margin.perp_at(mark)? * leverage);

// Pull half a band, margin with it.
let pull = band.liquidity * half;
let withdraw = maker.margin * half;

// Basis, as a number to reason about.
let basis = mark / index - 1.0;

// The price a fill traded at, from its two legs.
let entry = fill.usd_delta.per(fill.perp_delta)?;

// Room on a side, in dollars.
let room = capacity.on(side).saturating_sub(oi.on(side)).value_at(mark)?;

// What a day of funding costs this position.
let cost = funding.over(Duration::from_secs(86_400), notional);
```

Four rules make it hold together. **Judgements are floats; chain
quantities are types.** `0.5`, `zone`, `leverage` and `half` are the
strategy's own and stay `f64` or a validated `Share`; `mark`, `margin` and
`band.liquidity` are typed; the two meet at an operator. **Scaling is `*`,
and it never fails**: the factor is converted once, the product is taken in
512 bits and divided back, truncation is toward zero, and a factor that is
not a number — or a result past the quantity's own width — is a bug that
panics as an overflowing `+` does. **Crossings are named and
can fail**: `value_at`, `perp_at` and `per` are the only places two units
combine, and they keep `?` because an absurd price really can overflow.
**Dimensionless readings come back as `f64`**: a basis, a utilization, a
fill ratio are what the strategy reasons over, so they come out as what it
reasons in.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`UsdcAtoms`](amount.rs#L25) | USDC the chain holds, as a count of atoms: unsigned, because the contract stores a balance that cannot go below zero. Two of them add with `+` and sum from an iterator — the supply bound makes that infallible — while subtracting stays `checked_sub`, since a negative count is not a count. Multiplies by any [`Factor`](factor.rs#L46) with `*`, splits to the atom through `split` and `split_weighted`, measures itself against another count through `share_of`, buys tokens at a price through `perp_at`, and over a count of tokens gives the price it implies through `per`. Displays as the exact decimal, and arrives from a human `f64` rounded to the nearest atom | `*` by a factor, and `split_weighted` beside it; [`PerpAtoms::value_at`](amount.rs#L234), the one crossing from the other asset; [`margin_for_liquidity`](../math/DESIGN.md), what a depth requires; [`amounts_for_liquidity`](../math/DESIGN.md) and [`band_amounts`](../math/DESIGN.md), as a band's USDC leg; [`TakerQuote::amt1_limit`](../math/DESIGN.md); [`MakerEquityBreakdown::position_value`](../math/DESIGN.md), which is a value and so never negative; [`UtilizationRate::over`](rates.rs#L191), what a notional is charged | [`estimate_liquidity`](../math/DESIGN.md) and [`liquidity_for_target_ratio`](../math/DESIGN.md), which size liquidity from a margin; [`MakerState`](../math/DESIGN.md), as the position's stored margin; [`UsdcAtoms::share_of`](amount.rs#L25), one count measured against another; [`UsdcAtoms::perp_at`](amount.rs#L145), the crossing back; [`UsdcAtoms::per`](amount.rs#L169), the price two legs imply. The strategy layer's treasury and sizing hold one wherever a dollar figure must be exact. |
| [`UsdcDelta`](amount.rs#L45) | the same atoms, signed: the width a settle's components and a swap's deltas need, and any sum of them stays far inside `i128` because the accounting-token supply bounds every one. Which way positive points is the **field's**, never the type's — a settle's `funding_owed` is positive when the position *pays* and is subtracted, a swap's `usd_delta` is positive when the position *receives* — so a reader takes the direction from the field before adding it. Multiplies by any [`Factor`](factor.rs#L46) with `*`, a negative one flipping the side, and splits through `split` and `split_weighted`, the pieces keeping its sign; over an exposure, `per` is the price the two legs imply, by magnitudes. Displays as the exact signed decimal, and arrives from a human `f64` rounded to the nearest atom | `*` by a factor, and `split_weighted` beside it; every component of [`MakerEquityBreakdown`](../math/DESIGN.md) and its derived sums; [`PerpDelta::value_at`](amount.rs#L257), an exposure valued at a price; [`FundingRate::over`](rates.rs#L150), what a notional pays over an interval | [`TakerQuote`](../math/DESIGN.md), as the USDC a swap moved; [`MakerState`](../math/DESIGN.md), as the position's recorded USD delta; [`UsdcDelta::perp_at`](amount.rs#L197), the exposure it buys; [`UsdcDelta::per`](amount.rs#L213), a position's entry price from its cost basis. The strategy layer's pnl folds, which sum these and never a float. |
| [`PerpAtoms`](amount.rs#L35) | the market's own token, as a count of atoms; never interchangeable with [`UsdcAtoms`](amount.rs#L25) however alike the two look as integers. Adds and sums with the operators for the reason USDC does, subtracts through the same checked door, and multiplies by any [`Factor`](factor.rs#L46), splits and measures itself as USDC does. Displays as the exact decimal, and arrives from a human `f64` rounded to the nearest atom | `*` by a share or a factor, and `split_weighted` beside it; the capacity and open-interest reads, each a pair of these keyed by side; [`MarketCapacity::headroom`](../math/DESIGN.md); [`amounts_for_liquidity`](../math/DESIGN.md), as a band's perp leg; [`PerpDelta::magnitude`](amount.rs#L58), an exposure without its sign | [`liquidity_for_capacity`](../math/DESIGN.md), as the capacity target to invert; [`AccrualInputs`](../math/DESIGN.md) and [`MakerState`](../math/DESIGN.md), as the capacity and open-interest legs of the accrual; [`PerpAtoms::share_of`](amount.rs#L35); [`UsdcAtoms::per`](amount.rs#L169), as the token leg a price is implied over. |
| [`PerpDelta`](amount.rs#L58) | a signed exposure: positive long, negative short, as a position's `delta` stores it, and its sign is a [`Side`](side.rs#L20) read by `side`; multiplies by any [`Factor`](factor.rs#L46) and splits as [`UsdcDelta`](amount.rs#L45) does, the pieces keeping the side. Displays as the exact signed decimal, and arrives from a human `f64` rounded to the nearest atom | `*` by a share or a factor, and `split_weighted` beside it; otherwise nothing outside the module makes one: a caller names an exposure and the types carry it | [`PoolSnapshot::quote_perp`](../math/DESIGN.md), as the exposure to quote; [`TakerQuote`](../math/DESIGN.md), as the exposure a swap filled; [`MakerState`](../math/DESIGN.md), as the position's recorded perp delta; [`UsdcDelta::per`](amount.rs#L213), as the exposure a cost basis is priced over. The strategy layer's trade parameters, where the sign is the side and nothing else carries it. |
| [`Price`](price.rs#L26) | USDC per unit of the market's token, Q96; the representation is the type's own and the accessor names it. Scales by any [`Factor`](factor.rs#L46) with `*` — a stop, a landing zone, a buffer — and divides by another price into the plain `f64` a basis is one less than. Both take their product in 512 bits; the scaled price is exact to the Q96 unit, and the ratio is exact to 1e-18 until it becomes an `f64`, which makes it a reading for a judgement, never an input to arithmetic the chain will check | [`Mark::fair_price`](../math/DESIGN.md), the price the contract values at; [`fair_price`](../math/DESIGN.md), the deployed port; [`SqrtPrice::squared`](price.rs#L207), the one crossing from a root; [`Price::at_tick`](../math/tick.rs#L324), a tick's price; [`TickRange::geomean`](../math/DESIGN.md), a band's centre; [`UsdcAtoms::per`](amount.rs#L169) and [`UsdcDelta::per`](amount.rs#L213), the price two legs of a fill imply; `*` by a factor | [`fair_price`](../math/DESIGN.md), as each of its four inputs; [`Mark::advanced`](../math/DESIGN.md), as the pool price and the index; [`PerpAtoms::value_at`](amount.rs#L234) and [`UsdcAtoms::perp_at`](amount.rs#L145), as what values and sizes an amount; [`AccruedMakerSnapshot::with_mark`](../math/DESIGN.md) and [`StateAt::maker_equities_at_mark`](../client/DESIGN.md), as a what-if mark; [`TickRange::between`](../math/DESIGN.md), as the two ends of a band; [`Price::tick`](../math/tick.rs#L314), the exact route to a tick. |
| [`SqrtPrice`](price.rs#L36) | the square root of a price, Q96: what Uniswap stores and what every liquidity formula is linear in. The crossing from a price is exact — the integer root of the Q96 word, the inverse of `squared` — and floored, so a range built from prices aligns to the spacing rather than trusting a tick to the unit. `TryFrom<Price>` is the only door from a price: a second spelling of the same root went in 0.10.0. Hashes, so a root keys a map | [`get_sqrt_ratio_at_tick`](../math/DESIGN.md), the exact path from a tick; [`TickRange::sqrt_bounds`](../math/DESIGN.md), a range's two ends; `TryFrom<Price>`, the root of a price | [`get_tick_at_sqrt_ratio`](../math/DESIGN.md); [`band_capacity`](../math/DESIGN.md), [`band_amounts`](../math/DESIGN.md), [`amounts_for_liquidity`](../math/DESIGN.md), [`liquidity_for_capacity`](../math/DESIGN.md) and [`liquidity_for_target_ratio`](../math/DESIGN.md), every one of which is a formula in root prices; [`PoolSnapshot::quote_to_price`](../math/DESIGN.md), as the target to walk to; [`Funding::per_sqrt_price`](accumulators.rs#L104), as what it divides by. |
| [`Funding`](accumulators.rs#L29) | cumulative funding, USDC per perp token, Q96 signed: a level the contract only adds to, read as the growth since a checkpoint and never as a level | [`Funding::since`](accumulators.rs#L78) and [`Funding::advanced_by`](accumulators.rs#L89), the difference and the accrual replay | [`MakerMarketSnapshot`](../math/DESIGN.md) and [`MakerState`](../math/DESIGN.md), as the market's level and the position's four checkpoints; [`TickFunding`](../math/DESIGN.md), as a tick's opposite-side checkpoint; [`CumulativesInfo`](../events/DESIGN.md) and [`MarketEvent`](../events/DESIGN.md), as the event's own level. |
| [`FundingPerSqrtPrice`](accumulators.rs#L39) | the same funding per unit of sqrt-price exposure, Q96 signed: the form a maker's within-band leg accumulates in, because a band's exposure is linear in the root | [`Funding::per_sqrt_price`](accumulators.rs#L104), the one crossing from the funding it divides; [`FundingPerSqrtPrice::since`](accumulators.rs#L124) and [`FundingPerSqrtPrice::advanced_by`](accumulators.rs#L133) | one field in each of the four places its undivided twin has one: [`MakerMarketSnapshot`](../math/DESIGN.md), [`MakerState`](../math/DESIGN.md), [`TickFunding`](../math/DESIGN.md) and [`CumulativesInfo`](../events/DESIGN.md). They are the same signed word, subtracted from their own checkpoints a line apart, which is what the two types keep straight. |
| [`Earnings`](accumulators.rs#L51) | cumulative utilization earnings, USDC per perp token of capacity, Q96 unsigned: the contract only adds, so a checkpoint ahead of it is inconsistent state | [`Earnings::since`](accumulators.rs#L145) and [`Earnings::advanced_by`](accumulators.rs#L154) | [`MakerMarketSnapshot`](../math/DESIGN.md) and [`MakerState`](../math/DESIGN.md), two each for the two sides; [`CumulativesInfo`](../events/DESIGN.md), which carries the paid side under the same shape. |
| [`FeeGrowth`](accumulators.rs#L60) | Uniswap's fee growth per unit of liquidity, Q128 unsigned and **modular**: the word wraps by design and the difference is still correct across the wrap | nothing public makes one: the maker-equity batch reads the pool's global word and each band tick's outside word, and a crate-private fold turns them into the growth inside a band | [`MakerState`](../math/DESIGN.md), as the band's growth now and at the last checkpoint. |
| [`LUnits`](liquidity.rs#L42) | liquidity in the pool's own units, unsigned, as a `uint128`: neither asset, but the depth a range holds, and what every concentrated-liquidity formula is linear in. Multiplies by any [`Factor`](factor.rs#L46) and splits as the counts do, the one arithmetic on it that cannot overflow; displays as the whole count it is, since the pool's unit has no fraction | `*` by a share or a factor, and `split_weighted` beside it; [`estimate_liquidity`](../math/DESIGN.md), [`liquidity_for_target_ratio`](../math/DESIGN.md) and [`liquidity_for_capacity`](../math/DESIGN.md), the three ways to size a band; [`LDelta::magnitude`](liquidity.rs#L138) | [`MakerBand`](../math/DESIGN.md), as the depth standing in a range; [`amounts_for_liquidity`](../math/DESIGN.md), whose other three arguments are root prices; [`PoolSnapshot`](../math/DESIGN.md) and [`TakerQuote`](../math/DESIGN.md), as the active depth before and after a swap; [`MakerState`](../math/DESIGN.md); [`OpenMakerParams`](../client/DESIGN.md), [`ExactOpenMakerParams`](../client/DESIGN.md) and [`PerpClient::close_maker`](../client/DESIGN.md); [`LUnits::share_of`](liquidity.rs#L42). The strategy layer's ladders and corridors size in these. |
| [`LDelta`](liquidity.rs#L73) | a change in liquidity, signed: what an adjust asks for and what a tick's `liquidityNet` stores. Every operation is checked, because unlike a balance there is no supply bound to argue from; `*` by any [`Factor`](factor.rs#L46) and a split are the exceptions, since both only shrink it. Displays as the signed whole count | `*` by a share or a factor, and `split_weighted` beside it; [`LUnits::negated`](liquidity.rs#L104), the delta that closes a band; [`LDelta::negated`](liquidity.rs#L176), the mirror a band's upper tick carries | [`LUnits::checked_add_signed`](liquidity.rs#L83), the one place a depth moves; [`PoolSnapshot::with_liquidity_delta`](../math/DESIGN.md), a what-if band; [`TickLiquidity`](../math/DESIGN.md); [`AdjustMakerParams`](../client/DESIGN.md) and [`MarketEvent`](../events/DESIGN.md). |
| [`Side`](side.rs#L20) | long or short: the taker direction, and the key every directional quantity answers to. The sign of a [`PerpDelta`](amount.rs#L58) is the same fact as a number, and `exposure` and `side` cross between them; `long_if` names the side a boolean judgement — buy or sell, below the index or above — comes out as | [`PerpDelta::side`](side.rs#L76), which reads the sign; [`Side::long_if`](side.rs#L41), from a judgement; otherwise the caller, per read or per order | [`PerSide::on`](side.rs#L117) and [`PerSide::on_mut`](side.rs#L125), every side-keyed read; [`MarketCapacity::headroom`](../math/DESIGN.md), [`MarketCapacity::utilization_e6`](../math/DESIGN.md) and [`liquidity_for_capacity`](../math/DESIGN.md). The strategy layer names a side wherever it asks about one or places an order on one. |
| [`PerSide`](side.rs#L92) | one `T` per side, under the contract's own field names: what every `long_x`/`short_x` pair of fields became. A side is read with `on`, which borrows the value rather than copying it, so a `PerSide` of anything is readable and a count is one deref away; `map`, `zip` and `total` act on both at once | the reads, through the aliases `Capacity` and `OpenInterest`; [`MakerEquityBreakdown::util_earnings`](../math/DESIGN.md), the preview's two legs; the decoder, for every pair an event carries | a field on [`MarketCapacity`](../math/DESIGN.md), [`AccrualInputs`](../math/DESIGN.md), [`MakerMarketSnapshot`](../math/DESIGN.md), [`MakerState`](../math/DESIGN.md), [`MakerEquityBreakdown`](../math/DESIGN.md), [`CumulativesInfo`](../events/DESIGN.md) and [`MakerSettle`](../events/DESIGN.md), and the market snapshot in `client` through the `OpenInterest` alias. The strategy layer's capacity gauges and inventory, which hold one of these wherever they held two numbers. |
| [`FundingRate`](rates.rs#L70) | the funding rate per day, WAD, signed: positive means longs pay shorts. `int88` on chain, so it always fits an `i128`. `over` is what a notional pays at it across an interval, with the rate's sign | the caller, or a `rates()` read; nothing in the crate derives one | [`AccrualInputs`](../math/DESIGN.md), which turns a day's rate and an elapsed interval into the growth the funding cumulative advances by. The strategy layer's cost estimates, through `over`. |
| [`UtilizationRate`](rates.rs#L92) | a utilization fee rate per day, WAD, unsigned: the contract charges it and never pays it, and stores it as `uint64`. Separate from the funding rate for exactly that reason — one signed type would either lose this claim or force a check the width already makes. `over` is what a notional is charged at it across an interval | the caller, or a `rates()` read | [`AccrualInputs`](../math/DESIGN.md), one per side, feeding the two utilization legs of the replay. |
| [`Share`](share.rs#L38) | a fraction of a whole the caller chose, as millionths in `[0, 1]`: not a contract word, so its domain is the fraction's own and its wire form is the `0.25` a configuration writes; a [`Factor`](factor.rs#L46), so it sits on the right of `*`. `partition` makes shares from any weights that sum to exactly the whole | the caller, from a fraction or from weights; [`UsdcAtoms::share_of`](amount.rs#L25), [`PerpAtoms::share_of`](amount.rs#L35) and [`LUnits::share_of`](liquidity.rs#L42), what part a count is of another | `*` on every count and delta, where the truncation happens once; the `split_weighted` beside each, where the pieces always sum to the amount. The strategy layer's budgets, pulls and cohort weights. |
| [`Factor`](factor.rs#L46) | what a quantity is multiplied by: the one trait this module defines, and it is open. `apply` scales a magnitude exactly, truncating toward zero; `is_negative` says whether it flips a sign, which only a delta may take. Implemented by `f64` (to WAD once, exact from there), [`Share`](share.rs#L38), and whatever scalar a strategy defines for itself | the caller: a literal, a configured value, or a type of its own | the right-hand side of `*` on every count, delta and price; [`Ratio::for_leverage`](rates.rs#L248). The strategy layer's `Leverage`, `Skew` and the rest, which implement it by delegating to the float they wrap. |
| [`Ratio`](rates.rs#L105) | a dimensionless ratio, a margin threshold or a fee share, as the contract holds it: `uint24` millionths, so `50_000` is 5%. The domain is checked at every door, construction and deserialisation alike. Not a [`Share`](share.rs#L38): that is the caller's, this is the contract's | [`leverage_to_margin_ratio`](../client/DESIGN.md), the human door over [`Ratio::for_leverage`](rates.rs#L248); [`MakerEquityBreakdown::liquidation_margin_ratio`](../math/DESIGN.md); otherwise a read builds one from a module's `uint24` | [`MarginRatioTriple`](../client/DESIGN.md) and [`Fees`](../client/DESIGN.md), as every threshold and share; [`MakerState`](../math/DESIGN.md) and [`MakerEquityBreakdown`](../math/DESIGN.md), as the ratio stored on a position that the health test compares against; [`liquidation_price`](../math/DESIGN.md). |

The two right-hand columns are where a unit travels. A link under
*Produced by* is the function that makes one; under *Consumed by*, a
function that takes it. A method on the type itself is neither, which is
why the constructors and accessors appear in neither column — except on
the accumulators, where `since` is the only way to read one and is
therefore where the type's whole claim lives.

## Efficiency

Nothing here costs anything a call site was not already paying. Every
quantity is `repr(transparent)` over its primitive, so a value is the
primitive at runtime and a `Vec` of them has the same layout as a `Vec`
of integers; `PerSide<T>` is two `T`s side by side and `Side` is a byte.
The additions and comparisons are the primitive's own. Every scaling and
every crossing — `*` by a factor, `value_at`, `perp_at`, `per`, a price
ratio, a root — is one 512-bit multiply-divide, which is the cost of not
wrapping: a 256-bit product of a wide word and a wide factor can wrap, and
the module's own `to_f64` already refused to take that chance. Nothing on
an `Ok` path allocates. The splits return a `Vec` and `Display` formats
through a `String`; both are called once per rebalance or per log line,
and neither belongs in a hot fold.

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
  ports: the mul-divs are an encoding's multiplication and every scaling
  and crossing here goes through them, the WAD exponential is a rate's own
  function, and the checked add and subtract helpers are what the
  accumulators' `since` and `advanced_by` are written over. It did not
  shrink as the types landed; it became the one place a 512-bit product is
  taken, which is the better outcome.
- To [`math`](../math/DESIGN.md): every snapshot field, port parameter and
  port return is one of these types, and each port unwraps to primitives
  in its opening lines so the transcription stays readable beside the
  Solidity.
- To [`client`](../client/DESIGN.md): a read fills a typed field from a
  contract word, so the narrowing a chain value needs happens once, at
  the read, and is the only place a broken value can enter.
- To `convert`, which is [`client`](../client/DESIGN.md)'s: the exact door
  skips it; the human door carries `f64` and scales into these through it.
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
- **The crossing**: an operation between two unit types. The asset pair
  has three — `value_at`, `perp_at` and `per`, one per corner of the
  equation that relates them — the price pair has `squared` and its root,
  and funding has `per_sqrt_price`. Nothing else combines two units.
- **The human view**: what a type shows a person — the `f64` from `usdc()`
  or `perp()`, lossy and never arithmetic the chain will check, and the
  exact decimal `Display` writes, `5.8` for `5_800_000` atoms. **The
  human door**: `TryFrom<f64>`, which rounds to the nearest atom so a
  decimal of six places or fewer arrives as the atom count it names — up
  to 2^32 units, past which an `f64` cannot hold six decimals at all.

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
- **Each crossing has its inverse, so the two dangerous pairs flow both
  ways.** `PerpAtoms` and `UsdcAtoms`: `value_at` prices tokens and
  `perp_at` sizes them. `SqrtPrice` and `Price`: `squared` and `try_from`
  take a root to its price and back. An inverse is not a
  second home for a conversion: the two directions are two questions a
  strategy asks — what is this worth, and how much does this buy — and
  a pair with only one of them answered sends the other through `f64`.
- **`Price` divides into an `f64` and `f64` multiplies a `Price`**, which
  the graph reads as a cycle through a primitive. A ratio of prices is a
  number the strategy reasons over and a factor is a number it chose;
  that the same primitive carries both is the language's rule 4, not a
  conversion with two homes.
- **`Price` and `TickRange` flow both ways.** `between` builds a range
  from two prices and `geomean` hands a price back from a range; the
  grid and the prices on it are two views of one geometry, and a strategy
  needs both directions.
- **`Price` and `Mark` also convert both ways.** `Mark` is three prices at
  a block and its `fair_price` is a fourth, so prices go in and a price
  comes out. The cycle is the relation itself. `Emas` is the same shape
  one step earlier: two prices at a touch, `advanced` against the spots,
  `mark` a price out.
- **`LUnits` and `LDelta` convert both ways**, for the reason the asset
  pairs do: the pool stores a depth that cannot be negative and an adjust
  asks for a change that can.
- **A sizing formula and its inverse make `LUnits` flow both ways against
  each asset.** `estimate_liquidity` turns a [`UsdcAtoms`](amount.rs#L25)
  margin into a depth and `amounts_for_liquidity` turns a depth back into
  the two assets it holds; `liquidity_for_capacity` inverts a
  [`PerpAtoms`](amount.rs#L35) target the same way. The graph reads the
  round trip as a cycle, and here that is what an invertible formula looks
  like rather than a conversion with two homes. Neither direction is the
  other's `From`: they are different geometry, and the band's price range
  is the third argument that makes them not inverses of one another at a
  different price.
- **A [`Ratio`](rates.rs#L105) goes into a settle preview and comes back
  out.** [`MakerEquityBreakdown`](../math/DESIGN.md) stores the ratio the
  position was opened under and hands it back, because a caller comparing
  the health ratio to the threshold needs both from the same preview. A
  value held and returned is not a conversion with two homes.
- **A [`Share`](share.rs#L38) and each count flow both ways.** A share
  scales a count or a delta through `*` (the `mul` of `Mul<Share>`, and
  the slice of shares `split_weighted` takes), and a count measures itself
  against another as a share through `share_of`, so the graph reads a
  cycle between `Share` and each of [`UsdcAtoms`](amount.rs#L25),
  [`PerpAtoms`](amount.rs#L35) and [`LUnits`](liquidity.rs#L42), and a
  one-way flow into every delta. It is the relation itself: a fraction is
  what you get by dividing two amounts and what you apply to get one back,
  and neither direction is a conversion with two homes.
- **A [`Side`](side.rs#L20) flows both ways with the two things it keys.**
  `Side::exposure` signs a count into a [`PerpDelta`](amount.rs#L58) and
  `PerpDelta::side` reads the sign back; `PerSide::on` takes a side to
  pick a value and `PerSide::iter` hands each value back with its side. A
  key and the thing it indexes necessarily point at each other, and
  neither direction is a conversion with two homes: one is the sign of a
  number and the other is a lookup.
- **`PricePair` holds two `u128` prices rather than two [`Price`](price.rs#L26).**
  It mirrors the contract's struct, whose cast to `uint128` is the check
  its constructor performs, and a `Price` does not remember that width.

## Debts

- **Four macros where one generic might do.** `count!`, `bounded_count!`,
  `delta!` and `accumulator!` each stamp out a struct and its impl, and
  `split` and `split_weighted` are written twice. The alternative is a
  `Count<Unit>` over marker types: one impl, and a consumer could write
  code generic over the counts. Nominal types were chosen so that
  `atoms()` and `units()` say the unit at every call site, which a generic
  `.raw()` would not, and that remains the right trade for a product API.
  The trigger to revisit is the first consumer generic over the counts;
  the duplication grows by one method body per new verb until then.
- **The error is wide.** `ValidationError` carries `String` payloads, so a
  `Result<UsdcAtoms, _>` is several words where the `Ok` is sixteen bytes.
  Nothing allocates on the `Ok` path, so this is register pressure on a
  hot fold, not heap traffic; boxing the payload is the fix when a fold
  measures it.
- **The human readings have five names and one odd one.** `usdc()`,
  `perp()`, `units()`, `fraction()` and `per_day()` each say their unit,
  which is the rule; `Price::to_f64()` says a Rust type instead, and is
  the only one that is fallible. Both facts are defensible — a price has
  no unit word of its own, and a zero word is a failed read — and it is
  still where a new reader stops.
- **The two utilization payment words have no reader.** The event carries
  the paid side of the earnings accumulator beside the earned side, and
  nothing in the crate uses it; it is typed as [`Earnings`](accumulators.rs#L51)
  because it is the same quantity from the other direction, which the name
  does not say.
- **A computed utilization is not a [`Ratio`](rates.rs#L105).**
  [`MarketCapacity::utilization_e6`](../math/DESIGN.md) still answers in a
  bare `u32`, because it is derived rather than stored: open interest above
  capacity is state the contract refuses but a caller can construct, and
  the result then leaves the `uint24` domain the type enforces. Typing it
  would mean either a new failure mode or a silent clamp, so it keeps the
  `Option` whose `None` already means "the contract would pass a sentinel
  here".
