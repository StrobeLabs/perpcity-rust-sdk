# `cctp`: how collateral reaches the market's chain

Up: the [root](../../DESIGN.md). Sideways: [`client`](../client/DESIGN.md),
whose send pipeline or any alloy wallet carries the calldata built here.

## Purpose

A market settles in native USDC on Arbitrum, and money often arrives on
another chain. Circle's Cross-Chain Transfer Protocol moves it without a
pool or a wrapped token: USDC is burned on the source chain, Circle signs
the burn message, and the destination chain mints the same USDC to the
recipient. This module gives each of those three steps its shape, so a
maker who funds a wallet from Base, and our own treasury, use one
implementation of the message format and the contract addresses.

## What matters

**Calldata, not sends.** The module builds the burn and the mint and
reads whether a message has minted; it never signs. A burn on the source
chain and a mint on the destination are different chains, nonces and
wallets, and the caller already owns a sender for each.

**The mint is idempotent and unowned.** A burn leaves the destination
caller empty, so anyone may submit its mint, and the destination marks
the message's nonce used on the first. A caller that lost track of a
burn checks `nonce_used` before sending, and a mint it did not send is
not a failure.

**The recipient is fixed at the burn.** The message carries the mint
recipient; nothing at the mint can redirect it.

## Type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`CctpChain`](../cctp.rs#L63) | one chain's domain, native USDC and the two CCTP V2 contracts, from Circle's address table; the fields are private, so the four constants are the only values | the four constants; [`CctpChain::for_chain_id`](../cctp.rs#L134) | [`Burn::calldata`](../cctp.rs#L233), as the source chain. The strategy layer's treasury, which checks the destination USDC is the market's collateral. |
| [`Burn`](../cctp.rs#L203) | one `depositForBurn`: amount, destination, recipient, finality and the fee cap | [`Burn::standard`](../cctp.rs#L219), the free transfer | the strategy layer's treasury, which sends its calldata. |
| [`Finality`](../cctp.rs#L181) | the two thresholds Circle attests at, Fast (1000) and Standard (2000) | the caller | [`Burn`](../cctp.rs#L203), which carries it. |
| [`IrisClient`](../cctp.rs#L374) | one attestation service, mainnet or sandbox; every request bounded by `REQUEST_TIMEOUT` | [`IrisClient::mainnet`](../cctp.rs#L381), [`IrisClient::sandbox`](../cctp.rs#L386) | the strategy layer's treasury, which polls it for each burn. |
| [`Attested`](../cctp.rs#L261) | a burn message at least a burn body long, with Circle's signature | [`Attested::new`](../cctp.rs#L273); inside a complete `AttestationStatus` | the strategy layer's treasury, which sends its `receive_calldata` after `nonce_used`. |
| [`AttestationStatus`](../cctp.rs#L330) | one burn message's state: pending, or complete with the signed message; a transaction's list is empty until Iris indexes it | [`IrisClient::attestations`](../cctp.rs#L417) | the strategy layer's treasury. |
| [`IrisError`](../cctp.rs#L340) | a failed request, an unexpected status, or a malformed answer, with a stated transience | the Iris calls, inside `PerpCityError::Iris` | `PerpCityError::is_transient`, and through it the strategy layer's retry loop. |

## Terminology

**Domain**: CCTP's chain number, not the chain ID; Arbitrum is 3 and Base
is 6. **Standard** finality waits for the source chain's hard finality and
is free; **Fast** attests at confirmation for a fee.
