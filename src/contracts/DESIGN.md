# `contracts` and `storage`: the deployed shapes

Up: the [root](../../DESIGN.md). Consumed by [`client`](../client/DESIGN.md)
for every call, [`events`](../events/DESIGN.md) for every log, and
[`math`](../math/DESIGN.md) for the structs its snapshots are narrowed
from.

## Purpose

These two modules are the crate's knowledge of what is actually on the
chain: the ABI of the deployed contracts, generated as bindings and
locked by tests, and the storage layout of the values the contracts hold
but do not expose through a view. Everything else in the crate is built
on the assumption that these are right, so the design here is about how
that assumption is kept true.

`contracts` is public and is the only place a `sol!` block exists.
`storage` is crate-private: a slot is a fact about a layout, and no
caller above the client should know one exists.

## What matters

**The chain, not the repository.** The contracts repository's main
branch is ahead of what is deployed, and it will stay ahead until a
cutover. A binding that follows main calls a selector the deployed
bytecode does not have and gets an empty revert. So bindings match the
deployed commit, named in the module doc, and a change to a binding is
verified against a live market first: a selector probe whose typed
revert proves the function exists. The ABI lock tests then hold the shape:
selectors from input types, struct fields by name and type, event
signatures by topic, with the on-chain evidence in a comment.

**Events are the one era exception.** A market's early logs were emitted
by an earlier version, and they are on chain forever. So the event
bindings include every shape a live market has emitted,
`PerpDeployedEvents` where the deployed shape differs from main, and the
decoder recognises both. Calls do not get this exception.

```text
   contract history ─────────────────────────────────────────────────────►

        earlier bytecode          DEPLOYED (4bbe554f)            repository main
        ┌───────────────┐         ┌───────────────────┐          ┌──────────────────┐
        │ MakerClosed / │         │ what is live on   │          │ post-release     │
        │ MakerConverted│         │ Arbitrum today    │          │ work; not on     │
        │ with liqFee,  │         │                   │          │ chain             │
        │ isLiquidation │         │                   │          │                   │
        └───────┬───────┘         └─────────┬─────────┘          └────────┬─────────┘
                │                           │                             │
   calls  ──────┼───────────────────────────┤ the ONLY target             │ never called
                │                           │ (selector probe + abi_lock) │
                │                           │                             │
   events ──────┴───────────────────────────┴─────────────────────────────┤
          every shape a live market ever emitted stays decodable          │ shapes the
          (PerpDeployedEvents alongside Perp)                             │ next era will
                                                                          │ emit; not yet
   a market's tape:  ▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓
                     ▲ deploy block                        ▲ now
                     logs from every era it lived through, forever
```

Calls and events have different relationships to time, and the diagram
is the reason the era rule has one exception. A call is made now, so it
targets exactly one bytecode: the deployed one, verified by a selector
probe and locked by the ABI tests, and never the repository's head,
whose selectors the chain does not have. An event was emitted at some
block in the past and is on chain forever, so a scan of a market's tape
from its deploy block meets every shape that market ever emitted,
including ones from bytecode that has since been upgraded. The decoder
therefore knows every era's event shapes, while the calls know only the
present one. The bar at the bottom is why: a tape does not get shorter
when the contract changes.

**A slot is a layout fact, locked by an outcome.** A storage slot is
derived by the Solidity mapping rule from a base slot and an offset, and
the base slots and offsets are transcribed from the deployed layout. They
are not locked by a unit test on the arithmetic but by the reads that use
them reproducing a real outcome: the pool snapshot's tick map reconciling
with the pool's active liquidity, the maker settle reproducing a real
liquidation. A slot that is wrong fails those, loudly.

**Units are declared once.** Prices at `2^96`, margin and fees in
6-decimal USDC, ratio and fee parameters at `1e6`, rates at `1e18` per
day, the packed `BalanceDelta` with perp then USD. The bindings carry
the raw integers; the crate's unit rules, in the root and in `types`,
say what each is.

## The mental model

The `Perp` interface is the market: its structs are what it stores, its
functions are what it answers and accepts, its events are what it says.
The five module interfaces are the rules it delegates to, each with the
one or two views the SDK needs. The factory, the beacon, the ERC-20 and
the PoolManager's `extsload` are the market's surroundings. Multicall3 is
the batching primitive.

The storage module is the market seen from below: the mapping slots for
per-tick funding on the Perp, and the pool's state, tick bitmap, tick
info, fee growth and position slots on the PoolManager, so that what the
contracts compute but do not expose can be read anyway. Two layouts, two
sets of base slots, one mapping rule.

## The type system

| Item | What it is | The invariant it carries |
|---|---|---|
| [`Perp`] | the market's binding | matches the deployed commit; every selector, struct and event locked |
| [`PerpDeployedEvents`] | the deployed era's event shapes | decodable alongside `Perp`'s; the exception that keeps old logs readable |
| [`IFees`], [`IFunding`], [`IMarginRatios`], [`IPriceImpact`], [`IPricing`], [`IBeacon`] | the modules | the views the SDK reads, no more |
| [`PerpFactory`], [`IERC20`], [`IPoolManagerState`], [`IMulticall3`] | the surroundings | the calls the SDK makes, no more |
| [`Modules`], [`Position`], [`Maker`], [`Taker`], [`SwapResult`], [`Rates`], [`PricePair`], [`Capacity`], [`OpenInterest`], [`Cumulatives`], [`SolvencyState`], [`FeeFund`], [`TickInfo`], [`PoolKey`], [`MakerFunding`] | the contract's structs | raw wire units; narrowed into `math` and `types` at the edge |
| `abi_lock` | the tests | selectors, struct shapes and event topics as deployed, with evidence |
| `storage::*_slot` | slot derivation | the Solidity mapping rule over transcribed base slots; locked by the reads' golden outcomes |

The module doc names the deployed commit and lists what main has that
the chain does not. That list is the cutover's checklist.

## Edges

- From the [root](../../DESIGN.md): the chain is the deployed one; eras.
- To [`client`](../client/DESIGN.md): every call and every batch is a
  binding from here; every raw storage read is a slot from `storage`.
- To [`events`](../events/DESIGN.md): the decoder is the only consumer
  of the event bindings, both eras.
- To [`math`](../math/DESIGN.md): each port names the Solidity it
  transcribes; the snapshot types are narrowed from these structs.
- Out to the contracts repository: a change there is a change here only
  when it is deployed. The design proposal for the next era's events and
  views is the SDK's ask of that repository, and the cutover is the
  wholesale replacement of this module's deployed commit.

## Terminology

- **Binding**: a Rust type or function generated from an ABI.
- **Deployed**: the commit whose bytecode is live; the only thing calls
  target.
- **Era**: a contract version; **deployed era**, **next era**.
- **ABI lock**: the tests that pin selectors, struct shapes and event
  topics with on-chain evidence.
- **Selector probe**: a call whose typed revert proves the selector
  exists on the deployed bytecode.
- **Slot**: a storage address derived from a layout. **Base slot,
  offset**: the layout's transcribed constants. **Mapping rule**:
  `keccak256(abi.encode(key, base))`.

## Debts

- **The `Perp` interface follows main for events and the chain for
  calls.** That is the era rule working, but it means the interface is
  not one era's shape, and a reader has to know which events are live.
- **The `Maker` struct carries `capacity` on the deployed era and not on
  main.** The cutover's checklist starts here.
- **Slot constants are locked only by outcomes.** That is the right lock,
  but a layout change would fail a golden test rather than a test that
  names the slot; the failure would need reading to be understood.
- **The redeployed factory's `PerpCreated` shape exists only in a deleted
  commit** (SDK #110), recoverable when a second live factory makes it
  matter.
