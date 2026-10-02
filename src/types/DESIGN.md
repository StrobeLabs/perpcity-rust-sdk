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

**Types say units.** A bare `f64` field here is the human view; anything
exact carries a type from [`units`](../units/DESIGN.md), which is also
what the exact door takes. A reader never has to open a doc to know
whether a number is dollars or atoms, and a caller cannot pass one for the
other.

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

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`OpenTakerParams`](../types.rs#L245) | what a caller wants to do, in human units: none beyond field types; scaled once at the call | the caller | [`PerpClient::open_taker`](../client/DESIGN.md), which scales it once into the exact form. |
| [`ExactOpenTakerParams`](../types.rs#L257) | the same in atoms: the single submission path; no float round-trip | the caller, sizing to the atom; the human-unit open builds one by scaling, inside `PerpClient::open_taker` rather than as a conversion, which is a debt | [`PerpClient::open_taker_exact`](../client/DESIGN.md). A market maker that must not round-trip through `f64` builds this one. |
| [`AdjustTakerParams`](../types.rs#L292) | an adjustment in human units; a close is an adjustment by the whole size | the caller; `PerpClient::close_taker` builds one inside | [`PerpClient::adjust_taker`](../client/DESIGN.md). |
| [`ExactAdjustTakerParams`](../types.rs#L306) | the same in atoms; the single submission path | the caller; the human-unit adjustment builds one by scaling inside `PerpClient::adjust_taker` | [`PerpClient::adjust_taker_exact`](../client/DESIGN.md). |
| [`OpenMakerParams`](../types.rs#L272) | a band to open: margin in USDC, two ticks, liquidity | the caller | [`PerpClient::open_maker`](../client/DESIGN.md). The two loose ticks here are the debt a `TickRange` exists to remove. |
| [`AdjustMakerParams`](../types.rs#L319) | a maker adjustment; a close is the whole liquidity | the caller; `PerpClient::close_maker` builds one inside | [`PerpClient::adjust_maker`](../client/DESIGN.md). |
| [`OpenResult`](../types.rs#L341) | what an open did: the hash, the id, the realised deltas from the receipt's event, not the request | [`PerpClient::open_taker_exact`](../client/DESIGN.md) and [`PerpClient::open_maker`](../client/DESIGN.md) | nothing in the crate. The strategy layer records what actually filled. |
| [`AdjustTakerResult`](../types.rs#L359) | what a taker adjustment did, from the receipt | [`PerpClient::adjust_taker_exact`](../client/DESIGN.md), and the human-unit adjustment and close over it | nothing in the crate. The strategy layer. |
| [`AdjustMakerResult`](../types.rs#L372) | what a maker adjustment did, from the receipt | [`PerpClient::adjust_maker`](../client/DESIGN.md), and the close over it | nothing in the crate. The strategy layer. |
| [`MarketConfig`](../types.rs#L43), [`Bounds`](../types.rs#L69), [`Fees`](../types.rs#L151) | the market's configuration: fractions and human units, from e6 once, and the EMA window the contract smooths with; the fees and bounds convert to and from the slow cache's entries | [`MarketReader::get_config`](../client/DESIGN.md); [`MarketReader::get_snapshot`](../client/DESIGN.md), alongside the snapshot | nothing in the crate. The strategy layer reads it once per market, and advances the snapshot's EMAs by its window. |
| [`MarginRatios`](../types.rs#L140), [`MarginRatioTriple`](../types.rs#L86) | the two kinds' margin ratios as fractions: init, liquidation, backstop | [`StateAt::margin_ratios`](../client/DESIGN.md); [`MarketReader::get_margin_ratios`](../client/DESIGN.md) as the convenience | nothing in the crate. The strategy layer's sizing and health checks. |
| [`MarketSnapshot`](../types.rs#L212) | the market's live state at the lagged snapshot block: pool price, index, the contract's mark, the stored EMAs, funding, open interest; carries its [`BlockContext`](../math/DESIGN.md), so further reads can pin to it | [`MarketReader::get_snapshot`](../client/DESIGN.md), one multicall and the index, the mark exact then converted once | nothing in the crate. The strategy layer seeds a live cache from it and then follows the feed; the mark and the stored `Emas` are what let that cache keep marking at the contract's price between touches rather than at the pool's. |
| [`OpenInterest`](../types.rs#L184) | the two sides' draw in perp tokens | [`MarketReader::get_open_interest`](../client/DESIGN.md) | [`MarketSnapshot`](../types.rs#L212), as a field; the strategy layer's capacity gauges. |
| [`SolvencyState`](../types.rs#L196) | the market's solvency in USDC, at a block | [`StateAt::solvency`](../client/DESIGN.md) | nothing in the crate. The strategy layer's solvency audits. |
| [`ChainDeployments`](../types.rs#L32) | the addresses a chain shares: collateral and pool manager | the known chains' constants, or the caller for another | [`ChainReader::new`](../client/DESIGN.md). |
| [`Side`](../types.rs#L165) | long or short: the taker direction, nothing else | the caller | [`Capacity::on`](../math/DESIGN.md), [`MarketCapacity::headroom`](../math/DESIGN.md), [`MarketCapacity::open_interest`](../math/DESIGN.md) and [`MarketCapacity::utilization_e6`](../math/DESIGN.md), every side-keyed read on capacity; [`liquidity_for_capacity`](../math/DESIGN.md), which sizes for one side. |

`convert` has no row because its functions are edges, not nodes. Every
read that returns a price calls [`price_x96_to_f64`](../convert.rs#L191)
once; the trades call [`scale_to_6dec`](../convert.rs#L51)
once; the balance and solvency reads call
[`usdc_from_atoms`](../convert.rs#L91); the decoder and the
maker-equity batch call [`unpack_balance_delta`](../convert.rs#L297).
Each validates and refuses what the chain would, and none is called from
the middle of a computation.

Every type derives `Serialize` and `Deserialize`, because the surface is
what gets logged, dashboarded and persisted, and inert data can be
without risk.

## Edges

- From the [root](../../DESIGN.md): the unit boundary; the inert-data
  rule.
- To [`client`](../client/DESIGN.md): parameters go in, results and
  snapshots come out; the client is where `convert` is called.
- To [`math`](../math/DESIGN.md): the exact twins of everything here.
  `MarketSnapshot::pool_price` is the f64 of what `PoolSnapshot` holds
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
- **Wire unit**: a count of atoms or a fixed-point encoding; a type from
  [`units`](../units/DESIGN.md), never a bare integer.
- **Scale**: the 6-decimal conversion; exact. **Precision bound**: the
  known loss of an X96 to `f64` conversion.
- **Exact door**: the `Exact*` parameter types.
- **Inert**: no invariant, no arithmetic; safe to construct, serialise
  and log.

## Debts

- **`MarketConfig::pool_price` sits among configuration.** A market's
  configuration should not carry a live price; it is there because an
  older read returned both at once.
- **`convert` still describes itself against the Zig SDK.** Its rules are
  its own now.
- **`OpenInterest` is in perp tokens while capacity is in atoms.** The two
  are compared constantly and should share a unit at the surface, with the
  exact pair in `math`.
