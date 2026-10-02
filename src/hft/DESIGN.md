# `hft`: the execution machinery

Up: the [root](../../DESIGN.md). Sideways: [`client`](../client/DESIGN.md)
is the only caller of the pipeline and the caches; [`errors`](../errors/DESIGN.md)
names what the pipeline can fail with.

## Purpose

This module is everything a send needs that must not cost an RPC at the
moment of sending: the account's next nonce, the gas price for the
urgency asked, the gas limit for a call seen before, and the record of
what is in flight. It is also the cache the now-reads serve from, and a
latency tracker for measuring all of it. Together they make preparing a
transaction a local operation.

It is not the send itself, which is the client's, and it is not a
strategy: the position manager here evaluates triggers a caller set, it
does not decide what to hold.

## What matters

**The next nonce is owned, not discovered.** Asking the node for the
transaction count before every send is a round trip on the hot path and
a race against the node's own view. The nonce manager takes the count
once, at sync, and then owns the sequence: acquire is an atomic increment
with no lock, and the manager tracks every nonce it handed out until the
transaction resolves or fails. That ownership is what makes ordering a
fact rather than a hope.

**Doubt is a state, not a guess.** When a broadcast fails after signing
or a receipt never comes, the nonce's fate is unknowable locally: the
transaction may have landed. The wrong answers are to reuse the nonce
(collides if it landed) or to rewind (spins forever if it landed). The
pipeline instead marks the sequence desynced, refuses new sends until
nothing is in flight or being prepared, then resyncs from the chain, the
only authority.
`NonceDesynced` is transient by design: it clears itself.

**Time is a parameter.** Every cache and tracker takes an explicit
timestamp. Nothing reads a clock, so a TTL, a stuck-transaction check or
a latency window is deterministic under test, and a caller with its own
clock, a simulator, a replay, drives it.

**Zero RPC on the hot path is the specification.** Not an aspiration:
`prepare` makes no network call, and a design that would add one is
rejected on that ground. The base fee comes from a header feed or a
poller into the fee cache; the gas limit for a known selector comes from
the limit cache after one estimate; the nonce comes from the manager.

## The mental model

A send is prepared from three local facts and one signature. The
pipeline holds the nonce manager and the fee cache; `prepare` takes a
nonce, resolves fees for the urgency, and returns a `PreparedTx` the
client signs and broadcasts. `record_submission` starts tracking the
hash; `resolve` or `fail` ends it. A transaction older than the
configured timeout is stuck, and `prepare_bump` makes a replacement at
the same nonce with higher fees.

```text
   sync_nonce ──► SYNCED, next = n
                      │  prepare: n taken, next = n+1          no lock, no RPC
                      ▼
                  IN FLIGHT n, tracked by its hash
                      │
      ┌───────────────┼──────────────────┬──────────────────────────┐
      ▼               ▼                  ▼                          ▼
   signing failed   mined             stuck: past the timeout    broadcast failed,
   provably local   ok / reverted /                              receipt timed out:
                    out of gas                                   n's fate unknown
      │               │                  │                          │
   n released      n resolved         prepare_bump: same n,      DESYNCED: no new send;
   next = n        next unchanged     higher fees, IN FLIGHT     once nothing is in flight,
                                      again                      resync from the chain's
                                                                 count, which has n iff n landed
```

The first two outcomes are the steady state and cost nothing: acquire
is an atomic increment, and a resolved transaction just stops being
tracked. A signing failure is provably local, nothing left the process,
so the nonce goes straight back. The last outcome is the one the design
exists for: after a failed broadcast or a lost receipt the transaction
may be in a mempool or mined, and both reusing and rewinding `n` are
guesses that spin forever when wrong. So the sequence is marked
desynced, sends fail fast until everything in flight has resolved, and
one read of the chain's transaction count re-establishes the truth. A
stuck transaction stays in flight and is replaced at the same nonce, so
ordering is never disturbed.

Urgency is the one knob a caller has on price: a multiplier over the base
fee, from background to liquidation defence. The gas limit is the other
knob, cached per selector after the first estimate, with fixed limits
for calls whose cost is known, liquidation among them, so the send and
the probe use the same cap.

The state cache is the now-reads' memory: two TTLs, slow for what only
governance changes, fast for what changes every block, keyed by market or
by wallet, invalidated as a layer when a new block arrives. A now-read
served from the fast layer is at most one block old.

The latency tracker is a ring of samples with percentiles, so the claims
above can be measured rather than asserted.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`NonceManager`](nonce.rs#L42), [`PendingTx`](nonce.rs#L25) | the account's nonce sequence: acquire is lock-free; every nonce handed out is tracked until resolved or failed | [`NonceManager::new`](nonce.rs#L51) from the chain's transaction count, once at sync | [`TxPipeline`](pipeline.rs#L125), which owns one and is its only caller. It is a separate type so that ownership of the sequence has one home and the pipeline's state machine sits above it. |
| [`PipelineConfig`](pipeline.rs#L104) | the pipeline's limits: in-flight cap, stuck timeout | the caller; `Default` is the trading defaults | [`TxPipeline::new`](pipeline.rs#L146). |
| [`TxPipeline`](pipeline.rs#L125) | preparing a send: `prepare` makes no RPC; a desynced sequence refuses sends until it can resync; every in-flight transaction is tracked by hash | [`TxPipeline::new`](pipeline.rs#L146) from a starting nonce and a [`PipelineConfig`](pipeline.rs#L104) | [`PerpClient`](../client/DESIGN.md), which holds one per signer, and whose `TxBuilder::send` is the only path that calls `prepare`, `record_submission`, `resolve` and the desync handling. One caller is the point: the execution model is enforced in one place. |
| [`PreparedTx`](pipeline.rs#L61) | nonce, fees and gas limit resolved locally; nothing sent | [`TxPipeline::prepare`](pipeline.rs#L162) | [`TxPipeline::record_submission`](pipeline.rs#L208), once the broadcast has a hash; between the two, `TxBuilder::send` signs exactly what it says. It carries the nonce so that a signing failure can hand it back. |
| [`TxRequest`](pipeline.rs#L46) | what a send asks the pipeline for: destination, calldata, value, an optional limit, the urgency | the client's builder, from its parameters | [`TxPipeline::prepare`](pipeline.rs#L162). It exists so the pipeline never sees the client's types. |
| [`InFlightTx`](pipeline.rs#L74) | a submitted transaction the pipeline tracks: its nonce, hash, fees and submission time | `record_submission`, which stores one per broadcast | [`TxPipeline`](pipeline.rs#L125), which holds them until resolved or failed and reads their fees and times for `prepare_bump` and the stuck check. |
| [`BumpParams`](pipeline.rs#L89) | a replacement at the same nonce with higher fees | [`TxPipeline::prepare_bump`](pipeline.rs#L316), for a transaction stuck past the timeout | nothing in the crate yet; the client does not send bumps. See the debts. |
| [`FeeCache`](gas.rs#L230) | the gas price: fees are computed from a cached base fee within its TTL; stale is an error, never a guess | [`FeeCache::new`](gas.rs#L241); filled through the chain reader's `set_base_fee` and `refresh_gas` | [`ChainReader`](../client/DESIGN.md), which holds one per chain so every client shares the base fee; [`TxPipeline::prepare`](pipeline.rs#L162), which reads it at send time. |
| [`GasFees`](gas.rs#L213) | a max fee and a priority fee, scaled for an urgency | [`FeeCache::fees_for`](gas.rs#L302) | [`PreparedTx`](pipeline.rs#L61), as its fee fields. |
| [`Urgency`](gas.rs#L200) | the fee multiplier a caller chooses, background to liquidation defence | the caller, per action | [`FeeCache::fees_for`](gas.rs#L302); [`TxBuilder::with_urgency`](../client/DESIGN.md); every trade on the client takes one, [`PerpClient::open_taker`](../client/DESIGN.md) among them. The strategy layer picks it per action, which is the one knob on price it has. |
| [`GasLimits`](gas.rs#L35) | fixed caps for calls whose cost is known, liquidation among them | constants | the liquidation probe on the market reader and the liquidation send on the client, by name, so the probe and the send use the same cap. Constants leave no signature, which is why this row draws no edge. |
| [`GasLimitCache`](gas.rs#L115) | cached estimates per selector, evicted when disproved by an `OutOfGas` | [`GasLimitCache::new`](gas.rs#L136) | [`PerpClient`](../client/DESIGN.md), which holds one per signer and consults it in the simulation stage of a send before estimating. |
| [`StateCache`](state_cache.rs#L114), [`CachedValue`](state_cache.rs#L39), [`CachedFees`](state_cache.rs#L56), [`CachedBounds`](state_cache.rs#L69), [`BalanceKey`](state_cache.rs#L102) | the now-reads' memory: two TTLs; every entry carries its expiry; a layer invalidates whole | [`StateCache::new`](state_cache.rs#L130) from a [`StateCacheConfig`](state_cache.rs#L82) | [`ChainReader`](../client/DESIGN.md), which holds one and invalidates it on a new block; the now-reads on the market reader reach it through the chain reader and serve fees, bounds, prices, funding and balances from it. The cached fees and bounds convert to and from the surface types in `types` at the read. |
| [`StateCacheConfig`](state_cache.rs#L82) | the two TTLs | the caller; `Default` is 60 s and 2 s | [`StateCache::new`](state_cache.rs#L130). |
| [`LatencyTracker`](latency.rs#L62), [`LatencyStats`](latency.rs#L38) | the measurement: fixed ring; percentiles over the window | [`LatencyTracker::new`](latency.rs#L77) | nothing in the crate. The strategy layer's runners record around their sends, which is how the zero-RPC claim is checked. |
| [`PositionManager`](position_manager.rs#L201), [`ManagedPosition`](position_manager.rs#L69), [`TriggerType`](position_manager.rs#L45), [`TriggerAction`](position_manager.rs#L56) | trigger evaluation: one trigger per position per check, in a fixed precedence; the price is the caller's | [`PositionManager::new`](position_manager.rs#L207) | nothing in the crate, and the strategy layer has its own. See the debts. |

## Efficiency

This module's specification is a number: zero requests on the hot path.

- **`prepare` makes no request.** The nonce is an atomic increment on
  the manager; the fees come from the fee cache within its TTL, scaled by
  urgency; a cached gas limit comes from the limit cache by selector.
  Anything not in a cache is an error at prepare time, never a request.
- **What fills the caches, and how often.** The base fee arrives from a
  header feed or a poller, once per block or per interval, not per send.
  A gas limit is estimated once per selector and reused until an
  `OutOfGas` evicts it. The state cache holds fees and bounds for 60 s and
  prices, funding and balances for 2 s, and a new block invalidates the
  fast layer whole.
- **Synchronisation costs one request**, `eth_getTransactionCount`, at
  startup and after a desync, never per send.
- **Stuck detection and bumping are local**: the pipeline compares
  submission times against its timeout and prepares the replacement
  without a request.
- **Measured.** The latency tracker records samples in a fixed ring and
  reports percentiles; it is how "zero on the hot path" is checked rather
  than believed.

## Edges

- From the [root](../../DESIGN.md): every order matters; latency is a
  design input.
- To [`client`](../client/DESIGN.md): the pipeline is driven by
  `TxBuilder::send` and only by it; the state cache is read and written
  by the now-reads and invalidated by the chain reader. Nothing else
  touches these.
- To [`errors`](../errors/DESIGN.md): the pipeline's states are the
  transaction error variants, `GasUnavailable`, `TooManyInFlight`,
  `NonceDesynced`, each with its stated transience.
- From [`feeds`](../feeds/DESIGN.md): the base fee reaches the fee cache
  from a header feed, or from a poller in the layer above where the feed
  proved too expensive to hold.
- Out to the strategy layer: a runner sets the base fee and syncs the
  nonce at startup, and a wallet used out of band by another process is
  this module's ownership rule seen from outside: a nonce the manager did
  not hand out is a desync it will discover at the next send.

## Terminology

- **Prepare**: nonce and fees resolved locally, nothing sent. **Submit**:
  broadcast and begin tracking. **Resolve, fail**: end tracking, with or
  without releasing the nonce.
- **In flight**: submitted, not yet resolved. **Stuck**: in flight past
  the timeout. **Bump**: a replacement at the same nonce with higher fees.
- **Desynced**: the local sequence no longer provably matches the chain.
  **Resync**: taking the chain's count again, once nothing is in flight.
- **Urgency**: the fee multiplier a caller chooses.
- **Fast layer, slow layer**: the state cache's TTLs. **Invalidate**:
  clear a layer on a new block.
- **Hot path**: the code between deciding to send and broadcasting; makes
  no RPC.

## Accepted structure

- **A cache and its entry convert both ways, which is what a cache is.**
  `StateCache` takes a `CachedFees` or a `CachedBounds` in and hands the
  same type back, so the graph shows two cycles. The conversion is not
  mis-homed; there is no conversion, only storage with an expiry.
- **A registry and its element do the same.** `PositionManager` takes a
  `ManagedPosition` in and lends one out, by reference, because the
  manager advances each position's trailing anchor in place.
- **`TxPipeline` hands out a `PreparedTx` and takes it back.** That round
  trip is the mechanism, not an accident of it: `prepare` acquires the
  nonce and `record_submission` is the only thing that settles it, so the
  loan is what keeps the nonce accounting closed.
- **`TriggerAction` flows back into `PositionManager` through an output
  buffer.** `check_triggers_into` takes `&mut Vec<TriggerAction>` so a hot
  loop allocates nothing, and the graph reads that parameter as an input.
  Nothing consumes a trigger inside the crate; the second direction is the
  buffer, not a conversion.
- **`GasLimits` is a namespace, not a value.** It is a field-less struct
  carrying ten constants, so it is produced by nothing and consumed by
  nothing, and the row says as much.

## Debts

- **The cached fees and bounds duplicate the surface types field for
  field.** `CachedFees` and `CachedBounds` hold the same four `f64` as
  `Fees` and `Bounds` in [`types`](../types/DESIGN.md), with four identity
  `From` impls between them, and the cycle the graph draws over those impls
  is the usual conclusion holding: two types that want to be one. The
  cache should hold the surface type.
- **`BumpParams` is the strategy layer's and the table does not say so.**
  `prepare_bump` computes a replacement's fees and nothing in the crate
  sends one, so the type reads as a dead end rather than as part of the
  surface a caller builds on.
- **`position_manager` is a bot's strategy state, not chain truth.** It
  predates the boundary rule and is the one module here an outside
  market maker would not want from an SDK. It should move to the strategy
  layer or be removed.
- **`state_cache` is shaped for one bot on one market**, keyed by market
  word and wallet, and older than the handle split. The chain reader
  shares it correctly, but its API still speaks of "a perp id".
- **The gas-limit cache's eviction on `OutOfGas`** is the right rule
  arrived at from an incident; the estimate's headroom on Arbitrum is
  still a constant rather than a measured margin.
