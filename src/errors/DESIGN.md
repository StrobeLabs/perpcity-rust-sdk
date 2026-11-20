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
is, but for the two transient classes below, not an error to retry; it
is the contract answering a question. A
liquidation probe's `NotLiquidatable` is "healthy"; `NonMakerPosition` is
"wrong kind". `is_revert::<E>` is how a caller reads the answer, and the
decode module is how a raw revert becomes one.

**A typed revert has a class.** Most reverts are answers, but five
`v1.0.0` reverts are about the moment or the market, not the call:
`StaleBeaconIndex` and `DeadlineExpired` pass on a later try (after the
next beacon update; with the call built again with a new deadline), so
they are transient; a beacon that gives no usable index
(`BeaconIndexUnreadable`, `PriceOutsideRange`) fails every call that
reads it until the beacon is fixed (from `setBeacon` or `createPerp` the
fault is the named beacon's, not the market's), and a market near its
TVL cap (`TvlCapExceeded`) refuses an add that does not fit until the
cap or the TVL moves, while a smaller add can fit. The last two are not
the position's fault and not transient: a caller holds the market or the
add, and does not drop the position.
`RevertClass` is that verdict, decided once from the selector.

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
cause), an endpoint serving another chain (not).

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
| [`ValidationError`](validation.rs#L11) | the caller's input refused, typed per input kind; never transient, no transaction was sent. A variant names only what the caller could not have known (listed below the table) | every constructor, conversion and parse that checks its input, [`TickRange::new`](../math/DESIGN.md) and [`TransportConfigBuilder::build`](../transport/DESIGN.md) among them; every port in `math` and function in `convert`, since pure code fails only on its inputs | [`PerpCityError`](mod.rs#L46), by conversion. The strategy layer refuses a config before any request is made. |
| [`ContractError`](contract.rs#L11) | Each contract failure states its transience. A missing or malformed v1 receipt event retains the mined hash and is permanent. Short reverts retain the full zero-to-three-byte payload and are permanent. [`BlockUnavailable`](contract.rs#L87) and [`StateUnavailable`](contract.rs#L97) distinguish a missing block from unavailable state. | Pinned reads, batches, chain and build checks, history scans and receipt parsers. | [`PerpCityError`](mod.rs#L46), by conversion. A retry loop keys on the transience the variant states; a forensic read keys on which of the pair it was. |
| [`TransactionError`](transaction.rs#L17) | The variant identifies the send stage. `tx_hash()` is available after broadcast. `is_unconfirmed` marks trades that may still land. `MakerNotClosed` is a maker close that mined as a partial removal: transient, with its hash, since a new close reads the liquidity again. | [`TxPipeline::prepare`](../hft/DESIGN.md) and the send or replacement paths | [`PerpCityError`](mod.rs#L46), by conversion. The strategy layer checks the stage and hash before it retries a trade. |
| [`NotReplaceableReason`](transaction.rs#L278) | why a stuck transaction is not signed again; `original_can_land` says whether it can be this client's, pending or mined unread, which makes the refusal unconfirmed: every reason but `WrongSender`, since a call the method does not replace (a taker trade given to `replace_v1_maker`, a maker trade given to `replace_v1_taker`, a liquidation, a call with value), one to another market, or one signed for another chain (`OtherChain`) still lands | the replacement paths of `PerpClient::replace_v1_taker` and `PerpClient::replace_v1_maker`, inside a `TransactionError::NotReplaceable` | [`NotReplaceableReason::original_can_land`](transaction.rs#L306), which `TransactionError::is_unconfirmed` asks. The strategy layer reconciles the original when it can land. |
| [`RevertClass`](decode.rs#L201) | what a typed revert means for a retry: `Answer` for every selector but five `v1.0.0` errors; `Transient` (`StaleBeaconIndex`, `DeadlineExpired`) is the only class it calls transient; `BeaconFault` and `Hold` are not the position's fault | [`revert_class`](decode.rs#L260), from the selector alone, so the class of a selector cannot depend on the path that saw the revert (a path that keeps no selector gives none: a debt); [`TransactionError::revert_class`](transaction.rs#L453) for a simulation revert, and [`PerpCityError::revert_class`](mod.rs#L163) through it | [`RevertClass::is_transient`](decode.rs#L243), which `PerpCityError::is_transient` asks for a simulation revert. The strategy layer holds a market on a `BeaconFault`, an add on a `Hold`, and quarantines only on an `Answer`. |
| [`PerpCityError`](mod.rs#L46) | the umbrella: one enum for every failure; [`PerpCityError::is_transient`](mod.rs#L139) answers for all; [`PerpCityError::tx_hash`](mod.rs#L194) for any post-broadcast send; `is_unconfirmed` for one that may still land, which a retry of transients must not resend; `hash_to_replace` for the hash a replacement takes next | conversion from [`ValidationError`](validation.rs#L11), [`ContractError`](contract.rs#L11), [`TransactionError`](transaction.rs#L17), the transport's and the ABI's errors, the JSON parser's, Circle's attestation service's ([`IrisError`](../cctp/DESIGN.md)), and the filesystem's for a recording read or written; nothing constructs it directly | [`Result`](mod.rs#L207); [`MakerEquityKind`](../client/DESIGN.md), as the `Failed` payload of an outcome. The strategy layer's retry loops and per-position decisions key on `is_transient` (and, for a simulation revert, on `revert_class`) and never re-classify. |
| [`Result`](mod.rs#L207) | the crate's result: `PerpCityError` throughout; `Result<T, ValidationError>` only inside `math` and `convert`, where nothing else can fail | every read, scan and send | the strategy layer, which adds context at its binaries and nowhere else. |

What a `ValidationError` variant carries: `NoBandCapacity`, the band's
ticks; `ReduceOnlyViolated`, the position's perp amount a reduce-only
change was checked against; `InvalidAmount`, the text an amount's parse
refused; `BlockMismatch`, the two blocks inputs meant for one came from;
`LongCloseCapacityExceeded` and `ShortCloseCapacityExceeded`, the open
interest the contract checks on that side (with the residual a full
removal converts) and whether the removal takes the market's last maker
liquidity; `ExcessMakerRemoval`, the band's liquidity.

The enums are `#[non_exhaustive]`. A new variant is added when a caller
would branch on it, and existing callers with a wildcard keep compiling.

`NotReplaceable` refuses before signing. `ReplacementUnconfirmed` carries the original
hash, its nonce, any replacement hash, whether the node rejected it, and the
cause; its transience,
[`is_revert`](transaction.rs#L461) and [`revert_class`](decode.rs#L260)
are the cause's. A market of another build is refused as one, before the
look-up, so the hash the caller gave is kept and reported unresolved.
`ReplacementNonceUsed` says the nonce advanced and no receipt was found
for either hash after several looks: evidence of the nonce, not of the
winner, so neither hash can land and the effect is unknown. It keeps
`refused`, so `tx_hash` names the original after a refused replacement.
`hash_to_replace` names the hash a caller replaces next: the replacement
the node did not reject, else the original.

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
  pruned block as a missing position, all key on `is_transient`, and,
  for a simulation revert, on `revert_class`, so a beacon fault holds a
  market instead of dropping its positions. The
  layer above adds context at its binaries and never re-classifies.

## Terminology

- **Transient**: a retry can help. A `DeadlineExpired` is transient only
  for a retry that builds the call again with a new deadline; its
  calldata gets the same answer forever. **Not transient**: the same request
  gets the same answer; the fix is elsewhere.
- **Typed revert**: a contract error decoded to its selector; an answer,
  not a failure, unless its class says otherwise.
- **Revert class**: what a typed revert means for a retry: an answer, a
  transient, a beacon fault, or a hold.
- **Beacon fault**: a beacon gives no usable index: the market's own
  (every call that reads the index fails until it is fixed), or, from
  `setBeacon` or `createPerp`, the beacon the call names.
- **Hold**: a refusal of this amount by the market's state (a TVL cap);
  a smaller amount can fit, and this one waits until that state moves.
- **Stage**: where in a send a failure happened; the transaction
  variants are the stages.
- **Refused**: the caller's input rejected before any request.
- **Classify**: decide which variant a node's answer is, at the read that
  saw it.

## Debts

- **`is_transient` is by wrapping path for bare contract calls** (SDK
  #115). A transport failure inside a now-read surfaces as an ABI error
  the classification does not recognise. The pinned reads fixed their
  half; of the head reads, only the era read of the `v1.0.0` writes
  (`PerpClient::open_taker_v1`, `adjust_taker_v1`, `close_taker_v1` and
  `replace_v1_taker`) has: no answer (a bare `-32603`, a gateway with no
  backend) is `Rpc` and transient, and an answer (a revert, by message or
  by revert data under any code, a refused method) stays an ABI error,
  not transient.
- **`MulticallFailed` carries a string.** The reasons a batch fails are
  few and known; they should be variants a caller can branch on.
- **A revert that keeps no selector has no class.** A mined revert
  (`TransactionError::Reverted`) carries a reason string, and a revert on
  a read path reaches the caller as `PerpCityError::Abi`; both answer
  `revert_class` with `None`. A `v1.0.0` read that accrues reads the
  beacon, so it can revert with a beacon fault the caller cannot see as
  one. The class of a selector does not depend on the path; whether a
  path keeps the selector does.
- **`Rpc` and `Abi` are the raw layers showing through.** Every failure
  that reaches a caller should be one of the three families; the raw
  variants exist because some paths do not yet classify.
