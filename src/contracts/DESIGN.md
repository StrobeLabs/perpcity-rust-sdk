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
deployed commits, named in the module doc, and a change to a binding is
verified against a live market first: a selector probe whose typed
revert proves the function exists. The ABI lock tests then hold the shape:
selectors from input types, struct fields by name and type, event
signatures by topic, with the on-chain evidence in a comment.

**Two builds are live, and a market is one of them for life.** Build
`58b42b7` markets and `v0.2.2-upgradeable` markets share every view and
trade selector, so one `Perp` binding reads and trades both. Where they
differ, the difference is a fact about the market, read once from its
pool key (only a `v0.2.2` pool carries the guard hook) and kept with the
other immutables as its `Era`: a liquidation is the 2-arg whole-position
call on one and the 3-arg call by amount on the other, and the stored
EMAs are a view on one and a storage slot on both, so the slot is what
the SDK reads. `PerpV022` holds what only the newer build has. Nothing
above the client chooses by era; the client does, once per call shape.

**Events are the era exception.** A market's early logs were emitted by
an earlier version, and they are on chain forever. So the event bindings
include every shape a live market has emitted, `PerpDeployedEvents` for
the tailed maker closes of `58b42b7` and `PerpV022::TakerClosed` for the
untailed taker close of `v0.2.2`, and the decoder recognises each.

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
the raw integers; the crate's unit rules, in the root and in `convert`,
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

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`Perp`](../contracts.rs#L41), [`PerpDeployedEvents`](../contracts.rs#L41) | the market's binding: the views, trades and structs both live builds share, the `58b42b7` whole-position liquidations and its tailed `TakerClosed`; every selector, struct and event locked; `PerpDeployedEvents` holds that build's tailed maker closes | the ABI, through `sol!` | [`StateAt`](../client/DESIGN.md), [`MarketReader`](../client/DESIGN.md) and [`PerpClient`](../client/DESIGN.md), for every call and every batch; [`decode_log`](../events/DESIGN.md), for every event of both eras; the gas floor in `hft`, by selector. Nothing above the client calls it. |
| [`PerpV022`](../contracts.rs#L41) | what only a `v0.2.2-upgradeable` market has: the 3-arg liquidations by amount, the untailed `TakerClosed`, `SurplusRecovered`, `HOOKS`, and the errors of `Errors.sol`'s additions, the guard hook and the ERC-1967 proxy; every selector locked against `cast keccak` | the ABI, through `sol!` | the liquidation calldata on [`MarketReader`](../client/DESIGN.md), when the market's `Era` is the newer build; [`decode_log`](../events/DESIGN.md), for the untailed close; the gas floor in `hft`, by selector; [`try_extract_revert`](../errors/DESIGN.md), which names a `v0.2.2` revert by its selector. |
| [`IBeacon`](../contracts.rs#L41) | the index module's binding: `index` and `IndexUpdated` | the ABI | the index reads on [`ChainReader`](../client/DESIGN.md) and [`StateAt`](../client/DESIGN.md); [`beacon_prints`](../history/DESIGN.md), for the print series; [`decode_log`](../events/DESIGN.md). |
| [`IFees`](../contracts.rs#L41) | the fee module's views: the fee split and the liquidation fee | the ABI | the fee reads on [`MarketReader`](../client/DESIGN.md), into the slow cache. |
| [`IMarginRatios`](../contracts.rs#L41) | the margin module's views: the maker's and the taker's triple | the ABI | the margin-ratio reads on [`StateAt`](../client/DESIGN.md) and the bounds read on [`MarketReader`](../client/DESIGN.md). |
| [`IPriceImpact`](../contracts.rs#L41) | the impact module's view: the sqrt-price bounds a swap may reach | the ABI | the pool read on [`StateAt`](../client/DESIGN.md), which needs the bounds to quote. |
| [`IFunding`](../contracts.rs#L41), [`IPricing`](../contracts.rs#L41) | the two rule modules the SDK never calls: their computations are ported in `math` | the ABI | nothing. They are bound so a live probe can confirm the deployed selectors, and so a port has the interface it transcribes beside it. |
| [`IERC20`](../contracts.rs#L41) | the collateral token, and the position NFT's `Transfer` | the ABI | the balance reads on [`ChainReader`](../client/DESIGN.md) and [`StateAt`](../client/DESIGN.md); approvals and transfers on [`PerpClient`](../client/DESIGN.md); [`token_transfers`](../history/DESIGN.md). |
| [`IPoolManagerState`](../contracts.rs#L41) | `extsload`, the pool's storage read raw | the ABI | the tick map in [`StateAt::pool`](../client/DESIGN.md) and the fee-growth reads in the maker-equity batch on [`MarketReader`](../client/DESIGN.md), at slots `storage` derives. |
| [`IMulticall3`](../contracts.rs#L41) | the batching primitive, `aggregate3` | the ABI | [`ChainReader::get_balances_batch`](../client/DESIGN.md) and the maker-equity batch on [`MarketReader`](../client/DESIGN.md); the pinned reads batch through alloy's builder instead. |
| [`PerpFactory`](../contracts.rs#L41) | the factory's binding, for its error selectors | the ABI | [`try_extract_revert`](../errors/DESIGN.md), which names a factory revert; the dormant `PerpCreated` shape is SDK #110. |
| [`Capacity`](../contracts.rs#L41) | `calcCapacity`'s pair, raw perp atoms | `capacity` and `makerDetails` | [`Capacity`](../math/DESIGN.md) in `math`, by `From`; the accrual inputs and the `CapacityUpdated` event narrow it field by field. |
| [`OpenInterest`](../contracts.rs#L41) | the two sides' draw, raw | `openInterest` | the surface's `OpenInterest` and the draw leg of `MarketCapacity`, each filled field by field inside a read; see the debts. |
| [`PricePair`](../contracts.rs#L41) | the `(ammPrice, index)` word, `uint128` each | `RatesAndEmasRefreshed`, and `emas` on `58b42b7` (bound, no longer called) | the decoder, field by field into `math`'s `PricePair`; see the debts. |
| [`Rates`](../contracts.rs#L41) | funding and utilization rates with the last touch | `rates` | the pinned reads and the maker-equity batch, which take the last touch and the rates out of it field by field; see the debts. |
| [`Cumulatives`](../contracts.rs#L41) | the accounting trackers | `cumulatives` and `CumulativesAccrued` | the maker-equity batch and the decoder, field by field; see the debts. |
| [`SolvencyState`](../contracts.rs#L41) | the market's solvency words, raw | `solvencyState` | the solvency read, which scales it into the surface's `SolvencyState`; see the debts. |
| [`Position`](../contracts.rs#L41), [`Maker`](../contracts.rs#L41), [`MakerFunding`](../contracts.rs#L41), [`TickInfo`](../contracts.rs#L41) | a position's storage: the shared row, the maker's band and capacity, its funding trackers, the per-tick funding words | `positions`, `makerDetails`, and the slots `storage` derives | the band and position reads, which narrow them field by field into a `MakerBand`, a `MakerState` and its `TickFunding`; see the debts. `Position` is also what the position reads on `StateAt` return raw. |
| [`SwapResult`](../contracts.rs#L41) | a swap's outcome with its four fee legs | the taker events | [`SwapInfo`](../events/DESIGN.md), in human units. |
| [`Modules`](../contracts.rs#L41), [`PoolKey`](../contracts.rs#L41) | the five rule modules' addresses; the pool's key and tick spacing | `modules` and `poolKey` | the reads, which resolve a module's address before calling it, and the immutables cache. Never a caller's. |
| [`Taker`](../contracts.rs#L41), [`FeeFund`](../contracts.rs#L41) | bound, unread | the ABI | nothing. They are locked so a struct change on the deployed contract fails a test here. |
| `abi_lock` (tests) | selectors, struct shapes and event topics as deployed, with evidence | the on-chain evidence beside each | CI. |
| `storage::*_slot` (crate-private) | slot derivation: the Solidity mapping rule over transcribed base slots; locked by the reads' golden outcomes | the deployed layouts, the same on both builds for every slot read here | the tick map in the pool read and the fee-growth and tick-funding reads in the maker-equity batch. |
| `storage::stored_emas` (crate-private) | the stored EMA pair from its one storage word, `ammPrice` low and `index` high; locked by a golden word from a live market beside its `emas()` answer | `eth_getStorageAt` on slot 11, in [`ChainReader`](../client/DESIGN.md) | the mark on [`StateAt`](../client/DESIGN.md) and the snapshot on [`MarketReader`](../client/DESIGN.md), in place of the `emas()` view `v0.2.2` lacks. |

The module doc names both deployed commits and lists what main has that
neither chain build does. That list is the cutover's checklist.

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
- **Deployed**: a commit whose bytecode is live; the only thing calls
  target. Two are: **build `58b42b7`** and **`v0.2.2-upgradeable`**.
- **Era**: a contract version. A market's era is read from its pool key
  and never changes; **next era** is the contracts repository's main.
- **ABI lock**: the tests that pin selectors, struct shapes and event
  topics with on-chain evidence.
- **Selector probe**: a call whose typed revert proves the selector
  exists on the deployed bytecode.
- **Slot**: a storage address derived from a layout. **Base slot,
  offset**: the layout's transcribed constants. **Mapping rule**:
  `keccak256(abi.encode(key, base))`.

## Accepted structure

The bindings are the deployed contract's shape, and most of them are
islands in the graph: a binding is a word on the wire, not a value the
crate passes around. These are the ones where that is the whole answer.

- **The four call-parameter structs are the ABI's word order and nothing
  else.** `OpenTakerParams`, `OpenMakerParams`, `AdjustTakerParams` and
  `AdjustMakerParams` are assembled inside the send that uses them, from
  the crate's own twin in [`client`](../client/DESIGN.md), and never travel
  as a value. The twin is the point: a caller states a trade in human
  units, and the ABI's field order is a detail of the call.
- **`Taker`, `FeeFund` and `TickInfo` are bound and unread.** The
  interface is the deployed contract's, not a list of the calls the SDK
  makes, so a binding with no caller is the interface being complete, and
  the ABI lock is its consumer. `TickInfo`'s per-tick funding words are
  read as slots instead, beside the fee-growth slots in the same batch.
- **`Modules` is the one binding kept whole.** Its fields are module
  addresses, so there is nothing to narrow, and the reads hold it in a
  crate-private view; a public type whose only holder is private draws no
  edge.
- **`SwapResult` crosses by a crate-private free function.** `swap_info`
  turns it into [`SwapInfo`](../events/DESIGN.md), once, for the three
  taker events. The conversion is the decoder's and belongs with it, so
  the graph of the public surface shows none of it.
- **The `Perp` interface is the shared surface plus `58b42b7`'s own.**
  Its liquidations, `emas()` and `TakerClosed` are the older build's, and
  `PerpV022` carries the newer build's counterparts rather than the
  interface being split in two: every market answers the shared part, and
  the client picks the rest by `Era` in one place per call shape.

## Debts

- **Narrowing is field by field, and invisible to the type graph.**
  `Capacity` is the only binding that crosses into the crate's types
  through a `From` impl, and two of its three read paths bypass even that;
  `OpenInterest`, `PricePair`, `Rates`, `Cumulatives`, `SolvencyState` and
  `PoolKey` are copied field by field inside the reads that use them. As
  `From` and `TryFrom` impls those edges would be typed, checked and
  drawn, and the unit checks of #124 would have one place to live.
- **The `Maker` struct carries `capacity` on both deployed builds and not
  on main.** The cutover's checklist starts here.
- **`Perp::emas` is bound and never called.** It exists on `58b42b7` and
  is locked there; the slot read replaced it so one path serves both
  builds. Removing the binding would break the public surface for no
  gain, so it stays until the cutover.
- **`Position` is on the surface and its row does not say so.** The
  position reads on [`StateAt`](../client/DESIGN.md) hand it back raw, so
  the strategy layer consumes it; inside the crate only two private
  callers take one. It shares a row with three types that are not the
  surface, so marking it is a row of its own, not a phrase.
- **Slot constants are locked only by outcomes.** That is the right lock,
  but a layout change would fail a golden test rather than a test that
  names the slot; the failure would need reading to be understood.
- **The redeployed factory's `PerpCreated` shape exists only in a deleted
  commit** (SDK #110), recoverable when a second live factory makes it
  matter.
