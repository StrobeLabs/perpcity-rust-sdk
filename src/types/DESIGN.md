# `types` and `convert`: the human surface

Up: the [root](../../DESIGN.md). [`client`](../client/DESIGN.md) returns
these and takes them; [`math`](../math/DESIGN.md) is where the exact
twins live.

## Purpose

These two modules are the edge of the crate where the chain's integers
become numbers a person reasons in, and back. `types` is the inert data a
caller sees and constructs: parameters for trades, results of trades,
the market's configuration and its live state, in USDC, perp tokens,
prices and fractions. `convert` is the one place a human unit becomes a
wire unit or a wire unit a human one.

They are deliberately dumb. A type here carries no invariant beyond its
field types and no arithmetic. Anything with an invariant or a
computation lives with it in `math`; the moment a type here needs a
constructor that checks something, it moves.

## What matters

**The boundary is one line thick.** A trade's margin enters as `f64`
USDC and is scaled to atoms exactly once, in `convert`, at the moment the
client builds the call. A price leaves the chain as X96 and becomes `f64`
once, at the moment the client builds the result. Nothing in between
converts, so the precision loss is known, bounded and in one place:
`Q96_PRECISION` is the bound on a price, and the 6-decimal scaling is
exact.

**A caller that must not round-trip through `f64` has an exact door.**
The `Exact*` parameter types carry atoms directly and are the single
submission path; the human types scale into them. A market maker sizing
to the atom uses the exact door and never sees a float.

**Inert means inert.** `types` is the one module a strategy can construct
freely, serialise, log and persist, because nothing in it can be
invalid in a way the chain would care about before the call is built.
That is why the validated values, `TickRange` and its kin, are not here:
they would make the module's promise false.

**Names say units.** An unsuffixed `f64` field is a human unit; a
suffixed field is a wire unit. A reader should never have to open a doc
to know whether a number is dollars or atoms.

## The mental model

There are four kinds of data at the surface. Parameters a caller builds
to act: open, adjust, close, in human and exact forms. Results a call
returns: the hash, the position id, the realised deltas. The market's
configuration, read once and cached: addresses, bounds, fees, margin
ratios, tick spacing. And the market's live state, read now: the pool
price, the index, funding, open interest, solvency, each a snapshot
carrying its block.

`convert` is the set of pure conversions between the two unit systems:
6-decimal scaling in both directions, X96 prices to and from `f64`, sqrt
prices to and from prices, leverage to and from margin ratios, and the
unpacking of V4's packed delta. Each validates its input and refuses what
the chain would refuse.

## The type system

| Type | What it is | The invariant it carries |
|---|---|---|
| [`OpenTakerParams`], [`AdjustTakerParams`], [`OpenMakerParams`], [`AdjustMakerParams`] | what a caller wants to do, in human units | none beyond field types; scaled once at the call |
| [`ExactOpenTakerParams`], [`ExactAdjustTakerParams`] | the same in atoms | the single submission path; no float round-trip |
| [`OpenResult`], [`AdjustTakerResult`], [`AdjustMakerResult`] | what a call did | the hash, the id, the realised deltas from the event, not the request |
| [`PerpData`], [`Bounds`], [`Fees`], [`MarginRatios`], [`MarginRatioTriple`] | the market's configuration | fractions and human units, from e6 once |
| [`PerpSnapshot`], [`OpenInterest`], [`SolvencyState`] | the market's live state | a snapshot carries its block; every field from that read |
| [`ChainDeployments`] | the addresses a chain shares | collateral and pool manager |
| [`Side`] | long or short | the taker direction, nothing else |
| [`convert`](crate::convert) | the conversions | each validates and refuses what the chain would; one place per conversion |

Every type derives `Serialize` and `Deserialize`, because the surface is
what gets logged, dashboarded and persisted, and inert data can be
without risk.

## Edges

- From the [root](../../DESIGN.md): the unit boundary; the inert-data
  rule.
- To [`client`](../client/DESIGN.md): parameters go in, results and
  snapshots come out; the client is where `convert` is called.
- To [`math`](../math/DESIGN.md): the exact twins of everything here.
  `PerpSnapshot::pool_price` is the f64 of what `PoolSnapshot` holds
  exactly; `MarginRatioTriple` is the fraction of the e6 the contract
  holds; a `TickRange` is what two loose ticks here would want to be, and
  is why they are not here.
- Out to the strategy layer: a strategy's snapshots and views are built
  from these, and a downstream field named `mark_price` that held the
  pool price is the naming mistake this module's `pool_price` refuses to
  make.

## Terminology

- **Human unit**: USDC, perp tokens, a price, a fraction, a leverage;
  `f64`, unsuffixed.
- **Wire unit**: atoms, X96, X128, WAD, e6; integers, suffixed.
- **Scale**: the 6-decimal conversion; exact. **Precision bound**: the
  known loss of an X96 to `f64` conversion.
- **Exact door**: the `Exact*` parameter types.
- **Inert**: no invariant, no arithmetic; safe to construct, serialise
  and log.

## Debts

- **`PerpData::pool_price` sits among configuration.** A market's
  configuration should not carry a live price; it is there because an
  older read returned both at once.
- **`get_perp_data` returns a tuple.** Three loose values where a type
  should be; it predates the surface's rules.
- **`convert` still describes itself against the Zig SDK.** Its rules are
  its own now.
- **`OpenInterest` is in perp tokens while capacity is in atoms.** The two
  are compared constantly and should share a unit at the surface, with the
  exact pair in `math`.
