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
| [`NonceManager`](nonce::NonceManager), [`PendingTx`](nonce::PendingTx) | the account's nonce sequence: acquire is lock-free; every nonce handed out is tracked until resolved or failed | [`NonceManager::new`](nonce::NonceManager::new) from the chain's transaction count, once at sync | [`TxPipeline`](pipeline::TxPipeline), which owns one and is its only caller. It is a separate type so that ownership of the sequence has one home and the pipeline's state machine sits above it. |
| [`PipelineConfig`](pipeline::PipelineConfig) | the pipeline's limits: in-flight cap, stuck timeout | the caller; `Default` is the trading defaults | [`TxPipeline::new`](pipeline::TxPipeline::new). |
| [`TxPipeline`](pipeline::TxPipeline) | preparing a send: `prepare` makes no RPC; a desynced sequence refuses sends until it can resync; every in-flight transaction is tracked by hash | [`TxPipeline::new`](pipeline::TxPipeline::new) from a starting nonce and a [`PipelineConfig`](pipeline::PipelineConfig) | [`PerpClient`](crate::client::PerpClient), which holds one per signer; [`TxBuilder::send`](crate::client::TxBuilder::send), the only path that calls `prepare`, `record_submission`, `resolve` and the desync handling. One caller is the point: the execution model is enforced in one place. |
| [`PreparedTx`](pipeline::PreparedTx) | nonce, fees and gas limit resolved locally; nothing sent | [`TxPipeline::prepare`](pipeline::TxPipeline::prepare) | [`TxBuilder::send`](crate::client::TxBuilder::send), which signs exactly what it says; [`TxPipeline::record_submission`](pipeline::TxPipeline::record_submission), once the broadcast has a hash. It carries the nonce so that a signing failure can hand it back. |
| [`BumpParams`](pipeline::BumpParams) | a replacement at the same nonce with higher fees | [`TxPipeline::prepare_bump`](pipeline::TxPipeline::prepare_bump), for a transaction stuck past the timeout | nothing in the crate yet; the client does not send bumps. See the debts. |
| [`FeeCache`](gas::FeeCache) | the gas price: fees are computed from a cached base fee within its TTL; stale is an error, never a guess | [`FeeCache::new`](gas::FeeCache::new); filled by [`ChainReader::set_base_fee`](crate::client::ChainReader::set_base_fee) and [`ChainReader::refresh_gas`](crate::client::ChainReader::refresh_gas) | [`ChainReader`](crate::client::ChainReader), which holds one per chain so every client shares the base fee; [`TxPipeline::prepare`](pipeline::TxPipeline::prepare), which reads it at send time. |
| [`GasFees`](gas::GasFees) | a max fee and a priority fee, scaled for an urgency | [`FeeCache::fees_for`](gas::FeeCache::fees_for) | [`PreparedTx`](pipeline::PreparedTx), as its fee fields. |
| [`Urgency`](gas::Urgency) | the fee multiplier a caller chooses, background to liquidation defence | the caller, per action | [`FeeCache::fees_for`](gas::FeeCache::fees_for); [`TxBuilder::with_urgency`](crate::client::TxBuilder::with_urgency); every trade on [`PerpClient`](crate::client::PerpClient) takes one. The strategy layer picks it per action, which is the one knob on price it has. |
| [`GasLimits`](gas::GasLimits) | fixed caps for calls whose cost is known, liquidation among them | constants | the liquidation probe on [`MarketReader`](crate::client::MarketReader) and the liquidation send on [`PerpClient`](crate::client::PerpClient), so the probe and the send use the same cap. |
| [`GasLimitCache`](gas::GasLimitCache) | cached estimates per selector, evicted when disproved by an `OutOfGas` | [`GasLimitCache::new`](gas::GasLimitCache::new) | [`PerpClient`](crate::client::PerpClient), which holds one per signer; the simulation stage of [`TxBuilder::send`](crate::client::TxBuilder::send), which consults it before estimating. |
| [`StateCache`](state_cache::StateCache), [`CachedValue`](state_cache::CachedValue), [`CachedFees`](state_cache::CachedFees), [`CachedBounds`](state_cache::CachedBounds), [`BalanceKey`](state_cache::BalanceKey) | the now-reads' memory: two TTLs; every entry carries its expiry; a layer invalidates whole | [`StateCache::new`](state_cache::StateCache::new) from a [`StateCacheConfig`](state_cache::StateCacheConfig) | [`ChainReader`](crate::client::ChainReader), which holds one and invalidates it on a new block; the now-reads on [`MarketReader`](crate::client::MarketReader), which serve fees, bounds, prices, funding and balances from it. The cached fees and bounds convert to and from the surface types in `types` at the read. |
| [`StateCacheConfig`](state_cache::StateCacheConfig) | the two TTLs | the caller; `Default` is 60 s and 2 s | [`StateCache::new`](state_cache::StateCache::new). |
| [`LatencyTracker`](latency::LatencyTracker), [`LatencyStats`](latency::LatencyStats) | the measurement: fixed ring; percentiles over the window | [`LatencyTracker::new`](latency::LatencyTracker::new) | nothing in the crate. The strategy layer's runners record around their sends, which is how the zero-RPC claim is checked. |
| [`PositionManager`](position_manager::PositionManager), [`ManagedPosition`](position_manager::ManagedPosition), [`TriggerType`](position_manager::TriggerType), [`TriggerAction`](position_manager::TriggerAction) | trigger evaluation: one trigger per position per check, in a fixed precedence; the price is the caller's | [`PositionManager::new`](position_manager::PositionManager::new) | nothing in the crate, and the strategy layer has its own. See the debts. |

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

## Debts

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
