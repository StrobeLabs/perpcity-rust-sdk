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

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`Perp`], [`PerpDeployedEvents`] | the market's binding: matches the deployed commit; every selector, struct and event locked; the deployed era's event shapes decodable alongside | the ABI, through `sol!` | [`StateAt`](crate::client::StateAt), [`MarketReader`](crate::client::MarketReader) and [`PerpClient`](crate::client::PerpClient), for every call and every batch; [`decode_log`](crate::events::decode_log), for every event of both eras; the gas floor in `hft`, by selector. Nothing above the client calls it. |
| [`IBeacon`] | the index module's binding: `index` and `IndexUpdated` | the ABI | the index reads on [`ChainReader`](crate::client::ChainReader) and [`StateAt`](crate::client::StateAt); [`History::beacon_prints`](crate::history::History::beacon_prints), for the print series; [`decode_log`](crate::events::decode_log). |
| [`IFees`], [`IMarginRatios`], [`IPriceImpact`] | the rule modules' views the SDK reads, no more | the ABI | the configuration reads on [`MarketReader`](crate::client::MarketReader) and [`StateAt`](crate::client::StateAt): fees and bounds into the slow cache, margin ratios, the impact bounds the pool read needs. |
| [`IFunding`], [`IPricing`] | the two rule modules the SDK never calls: their computations are ported in `math` | the ABI | nothing. They are bound so a live probe can confirm the deployed selectors, and so a port has the interface it transcribes beside it. |
| [`IERC20`] | the collateral token, and the position NFT's `Transfer` | the ABI | the balance reads on [`ChainReader`](crate::client::ChainReader) and [`StateAt`](crate::client::StateAt); approvals and transfers on [`PerpClient`](crate::client::PerpClient); [`History::token_transfers`](crate::history::History::token_transfers). |
| [`IPoolManagerState`] | `extsload`, the pool's storage read raw | the ABI | the tick map in [`StateAt::pool`](crate::client::StateAt::pool) and the fee-growth reads in the maker-equity batch on [`MarketReader`](crate::client::MarketReader), at slots `storage` derives. |
| [`IMulticall3`] | the batching primitive, `aggregate3` | the ABI | [`ChainReader::get_balances_batch`](crate::client::ChainReader::get_balances_batch) and the maker-equity batch on [`MarketReader`](crate::client::MarketReader); the pinned reads batch through alloy's builder instead. |
| [`PerpFactory`] | the factory's binding, for its error selectors | the ABI | [`try_extract_revert`](crate::errors::decode::try_extract_revert), which names a factory revert; the dormant `PerpCreated` shape is SDK #110. |
| [`Capacity`] | `calcCapacity`'s pair, raw perp atoms | `capacity` and `makerDetails` | [`Capacity`](crate::math::capacity::Capacity) in `math`, by `From`; the accrual inputs and the `CapacityUpdated` event narrow it field by field. |
| [`OpenInterest`] | the two sides' draw, raw | `openInterest` | [`OpenInterest`](crate::types::OpenInterest) at the surface; [`MarketCapacity`](crate::math::capacity::MarketCapacity) as its draw leg. |
| [`PricePair`] | the `(ammPrice, index)` word, `uint128` each | `emas` and `RatesAndEmasRefreshed` | [`PricePair`](crate::math::pricing::PricePair), the same pair typed for the EMA arithmetic. |
| [`Rates`] | funding and utilization rates with the last touch | `rates` | [`Mark`](crate::math::pricing::Mark), for the last touch it advances from; [`AccrualInputs`](crate::math::maker_equity::AccrualInputs); the funding reads at the surface. |
| [`Cumulatives`] | the accounting trackers | `cumulatives` and `CumulativesAccrued` | [`MakerMarketSnapshot`](crate::math::maker_equity::MakerMarketSnapshot); [`CumulativesInfo`](crate::events::CumulativesInfo), verbatim. |
| [`SolvencyState`] | the market's solvency words, raw | `solvencyState` | [`SolvencyState`](crate::types::SolvencyState) at the surface, in USDC. |
| [`Position`], [`Maker`], [`MakerFunding`], [`TickInfo`] | a position's storage: the shared row, the maker's band and capacity, its funding trackers, the per-tick funding words | `positions`, `makerDetails`, and the slots `storage` derives | [`MakerBand`](crate::math::range::MakerBand), the band narrowed to a validated value; [`MakerState`](crate::math::maker_equity::MakerState) and [`TickFunding`](crate::math::maker_equity::TickFunding), the settle's per-position inputs. `Position` is also what the position reads on `StateAt` return raw; see the debts. |
| [`SwapResult`] | a swap's outcome with its four fee legs | the taker events | [`SwapInfo`](crate::events::SwapInfo), in human units. |
| [`Modules`], [`PoolKey`] | the five rule modules' addresses; the pool's key and tick spacing | `modules` and `poolKey` | the reads, which resolve a module's address before calling it, and the immutables cache. Never a caller's. |
| [`Taker`], [`FeeFund`] | bound, unread | the ABI | nothing. They are locked so a struct change on the deployed contract fails a test here. |
| `abi_lock` (tests) | selectors, struct shapes and event topics as deployed, with evidence | the on-chain evidence beside each | CI. |
| `storage::*_slot` (crate-private) | slot derivation: the Solidity mapping rule over transcribed base slots; locked by the reads' golden outcomes | the deployed layouts | the tick map in the pool read and the fee-growth and tick-funding reads in the maker-equity batch. |

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
