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
nothing is in flight, then resyncs from the chain, the only authority.
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
                       sync_nonce (once, from the chain's count)
                                       │
                                       ▼
                              ┌─────────────────┐
              ┌──────────────►│     SYNCED      │◄──────────────────────┐
              │               │ next = n        │                       │
              │               └────────┬────────┘                       │
              │        prepare: acquire n, next = n+1 (no lock, no RPC)  │
              │                        ▼                                │
              │               ┌─────────────────┐   fail (provably      │
              │               │   IN FLIGHT n   │   local: signing)      │
              │               │ tracked by hash │──────────────────────►│ nonce released
              │               └───┬─────┬───┬───┘                       │
              │   resolve         │     │   │  older than the timeout   │
              │ (mined: ok,       │     │   └──────────► STUCK n ───────┤ prepare_bump:
              │  reverted,        │     │                                │ same n, higher fees,
              │  out of gas)      │     │ broadcast failed /             │ back to IN FLIGHT
              └───────────────────┘     │ receipt timed out              │
                                        ▼                                │
                              ┌─────────────────┐                       │
                              │    DESYNCED     │  no new send starts    │
                              │ n's fate unknown│                       │
                              └────────┬────────┘                       │
                                       │ in-flight count reaches 0      │
                                       ▼                                │
                              resync: take the chain's count ───────────┘
                              (counts n iff n is live)
```

The loop on the left is the steady state and costs nothing: acquire is
an atomic increment, and a resolved transaction just stops being
tracked. The two exits on the right are the cases where the manager
learns something it cannot verify locally. A signing failure is provably
local, nothing left the process, so the nonce goes back and the next
prepare reuses it. A broadcast failure or a receipt timeout is not: the
transaction may be in a mempool or mined, and either reusing or
rewinding `n` is a guess that is wrong half the time and, when wrong,
spins forever. So the sequence is marked `DESYNCED`, sends fail fast
until everything in flight has resolved, and then one read of the chain's
transaction count re-establishes the truth, counting `n` exactly when it
landed. A stuck transaction is the one case that stays `IN FLIGHT`: the
bump replaces it at the same nonce, so ordering is never disturbed.

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

| Type | What it is | The invariant it carries |
|---|---|---|
| [`NonceManager`](nonce::NonceManager), [`PendingTx`](nonce::PendingTx) | the account's nonce sequence | acquire is lock-free; every nonce handed out is tracked until resolved or failed |
| [`TxPipeline`](pipeline::TxPipeline), [`PipelineConfig`](pipeline::PipelineConfig), [`PreparedTx`](pipeline::PreparedTx), [`InFlightTx`](pipeline::InFlightTx), [`BumpParams`](pipeline::BumpParams) | preparing a send | `prepare` makes no RPC; a desynced sequence refuses sends until it can resync |
| [`FeeCache`](gas::FeeCache), [`GasFees`](gas::GasFees), [`Urgency`](gas::Urgency) | the gas price | fees are computed from a cached base fee within its TTL, scaled by urgency; stale is an error, never a guess |
| [`GasLimits`](gas::GasLimits), [`GasLimitCache`](gas::GasLimitCache) | the gas limit | fixed caps for known calls; cached estimates per selector, evicted when disproved |
| [`StateCache`](state_cache::StateCache), [`StateCacheConfig`](state_cache::StateCacheConfig), [`CachedValue`](state_cache::CachedValue), [`CachedFees`](state_cache::CachedFees), [`CachedBounds`](state_cache::CachedBounds), [`BalanceKey`](state_cache::BalanceKey) | the now-reads' memory | two TTLs; every entry carries its expiry; a layer invalidates whole |
| [`LatencyTracker`](latency::LatencyTracker), [`LatencyStats`](latency::LatencyStats) | the measurement | fixed ring; percentiles over the window |
| [`PositionManager`](position_manager::PositionManager), [`ManagedPosition`](position_manager::ManagedPosition), [`TriggerType`](position_manager::TriggerType), [`TriggerAction`](position_manager::TriggerAction) | trigger evaluation | one trigger per position per check, in a fixed precedence; the price is the caller's |

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
