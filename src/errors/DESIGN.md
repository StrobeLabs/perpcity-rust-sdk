# `errors`: what a failure means

Up: the [root](../../DESIGN.md). Every module produces these; the ones
that decide which variant a node's answer is are [`client`](../client/DESIGN.md),
[`history`](../history/DESIGN.md) and [`transport`](../transport/DESIGN.md).

## Purpose

This module names every way the crate can fail, and for each name says
the one thing a caller must know: whether trying again can help. A
strategy's retry loop, a batch's per-position outcome, a runner's
decision to back off or to page someone, all key on that answer. Getting
it wrong in either direction has a cost: spinning on a pruned-state read
forever, or abandoning a send whose replica was one block behind.

It is a taxonomy, not a log. A variant exists because a caller would
branch on it; a failure no caller would branch on is a message inside a
variant, not a variant.

## What matters

**Transience is a property of the failure, not of the path that wrapped
it.** A node saying "historical state is not available" is not transient
whether it arrived through a multicall or a single call; a node saying
"header not found" is transient the same way. So classification happens
where the answer was seen, in the read that knows what it asked, and the
variant carries the verdict. The part of the crate where this is still
by wrapping path is a named debt.

**A send's failure says where in the send it happened.** Before
simulation, at simulation, at signing, at broadcast, at the receipt: each
stage has different consequences for the nonce and the money. A
simulation revert burned nothing and the nonce is untouched; a mined
revert burned gas; a broadcast failure or a receipt timeout may have
landed, so the hash is carried and the nonce is never reused. The
variant is the stage.

**Typed reverts are answers.** A contract revert decoded to its selector
is not an error to retry; it is the contract answering a question. A
liquidation probe's `NotLiquidatable` is "healthy"; `NonMakerPosition` is
"wrong kind". `is_revert::<E>` is how a caller reads the answer, and the
decode module is how a raw revert becomes one.

**Validation fails before the chain is asked.** A price that is not
positive, a range that is not a range, a margin under the minimum, a
block range backwards: these are the caller's mistakes and they are
refused locally, typed, with no request sent. None is transient.

## The mental model

There are three families and one umbrella.

`ValidationError` is the caller's input refused: prices, margins,
leverage, ranges, ratios, overflow, a decode that failed, a config that
cannot be. Never transient.

`ContractError` is the chain's state refusing or missing: a position
that does not exist or is not the caller's, a module not registered, an
event not in the receipt, a batch that failed, a block the replica lacks
(transient), state the node pruned (not), a log range refused (not), a
storage read that failed (transient when a transport error is the
cause).

`TransactionError` is the send's stages: simulation reverted or failed,
signing failed, gas unavailable, too many in flight, broadcast failed,
receipt timed out, mined but reverted, mined but out of gas, nonce
desynced. Each carries what the stage knows, and every post-broadcast
variant carries the hash.

`PerpCityError` composes the three with the raw transport, ABI and
serde errors underneath, so any module returns its own family with `?`
and a caller matches one enum. `is_transient` is the one question it
answers for all of them.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`ValidationError`](validation.rs#L11) | the caller's input refused: typed per input kind — `InvalidShare` for a fraction past the whole or weights that do not make one — never transient; no request was sent. A variant names only what the caller could not have known — `NoBandCapacity` carries the band's ticks and not the side, because the caller passed the side in and naming it back would make this module depend on the vocabulary of one above it | every constructor and conversion that checks its input, [`TickRange::new`](../math/DESIGN.md) and [`TransportConfigBuilder::build`](../transport/DESIGN.md) among them; every port in `math` and every function in `convert` returns it alone, since pure code can fail only on its inputs | [`PerpCityError`](mod.rs#L45), by conversion. The strategy layer refuses a config or a parameter before any request is made, which is why this family is separate from the chain's answers. |
| [`ContractError`](contract.rs#L9) | the chain's state refusing: each variant states its transience in its doc; [`BlockUnavailable`](contract.rs#L60) and [`StateUnavailable`](contract.rs#L70) are the pair every pinned read distinguishes | no signature names it: every read returns the crate's `Result`, and the variant is chosen inside the pinned reads on `StateAt` (pruned state, missing block), the batches on `MarketReader` and `ChainReader` (a multicall or storage read that failed), the scans in `history` (a range no narrowing can serve) and the receipt parsers in the trades (an event not emitted) | [`PerpCityError`](mod.rs#L45), by conversion. A retry loop keys on the transience the variant states; a forensic read keys on which of the pair it was. |
| [`TransactionError`](transaction.rs#L11) | the send's stages: the variant is the stage; every variant after broadcast carries `tx_hash`; [`is_revert`](transaction.rs#L206) reads a typed revert | [`TxPipeline::prepare`](../hft/DESIGN.md), the one signature that names it; the stages of `TxBuilder::send` choose the rest inside the crate's `Result`, and the simulation stage names a revert through `try_extract_revert`, the bridge from the node's hex to a selector | [`PerpCityError`](mod.rs#L45), by conversion. The strategy layer reads the stage to know whether money may have moved, and the hash to find out. |
| [`PerpCityError`](mod.rs#L45) | the umbrella: one enum for every failure; [`is_transient`](mod.rs#L111) answers for all; [`PerpCityError::tx_hash`](mod.rs#L136) for any post-broadcast send | conversion from [`ValidationError`](validation.rs#L11), [`ContractError`](contract.rs#L9), [`TransactionError`](transaction.rs#L11), the transport's and the ABI's errors, the JSON parser's, and the filesystem's for a recording read or written; nothing constructs it directly | [`Result`](mod.rs#L145); [`MakerEquityKind`](../client/DESIGN.md), as the `Failed` payload of an outcome. The strategy layer's retry loops and per-position decisions key on `is_transient` and never re-classify. |
| [`Result`](mod.rs#L145) | the crate's result: `PerpCityError` throughout; `Result<T, ValidationError>` only inside `math` and `convert`, where nothing else can fail | every read, scan and send | the strategy layer, which adds context at its binaries and nowhere else. |

The enums are `#[non_exhaustive]`. A new variant is added when a caller
would branch on it, and existing callers with a wildcard keep compiling.

## Edges

- From the [root](../../DESIGN.md): failure is classified, not described;
  every order matters.
- From [`client`](../client/DESIGN.md): the pinned reads classify pruned
  state and missing blocks; the send path produces every transaction
  variant; the probes read typed reverts.
- From [`history`](../history/DESIGN.md): a refusal a narrower range
  cannot fix is `LogsRejected`; everything else is left transient for the
  caller's policy.
- From [`transport`](../transport/DESIGN.md): a transport error that
  reaches the caller has already exhausted the transport's retries and is
  transient at this level.
- From [`math`](../math/DESIGN.md): only `ValidationError`, because pure
  math can fail only on its inputs.
- Out to the strategy layer: retry loops, the maker-equity batch's
  per-position retry decision, a forensic tool's refusal to treat a
  pruned block as a missing position, all key on `is_transient`. The
  layer above adds context at its binaries and never re-classifies.

## Terminology

- **Transient**: a retry can help. **Not transient**: the same request
  gets the same answer; the fix is elsewhere.
- **Typed revert**: a contract error decoded to its selector; an answer,
  not a failure.
- **Stage**: where in a send a failure happened; the transaction
  variants are the stages.
- **Refused**: the caller's input rejected before any request.
- **Classify**: decide which variant a node's answer is, at the read that
  saw it.

## Debts

- **`is_transient` is by wrapping path for bare contract calls** (SDK
  #115). A transport failure inside a now-read surfaces as an ABI error
  the classification does not recognise. The pinned reads fixed their
  half; the head reads have not.
- **`MulticallFailed` carries a string.** The reasons a batch fails are
  few and known; they should be variants a caller can branch on.
- **`Rpc` and `Abi` are the raw layers showing through.** Every failure
  that reaches a caller should be one of the three families; the raw
  variants exist because some paths do not yet classify.
