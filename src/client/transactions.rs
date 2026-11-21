//! Transaction preparation, signing, broadcasting, and receipt polling.
//!
//! Transactions are built via [`TxBuilder`], obtained from
//! [`PerpClient::tx`]. The builder collects parameters and sends in a
//! single `.send()` call:
//!
//! ```rust,ignore
//! let receipt = client
//!     .tx(perp_manager, calldata)
//!     .with_urgency(Urgency::High)
//!     .send()
//!     .await?;
//! ```

use std::time::Duration;

use alloy::contract::Error as CallError;
use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::transports::TransportError;

use crate::constants::{RECEIPT_POLL_INTERVAL, RECEIPT_TIMEOUT};
use crate::errors::{
    ContractError, PerpCityError, Result, TransactionError, ValidationError, decode,
};
use crate::hft::gas::{Urgency, is_out_of_gas};
use crate::hft::pipeline::TxRequest;
use crate::transport::fault::{
    ExecutionEvidence, execution_evidence, is_execution_error, lacks_block, state_pruned,
};

use super::PerpClient;
use super::chain::NextBlock;
use super::queries::Era;

/// Delay before the first receipt poll — a just-broadcast transaction
/// rarely has a receipt sooner, so an earlier poll is a wasted call.
const RECEIPT_POLL_INITIAL_DELAY: Duration = Duration::from_secs(2);

// ── TxBuilder ───────────────────────────────────────────────────────

/// Builder for constructing and sending transactions.
///
/// Created via [`PerpClient::tx`]. Defaults: `value = 0`,
/// `gas_limit = None` (estimated at send), `urgency = Normal`.
#[derive(Debug)]
pub struct TxBuilder<'a> {
    client: &'a PerpClient,
    to: Address,
    calldata: Bytes,
    value: u128,
    gas_limit: Option<u64>,
    urgency: Urgency,
    market_era: Option<Era>,
}

impl<'a> TxBuilder<'a> {
    /// Use the market era already verified by the caller.
    pub(super) fn with_market_era(mut self, era: impl Into<Option<Era>>) -> Self {
        self.market_era = era.into();
        self
    }

    /// Attach ETH value to the transaction.
    pub fn with_value(mut self, value: u128) -> Self {
        self.value = value;
        self
    }

    /// Set an explicit gas limit, skipping gas estimation.
    ///
    /// The transaction is still simulated via `eth_call` capped at this
    /// limit before broadcast, so a would-be revert surfaces as a decoded
    /// [`TransactionError::SimulationReverted`] instead of a mined failure.
    /// A possible gas shortfall triggers a call without the cap. Success
    /// there is [`TransactionError::GasLimitTooLow`]. Uncapped gas
    /// exhaustion or an inconclusive reply is transient
    /// [`TransactionError::GasUnavailable`]. Both calls use the same hash
    /// on the pinned path (see [`send`](Self::send)). Use this when the estimate cannot be trusted (e.g.
    /// liquidations that have gone out-of-gas on Arbitrum where the
    /// estimate passed).
    pub fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = Some(gas_limit);
        self
    }

    /// Set the transaction urgency (affects EIP-1559 fee scaling).
    pub fn with_urgency(mut self, urgency: Urgency) -> Self {
        self.urgency = urgency;
        self
    }

    /// Simulate, sign, broadcast, and wait for the transaction receipt.
    ///
    /// Every send is simulated before broadcast. When `gas_limit` is `None`
    /// (the default), `eth_estimateGas` (or a cached estimate plus an
    /// `eth_call` preflight) provides both the limit and the simulation. An
    /// explicit `gas_limit` skips only the estimation — the `eth_call`
    /// preflight still runs so reverts decode into typed errors.
    ///
    /// Only a confirmed `v0.2.2` market uses the ordinary simulation.
    /// Other targets use the head hash with overrides for the next block's
    /// number and time. Without a known era, read the target's pool key.
    ///
    /// # Errors
    ///
    /// - The era read's failure: no answer from the node is
    ///   [`PerpCityError::Rpc`] (transient).
    /// - [`TransactionError::SimulationReverted`] for a decoded revert, and
    ///   [`TransactionError::SimulationFailed`] for the node's execution
    ///   failure without revert data.
    /// - [`TransactionError::GasLimitTooLow`] when an explicit limit fails
    ///   and the same call passes without the cap.
    /// - [`TransactionError::GasUnavailable`] (transient) when a simulation
    ///   has no definitive result after one more head read.
    pub async fn send(self) -> Result<TransactionReceipt> {
        let now = super::now_ms();

        // If a previous send left the nonce sequence in doubt (failed
        // broadcast, receipt timeout), repair it before preparing anything
        // new. The chain is the only authority on where the sequence stands:
        // rewinding or reusing a doubtful nonce locally spins forever
        // whenever the "failed" transaction actually landed. Resync is only
        // safe once nothing holds a nonce, so while transactions are still
        // in flight this fails fast with a transient error instead.
        let needs_resync = {
            let pipeline = self.client.pipeline.lock().unwrap();
            if pipeline.is_desynced() {
                if !pipeline.can_resync() {
                    return Err(TransactionError::NonceDesynced {
                        in_flight: pipeline.in_flight_count(),
                    }
                    .into());
                }
                true
            } else {
                false
            }
        };
        if needs_resync {
            // `pending` counts mempool transactions too, so a broadcast that
            // reported failure but propagated anyway keeps its nonce — the
            // resync steps past it instead of colliding with it.
            let count = self
                .client
                .chain()
                .provider()
                .get_transaction_count(self.client.address)
                .pending()
                .await?;
            let pipeline = self.client.pipeline.lock().unwrap();
            // Re-check under the lock: another sender may have resynced (or
            // started preparing) while we were fetching the count.
            if pipeline.is_desynced() && pipeline.can_resync() {
                pipeline.resync_nonce(count);
            }
        }

        // Captured before `self.calldata` is moved into the request: the
        // receipt check below needs it to evict a bad cached estimate.
        let selector: Option<[u8; 4]> = self.calldata.get(..4).map(|s| s.try_into().unwrap());
        if self.gas_limit == Some(0) {
            return Err(ValidationError::InvalidConfig {
                reason: "gas_limit must be > 0".into(),
            }
            .into());
        }
        let chain = self.client.chain();
        let era = match self.market_era {
            Some(era) => Some(era),
            None => match chain.market(self.to).era_at(BlockId::latest()).await {
                Ok(era) => Some(era),
                Err(PerpCityError::Abi(CallError::TransportError(error))) => {
                    if !error.as_error_resp().is_some_and(|resp| {
                        is_execution_error(resp)
                            || lacks_block(&resp.message)
                            || state_pruned(&resp.message)
                    }) {
                        return Err(error.into());
                    }
                    None
                }
                Err(
                    PerpCityError::Abi(_)
                    | PerpCityError::Contract(ContractError::UnknownBuild { .. }),
                ) => None,
                Err(error) => return Err(error),
            },
        };
        let pinned = era != Some(Era::Upgradeable);

        // Simulate + resolve gas limit. An explicit gas_limit skips only the
        // estimation: the preflight still runs so a would-be revert is
        // decoded before broadcast instead of burning the pinned gas.
        let resolved_gas_limit = match self.gas_limit {
            Some(limit) => {
                // Preflight at the pinned limit itself: a call that runs out
                // of gas at this limit must fail here, not on-chain.
                self.client
                    .chain()
                    .preflight_at_limit(
                        self.client.address,
                        self.to,
                        &self.calldata,
                        self.value,
                        limit,
                        pinned,
                    )
                    .await?;
                limit
            }
            None => {
                self.client
                    .simulate(self.to, &self.calldata, self.value, now, pinned)
                    .await?
            }
        };

        // Prepare via pipeline (zero RPC)
        let prepared = {
            let pipeline = self.client.pipeline.lock().unwrap();
            let fee_cache = self.client.chain().fee_cache().lock().unwrap();
            pipeline.prepare(
                TxRequest {
                    to: self.to.into_array(),
                    calldata: self.calldata.to_vec(),
                    value: self.value,
                    gas_limit: resolved_gas_limit,
                    urgency: self.urgency,
                },
                &fee_cache,
                now,
            )?
        };

        tracing::debug!(
            nonce = prepared.nonce,
            gas_limit = prepared.gas_limit,
            max_fee = prepared.gas_fees.max_fee_per_gas,
            priority_fee = prepared.gas_fees.max_priority_fee_per_gas,
            to = %self.to,
            urgency = ?self.urgency,
            "tx prepared"
        );

        // Build EIP-1559 transaction
        let tx = TransactionRequest::default()
            .with_to(self.to)
            .with_input(self.calldata)
            .with_value(U256::from(prepared.request.value))
            .with_nonce(prepared.nonce)
            .with_gas_limit(prepared.gas_limit)
            .with_max_fee_per_gas(prepared.gas_fees.max_fee_per_gas as u128)
            .with_max_priority_fee_per_gas(prepared.gas_fees.max_priority_fee_per_gas as u128)
            .with_chain_id(self.client.chain().chain_id());

        // Sign. A failure here is provably local — nothing was broadcast —
        // so the nonce can be handed straight back (a bare `?` would strand
        // it: acquired in prepare, accounted for only by record_submission).
        let tx_envelope = match tx.build(&self.client.wallet).await {
            Ok(envelope) => envelope,
            Err(e) => {
                let pipeline = self.client.pipeline.lock().unwrap();
                pipeline.abandon_prepared(prepared.nonce);
                return Err(TransactionError::SigningFailed {
                    reason: format!("{e}"),
                }
                .into());
            }
        };

        // Broadcast. A failure here is NOT provably local: the transaction
        // may never have left, may sit in the mempool, or may already be
        // mined — and no local bookkeeping can tell which. Releasing or
        // reusing the nonce guesses, and the wrong guess retries a consumed
        // nonce forever. Flag the doubt instead; the next send resyncs from
        // chain once nothing is in flight. The hash is known before the
        // request, so the caller can still look the transaction up.
        let tx_hash_b256 = *tx_envelope.tx_hash();
        if let Err(source) = self
            .client
            .chain()
            .provider()
            .send_tx_envelope(tx_envelope)
            .await
        {
            let pipeline = self.client.pipeline.lock().unwrap();
            pipeline.mark_desynced_prepared();
            return Err(TransactionError::BroadcastFailed {
                tx_hash: tx_hash_b256,
                source,
            }
            .into());
        }
        let tx_hash_bytes: [u8; 32] = tx_hash_b256.into();

        tracing::debug!(tx_hash = %tx_hash_b256, nonce = prepared.nonce, urgency = ?self.urgency, "tx broadcast");

        // Record in pipeline
        let prepared_gas_limit = prepared.gas_limit;
        {
            let mut pipeline = self.client.pipeline.lock().unwrap();
            pipeline.record_submission(tx_hash_bytes, prepared, now);
        }

        // Wait for receipt. A timeout does NOT mean the transaction died —
        // it is broadcast and may still mine later, so its nonce must never
        // be rewound or reused (`fail` would). Stop tracking it and flag the
        // doubt; the next send resyncs from chain, which counts the
        // transaction if and only if it is still live.
        tokio::time::sleep(RECEIPT_POLL_INITIAL_DELAY).await;
        let receipt = match self.client.poll_receipt(tx_hash_b256).await {
            Ok(receipt) => receipt,
            Err(e) => {
                let mut pipeline = self.client.pipeline.lock().unwrap();
                pipeline.resolve(&tx_hash_bytes);
                pipeline.mark_desynced();
                return Err(e);
            }
        };

        // Confirm in pipeline. This holds for reverted receipts too: a
        // reverted transaction MINED, so its nonce is consumed exactly as if
        // it had succeeded — never released.
        {
            let mut pipeline = self.client.pipeline.lock().unwrap();
            pipeline.resolve(&tx_hash_bytes);
        }

        // Out of gas, not reverted: the call was never disproved, only the
        // limit was too small. The capped pre-flight in `simulate()` evicts a
        // stale estimate that fails `eth_call`, but an Arbitrum out-of-gas
        // reproduces in neither `eth_call` nor `eth_estimateGas` — the receipt
        // is where it first shows, so it is where the estimate is dropped.
        let gas_used = receipt.gas_used as u64;
        if !receipt.status() && is_out_of_gas(gas_used, prepared_gas_limit) {
            if let Some(selector) = selector {
                self.client
                    .gas_limit_cache
                    .lock()
                    .unwrap()
                    .invalidate(&selector);
            }
            tracing::warn!(
                tx_hash = %tx_hash_b256,
                gas_used,
                gas_limit = prepared_gas_limit,
                "tx out of gas; cached estimate evicted"
            );
            return Err(TransactionError::OutOfGas {
                tx_hash: tx_hash_b256,
                gas_used,
                gas_limit: prepared_gas_limit,
            }
            .into());
        }

        // Check if reverted
        if !receipt.status() {
            tracing::warn!(tx_hash = %tx_hash_b256, "tx reverted");
            return Err(TransactionError::Reverted {
                tx_hash: tx_hash_b256,
                reason: format!("transaction {tx_hash_b256} reverted"),
            }
            .into());
        }

        tracing::debug!(
            tx_hash = %tx_hash_b256,
            block = ?receipt.block_number,
            gas_used = ?receipt.gas_used,
            "tx confirmed"
        );

        Ok(receipt)
    }
}

/// Build the preflight `eth_call` request. Kept as a pure function so the
/// request shape — in particular that a pinned gas limit is carried onto
/// the simulation — is unit-testable without a provider.
pub(super) fn preflight_request(
    from: Address,
    to: Address,
    calldata: &Bytes,
    value: u128,
    gas_limit: Option<u64>,
) -> TransactionRequest {
    let tx = TransactionRequest::default()
        .with_from(from)
        .with_to(to)
        .with_input(calldata.clone())
        .with_value(U256::from(value));
    match gas_limit {
        Some(limit) => tx.with_gas_limit(limit),
        None => tx,
    }
}

/// The receipt poll loop behind [`PerpClient::poll_receipt`], generic over
/// the provider so it is testable against a mocked transport. A receipt
/// of another hash than `tx_hash` is a failed read.
async fn wait_for_receipt<P: Provider>(provider: &P, tx_hash: B256) -> Result<TransactionReceipt> {
    let deadline = tokio::time::Instant::now() + RECEIPT_TIMEOUT;
    loop {
        let read = provider
            .get_transaction_receipt(tx_hash)
            .await
            .map_err(|e| e.to_string())
            .and_then(|receipt| match receipt {
                Some(receipt) if receipt.transaction_hash != tx_hash => Err(format!(
                    "the node answered with the receipt of {}",
                    receipt.transaction_hash
                )),
                receipt => Ok(receipt),
            });
        match read {
            Ok(Some(receipt)) => return Ok(receipt),
            Ok(None) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(TransactionError::ReceiptTimeout {
                        tx_hash,
                        reason: format!("no receipt after {}s", RECEIPT_TIMEOUT.as_secs()),
                    }
                    .into());
                }
            }
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(TransactionError::ReceiptTimeout {
                        tx_hash,
                        reason: format!("last receipt poll failed: {e}"),
                    }
                    .into());
                }
                tracing::warn!(tx_hash = %tx_hash, error = %e, "receipt poll RPC error, retrying");
            }
        }
        tokio::time::sleep(RECEIPT_POLL_INTERVAL).await;
    }
}

// ── PerpClient transaction methods ──────────────────────────────────

impl PerpClient {
    /// Start building a transaction.
    ///
    /// Returns a [`TxBuilder`] with defaults: `value = 0`,
    /// `gas_limit = None` (estimated at send), `urgency = Normal`.
    pub fn tx(&self, to: Address, calldata: Bytes) -> TxBuilder<'_> {
        TxBuilder {
            client: self,
            to,
            calldata,
            value: 0,
            gas_limit: None,
            urgency: Urgency::Normal,
            market_era: None,
        }
    }

    /// Wait for the receipt of `tx_hash`: poll at once, then every
    /// [`RECEIPT_POLL_INTERVAL`] for up to [`RECEIPT_TIMEOUT`].
    ///
    /// Use it to resolve a send whose outcome is unknown, with the hash from
    /// [`PerpCityError::tx_hash`]. On timeout it returns
    /// [`TransactionError::ReceiptTimeout`] for the same hash, so the call can
    /// repeat. A receipt the node gives for another hash is a failed poll.
    /// For one look-up with no wait, use
    /// `provider().get_transaction_receipt(tx_hash)`.
    ///
    /// The future holds no state, so dropping it only stops the polling;
    /// wrap it in `tokio::time::timeout` for a shorter deadline.
    ///
    /// It does not touch nonce tracking. A send that returned with a hash
    /// has already stopped tracking the transaction, and a doubtful nonce
    /// resyncs from chain before the next send whether or not this is
    /// called.
    pub async fn poll_receipt(&self, tx_hash: B256) -> Result<TransactionReceipt> {
        wait_for_receipt(self.chain().provider(), tx_hash).await
    }

    /// Resolve the gas limit by estimating it or preflighting a cached
    /// limit. A pinned preflight can re-estimate at the same hash when
    /// the capped call leaves the required gas in doubt.
    async fn simulate(
        &self,
        to: Address,
        calldata: &Bytes,
        value: u128,
        now: u64,
        pinned: bool,
    ) -> Result<u64> {
        let selector: Option<[u8; 4]> = calldata.get(..4).map(|bytes| bytes.try_into().unwrap());
        let cached = selector.and_then(|selector| {
            let cache = self.gas_limit_cache.lock().unwrap();
            cache
                .get(&selector, now)
                .map(|limit| (limit, cache.preflight_limit(limit)))
        });
        let chain = self.chain();
        let simulate = |next: Option<NextBlock>| async move {
            if let Some((limit, cap)) = cached {
                let cap = if next.is_some() { cap } else { limit };
                let tx = preflight_request(self.address, to, calldata, value, Some(cap));
                match chain.call_in(tx, next).await {
                    Ok(()) => return Ok((limit, false)),
                    Err(error) => {
                        let (failure, cap_failure) = simulation_failure(&error, "eth_call", true);
                        if cap_failure.is_none() {
                            return Err(failure);
                        }
                        if matches!(failure, TransactionError::SimulationFailed { .. })
                            && let Some(selector) = selector
                        {
                            self.gas_limit_cache.lock().unwrap().invalidate(&selector);
                        }
                        tracing::debug!(limit, %failure, "cached gas limit failed preflight; re-estimating");
                    }
                }
            }

            let tx = preflight_request(self.address, to, calldata, value, None);
            chain
                .estimate_gas_in(tx, next)
                .await
                .map(|raw| (raw, true))
                .map_err(|error| classify_simulation_failure(&error, "eth_estimateGas"))
        };

        let (limit, estimated) = if pinned {
            chain.in_next_block(|next| simulate(Some(next))).await
        } else {
            simulate(None).await
        }?;

        if !estimated {
            return Ok(limit);
        }
        let raw_estimate = limit;
        let Some(selector) = selector else {
            // A plain value transfer has no cache key. Keep the same buffer.
            return Ok(raw_estimate + raw_estimate / 5);
        };
        let buffered = {
            let mut cache = self.gas_limit_cache.lock().unwrap();
            cache.put(selector, raw_estimate, now);
            cache.get(&selector, now).unwrap()
        };
        tracing::debug!(
            selector = %alloy::primitives::hex::encode(selector),
            raw_estimate,
            buffered,
            "gas estimate cached"
        );
        Ok(buffered)
    }
}

/// Classify an RPC failure from any simulation path.
pub(super) fn classify_simulation_failure(
    error: &TransportError,
    method: &str,
) -> TransactionError {
    simulation_failure(error, method, false).0
}

/// Also name failures a gas cap can explain, for a same-block uncapped check.
pub(super) fn simulation_failure(
    error: &TransportError,
    method: &str,
    capped: bool,
) -> (TransactionError, Option<String>) {
    let resp = error.as_error_resp();
    let evidence = resp.and_then(execution_evidence);
    let reason = format!("{method} failed: {error}");
    match evidence {
        Some(ExecutionEvidence::Revert(data)) => {
            if let Some((error_name, selector)) = data.as_deref().and_then(decode::decode_revert) {
                let cap_failure = capped.then(|| error_name.clone());
                return (
                    TransactionError::SimulationReverted {
                        error_name,
                        selector: selector.into(),
                        revert_data: data.filter(|data| data.len() > 10),
                    },
                    cap_failure,
                );
            }
            let cap_failure = capped.then(|| "execution reverted".into());
            return (TransactionError::SimulationFailed { reason }, cap_failure);
        }
        Some(ExecutionEvidence::OutOfGas) if capped => {
            return (
                TransactionError::SimulationFailed { reason },
                Some("out of gas".into()),
            );
        }
        Some(ExecutionEvidence::OutOfGas) => {
            return (TransactionError::GasUnavailable { reason }, None);
        }
        Some(ExecutionEvidence::Halt) => {
            return (TransactionError::SimulationFailed { reason }, None);
        }
        None => {}
    }
    if let Some(resp) = resp {
        let message = resp.message.to_ascii_lowercase();
        let intrinsic = message.contains("intrinsic gas too low");
        if intrinsic
            || [
                "insufficient funds",
                "nonce too low",
                "nonce too high",
                "already known",
            ]
            .iter()
            .any(|refusal| message.contains(refusal))
        {
            let cap_failure = (capped && intrinsic).then(|| "intrinsic gas too low".into());
            return (TransactionError::SimulationFailed { reason }, cap_failure);
        }
    }
    (TransactionError::GasUnavailable { reason }, None)
}

#[cfg(test)]
mod tests {
    use alloy::consensus::{Receipt, ReceiptEnvelope, Transaction as _};
    use alloy::primitives::{I256, aliases::I24};
    use alloy::providers::ProviderBuilder;
    use alloy::sol_types::{SolCall, SolError};
    use alloy::transports::TransportErrorKind;
    use alloy::transports::mock::Asserter;
    use serde_json::Value;
    use serde_json::value::RawValue;

    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::client::{PositionRole, mock, now_ms};
    use crate::contracts::{Maker, Perp as EarlierPerp, PerpV022, Position, v1};
    use crate::convert::pack_balance_delta;
    use crate::errors::{PerpCityError, RevertClass};
    use crate::hft::gas::GasLimits;

    /// Solady's `TransferFromFailed()`, which a transfer that runs out of
    /// gas inside the token reverts with.
    const TRANSFER_FROM_FAILED: [u8; 4] = [0x79, 0x39, 0xf4, 0x24];

    /// A successful receipt for `hash`, from the client to its market.
    fn mined(client: &PerpClient, hash: B256) -> TransactionReceipt {
        let receipt = Receipt {
            status: true.into(),
            cumulative_gas_used: 100_000,
            logs: Vec::new(),
        };
        TransactionReceipt {
            inner: ReceiptEnvelope::Eip1559(receipt.with_bloom()),
            transaction_hash: hash,
            transaction_index: Some(0),
            block_hash: Some(B256::with_last_byte(1)),
            block_number: Some(92),
            gas_used: 100_000,
            effective_gas_price: 80,
            blob_gas_used: None,
            blob_gas_price: None,
            from: client.address(),
            to: Some(mock::PERP),
            contract_address: None,
        }
    }

    /// The node takes the next signed transaction and mines it.
    fn answer_send(rpc: &mock::Rpc, client: &PerpClient) {
        let hash = B256::repeat_byte(0x5e);
        rpc.response(&hash);
        rpc.response(&mined(client, hash));
    }

    /// A client with fresh fees and `limit` cached for `selector`.
    fn cached(selector: [u8; 4], raw_estimate: u64) -> (PerpClient, mock::Rpc) {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        client
            .gas_limit_cache
            .lock()
            .unwrap()
            .put(selector, raw_estimate, now_ms());
        (client, rpc)
    }

    fn calldata(selector: [u8; 4]) -> Bytes {
        let mut data = selector.to_vec();
        data.extend_from_slice(&[7; 32]);
        data.into()
    }

    fn cached_limit(client: &PerpClient, selector: [u8; 4]) -> Option<u64> {
        client
            .gas_limit_cache
            .lock()
            .unwrap()
            .get(&selector, now_ms())
    }

    /// A capped preflight that reverts may only have run short of gas
    /// inside a transfer: it is estimated again without the cap, and a
    /// success there replaces the cached limit and sends the trade with
    /// the new one. A revert there too is the call's answer, and the
    /// cached limit stays.
    #[tokio::test(start_paused = true)]
    async fn a_capped_preflight_revert_is_estimated_again_without_the_cap() {
        let selector = v1::Perp::openMakerCall::SELECTOR;
        let (client, rpc) = cached(selector, 500_000);
        assert_eq!(
            cached_limit(&client, selector),
            Some(GasLimits::V1_OPEN_MAKER)
        );
        rpc.block(500, 1_000);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.quantity(900_000);
        answer_send(&rpc, &client);
        let receipt = client
            .tx(mock::PERP, calldata(selector))
            .with_market_era(Era::V1)
            .send()
            .await
            .unwrap();
        assert!(receipt.status());
        assert_eq!(
            rpc.methods(),
            [
                "eth_getBlockByNumber",
                "eth_call",
                "eth_estimateGas",
                "eth_sendRawTransaction",
                "eth_getTransactionReceipt"
            ]
        );
        assert_eq!(rpc.call_gas(), [Some(791_667)]);
        assert!(rpc.params("eth_estimateGas")[0][0].get("gas").is_none());
        let sent = rpc.sent_txs();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].gas_limit(), 1_080_000);
        assert_eq!(cached_limit(&client, selector), Some(1_080_000));
        assert!(rpc.is_drained());

        let (client, rpc) = cached(selector, 500_000);
        rpc.block(500, 1_000);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        let err = client
            .tx(mock::PERP, calldata(selector))
            .with_market_era(Era::V1)
            .send()
            .await
            .unwrap_err();
        assert!(
            matches!(&err, PerpCityError::Transaction(TransactionError::SimulationReverted {
                selector, ..
            }) if selector.0 == TRANSFER_FROM_FAILED),
            "{err}"
        );
        assert!(rpc.sent_txs().is_empty());
        assert_eq!(
            cached_limit(&client, selector),
            Some(GasLimits::V1_OPEN_MAKER)
        );
        assert!(rpc.is_drained());
    }

    /// The block overrides of `params`, an `eth_call`'s or an
    /// `eth_estimateGas`'s.
    fn overrides(params: &Value) -> &Value {
        &params[3]
    }

    fn block_pin(hash: B256) -> Value {
        serde_json::json!({"blockHash": hash})
    }

    /// Both liquidation roles use their resolved era when they preflight.
    #[tokio::test(start_paused = true)]
    async fn liquidation_sends_preflight_on_the_market_era() {
        let recipient = Address::repeat_byte(0x51);
        let pos_id = U256::from(7);
        for era in [Era::V1, Era::Upgradeable] {
            for role in [PositionRole::Taker, PositionRole::Maker] {
                let (client, rpc) = mock::client();
                client.chain().set_base_fee(100);
                rpc.immutables(era);
                let delta = pack_balance_delta(-2_500_000, 3_000_000);
                match (era, role) {
                    (Era::V1, PositionRole::Taker) => {
                        rpc.call::<v1::Perp::positionsCall>(&v1::Position {
                            delta,
                            margin: 1_000_000,
                            lastCumlFundingX96: I256::ZERO,
                        });
                    }
                    (Era::V1, PositionRole::Maker) => {
                        rpc.call::<v1::Perp::makerDetailsCall>(&v1::Maker {
                            tickLower: I24::try_from(-60).unwrap(),
                            tickUpper: I24::try_from(60).unwrap(),
                            liquidity: 335_000_000,
                            lastLpFeeGrowthInsideX128: U256::ZERO,
                            lastCumlFunding: v1::MakerFunding {
                                belowX96: I256::ZERO,
                                withinInventoryX192: I256::ZERO,
                            },
                            lastFundingCorrectionX128: I256::ZERO,
                        });
                    }
                    (Era::Upgradeable, PositionRole::Taker) => {
                        rpc.call::<EarlierPerp::positionsCall>(&Position {
                            delta,
                            ..mock::position(1_000_000)
                        });
                    }
                    (Era::Upgradeable, PositionRole::Maker) => {
                        rpc.call::<EarlierPerp::makerDetailsCall>(&Maker {
                            liquidity: 335_000_000,
                            ..mock::maker(-60, 60, 0)
                        });
                    }
                    (Era::Legacy, _) => unreachable!(),
                }
                let head = (era == Era::V1).then(|| rpc.block(500, 1_000));
                rpc.response(&Bytes::new());
                answer_send(&rpc, &client);

                let receipt = match role {
                    PositionRole::Taker => {
                        client
                            .liquidate_taker(pos_id, recipient, Urgency::Normal)
                            .await
                    }
                    PositionRole::Maker => {
                        client
                            .liquidate_maker(pos_id, recipient, Urgency::Normal)
                            .await
                    }
                }
                .unwrap();
                assert!(receipt.status());
                let calls = rpc.params("eth_call");
                let preflight = calls.last().unwrap();
                assert_eq!(rpc.call_gas().last(), Some(&Some(GasLimits::LIQUIDATE)));
                assert_eq!(
                    &rpc.methods()[rpc.methods().len() - 3..],
                    [
                        "eth_call",
                        "eth_sendRawTransaction",
                        "eth_getTransactionReceipt"
                    ]
                );
                if let Some(hash) = head {
                    assert_eq!(preflight[1], block_pin(hash));
                    assert_eq!(preflight.as_array().unwrap().len(), 4);
                    assert_eq!(
                        overrides(preflight),
                        &serde_json::json!({"number": "0x1f5", "time": "0x3e9"})
                    );
                } else {
                    assert_eq!(preflight.as_array().unwrap().len(), 2);
                    assert_eq!(preflight[1], "pending");
                    assert!(rpc.params("eth_getBlockByNumber").is_empty());
                }
                assert!(rpc.is_drained());
            }
        }
    }

    /// A v1 trade is estimated and preflighted at the head's state in the
    /// next second, where it accrues as a mined trade does, and a cache
    /// hit sends the cached limit, not the preflight's.
    #[tokio::test(start_paused = true)]
    async fn a_v1_trade_simulates_in_the_next_second() {
        let selector = v1::Perp::adjustTakerCall::SELECTOR;
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let head = rpc.block(500, 1_000);
        rpc.quantity(1_300_000);
        answer_send(&rpc, &client);
        client
            .tx(mock::PERP, calldata(selector))
            .with_market_era(Era::V1)
            .send()
            .await
            .unwrap();
        let estimate = &rpc.params("eth_estimateGas")[0];
        assert_eq!(rpc.params("eth_getBlockByNumber")[0][0], "latest");
        assert_eq!(estimate[1], block_pin(head));
        assert_eq!(
            overrides(estimate),
            &serde_json::json!({"number": "0x1f5", "time": "0x3e9"})
        );
        assert_eq!(rpc.sent_txs()[0].gas_limit(), 1_560_000);

        let head = rpc.block(501, 1_000);
        rpc.response(&Bytes::new());
        answer_send(&rpc, &client);
        client
            .tx(mock::PERP, calldata(selector))
            .with_market_era(Era::V1)
            .send()
            .await
            .unwrap();
        let preflight = &rpc.params("eth_call")[0];
        assert_eq!(preflight[1], block_pin(head));
        assert_eq!(
            overrides(preflight),
            &serde_json::json!({"number": "0x1f6", "time": "0x3e9"})
        );
        assert_eq!(rpc.call_gas(), [Some(1_300_000)]);
        assert_eq!(rpc.sent_txs()[1].gas_limit(), 1_560_000, "the cached limit");
        assert!(rpc.is_drained());
    }

    /// Through the real transport, with one endpoint that refuses block
    /// overrides on every simulation: each path, an explicit limit
    /// included, reads the head again once, simulates once more with the
    /// overrides, and is refused as transient. No simulation runs without
    /// them, nothing is recorded, and the next send asks again.
    #[tokio::test]
    async fn a_refused_override_is_tried_once_more_and_never_dropped() {
        use crate::transport::scripted::{self, Answer};
        use crate::{HftTransport, TransportConfig};

        for path in [Path::Estimate, Path::CacheHit, Path::Explicit] {
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let log = Arc::clone(&seen);
            let head = serde_json::to_value(mock::block(500, 1_000)).unwrap();
            let url = scripted::endpoint(move |method, params| {
                let count = params.as_array().map_or(0, Vec::len);
                log.lock().unwrap().push(format!("{method}/{count}"));
                match (method, count) {
                    ("eth_getBlockByNumber", _) => Answer::Result(head.clone()),
                    ("eth_call" | "eth_estimateGas", 4) => {
                        Answer::Error(-32602, "too many arguments, want at most 3".into())
                    }
                    ("eth_call", _) => Answer::Result("0x".into()),
                    _ => Answer::Result("0x5208".into()),
                }
            })
            .await;
            let transport = HftTransport::new(
                TransportConfig::builder()
                    .shared_endpoint(&url)
                    .build()
                    .unwrap(),
            )
            .unwrap();
            let client = mock::client_through(transport);
            client.chain().set_base_fee(100);
            if let Path::CacheHit = path {
                client.gas_limit_cache.lock().unwrap().put(
                    v1::Perp::adjustTakerCall::SELECTOR,
                    1_300_000,
                    now_ms(),
                );
            }
            let simulation = match path {
                Path::Estimate => "eth_estimateGas/4",
                Path::CacheHit | Path::Explicit => "eth_call/4",
            };
            for _ in 0..2 {
                seen.lock().unwrap().clear();
                let err = send_on(path, &client).await.unwrap_err();
                assert!(
                    matches!(
                        &err,
                        PerpCityError::Transaction(TransactionError::GasUnavailable { reason })
                            if reason.contains("too many arguments")
                    ),
                    "{path:?}: {err}"
                );
                assert!(err.is_transient());
                assert_eq!(
                    *seen.lock().unwrap(),
                    [
                        "eth_getBlockByNumber/2",
                        simulation,
                        "eth_getBlockByNumber/2",
                        simulation
                    ],
                    "{path:?}"
                );
            }
        }
    }

    /// An initial miss uses the only retry, even if the next head exists.
    #[tokio::test(start_paused = true)]
    async fn an_initial_head_miss_uses_the_one_retry() {
        for path in [Path::Estimate, Path::CacheHit, Path::Explicit] {
            for (second_head, succeeds) in [(false, false), (true, false), (true, true)] {
                let (client, rpc) = v1_trade_on(path);
                rpc.no_block();
                if second_head {
                    rpc.block(500, 1_000);
                    if succeeds {
                        simulation_succeeds(path, &rpc);
                        answer_send(&rpc, &client);
                    } else {
                        rpc.node_error(-32000, MISSING_HASH);
                    }
                } else {
                    rpc.no_block();
                }
                let result = send_on(path, &client).await;
                if succeeds {
                    assert!(result.is_ok(), "{path:?}: {result:?}");
                } else {
                    let cause = if second_head {
                        MISSING_HASH
                    } else {
                        "no head block"
                    };
                    assert!(
                        matches!(result, Err(PerpCityError::Transaction(
                            TransactionError::GasUnavailable { ref reason }
                        )) if reason.contains(cause)),
                        "{path:?}: {result:?}"
                    );
                }
                assert_eq!(rpc.params("eth_getBlockByNumber").len(), 2);
                assert_eq!(rpc.sent_txs().len(), usize::from(succeeds));
                assert!(rpc.is_drained());
            }
        }
    }

    /// A zero limit is refused before any request.
    #[tokio::test]
    async fn a_zero_gas_limit_is_refused_before_any_request() {
        for selector in [
            v1::Perp::openTakerCall::SELECTOR,
            EarlierPerp::openMakerCall::SELECTOR,
        ] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            let err = client
                .tx(mock::PERP, calldata(selector))
                .with_gas_limit(0)
                .send()
                .await
                .unwrap_err();
            assert!(
                matches!(
                    err,
                    PerpCityError::Validation(ValidationError::InvalidConfig { .. })
                ),
                "{err}"
            );
            assert!(rpc.methods().is_empty());
        }
    }

    /// An explicit limit on a v1 trade is preflighted at that limit in the
    /// next second, where the mined trade accrues.
    #[tokio::test(start_paused = true)]
    async fn an_explicit_v1_limit_is_preflighted_in_the_next_second() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        rpc.block(500, 1_000);
        rpc.response(&Bytes::new());
        answer_send(&rpc, &client);
        client
            .tx(mock::PERP, calldata(v1::Perp::moveMakerCall::SELECTOR))
            .with_market_era(Era::V1)
            .with_gas_limit(2_000_000)
            .send()
            .await
            .unwrap();
        let preflight = &rpc.params("eth_call")[0];
        assert_eq!(preflight[1], block_pin(mock::block(500, 1_000).header.hash));
        assert_eq!(overrides(preflight)["time"], "0x3e9");
        assert_eq!(rpc.call_gas(), [Some(2_000_000)]);
        assert_eq!(rpc.sent_txs()[0].gas_limit(), 2_000_000);
        assert!(rpc.is_drained());
    }

    /// A cached limit the call runs out of inside its preflight is
    /// dropped even when the estimate after it gets no answer.
    #[tokio::test]
    async fn an_out_of_gas_preflight_drops_the_limit_before_the_estimate() {
        let selector = EarlierPerp::openMakerCall::SELECTOR;
        for era in [Era::V1, Era::Upgradeable] {
            let (client, rpc) = cached(selector, 500_000);
            if era == Era::V1 {
                rpc.block(500, 1_000);
            }
            rpc.node_error(-32000, "out of gas");
            rpc.fails("request timed out");
            if era == Era::V1 {
                rpc.block(501, 1_001);
                rpc.fails("request timed out");
            }
            let err = client
                .tx(mock::PERP, calldata(selector))
                .with_market_era(era)
                .send()
                .await
                .unwrap_err();
            assert!(err.is_transient(), "{err}");
            assert_eq!(cached_limit(&client, selector), None);
            assert_eq!(rpc.params("eth_estimateGas").len(), 1);
            assert!(rpc.is_drained());
        }
    }

    /// geth's answer for a block hash it has not seen, which a node behind
    /// the one that served the head gives. Every simulation pins the head
    /// by its hash.
    const MISSING_HASH: &str = "header not found";

    /// The answers that send a simulation to a head read again: a missing
    /// block, and a refusal of the block overrides as invalid params.
    const NEW_HEAD: [(i64, &str); 2] = [
        (-32000, MISSING_HASH),
        (-32602, "too many arguments, want at most 3"),
    ];

    /// How a v1 trade resolves its limit: an estimate, a cache hit's
    /// preflight, or an explicit limit's preflight.
    #[derive(Clone, Copy, Debug)]
    enum Path {
        Estimate,
        CacheHit,
        Explicit,
    }

    /// A v1 trade's client for `path`, and the send for it.
    fn v1_trade_on(path: Path) -> (PerpClient, mock::Rpc) {
        let selector = v1::Perp::adjustTakerCall::SELECTOR;
        let (client, rpc) = match path {
            Path::CacheHit => cached(selector, 1_300_000),
            Path::Estimate | Path::Explicit => mock::client(),
        };
        client.chain().set_base_fee(100);
        (client, rpc)
    }

    async fn send_on(path: Path, client: &PerpClient) -> Result<TransactionReceipt> {
        let tx = client
            .tx(mock::PERP, calldata(v1::Perp::adjustTakerCall::SELECTOR))
            .with_market_era(Era::V1);
        match path {
            Path::Explicit => tx.with_gas_limit(2_000_000).send().await,
            Path::Estimate | Path::CacheHit => tx.send().await,
        }
    }

    /// The method `path` simulates with, and its successful answer.
    fn simulation_succeeds(path: Path, rpc: &mock::Rpc) -> &'static str {
        match path {
            Path::Estimate => {
                rpc.quantity(1_300_000);
                "eth_estimateGas"
            }
            Path::CacheHit | Path::Explicit => {
                rpc.response(&Bytes::new());
                "eth_call"
            }
        }
    }

    /// A node without the head the simulation pins (geth's "header not
    /// found"), or one that refuses the block overrides, is
    /// answered by reading the head again and simulating once more with
    /// the overrides, on each path; the second simulation pins the new
    /// head.
    #[tokio::test(start_paused = true)]
    async fn a_missing_pinned_head_is_read_again_once() {
        for path in [Path::Estimate, Path::CacheHit, Path::Explicit] {
            for (code, message) in NEW_HEAD {
                let (client, rpc) = v1_trade_on(path);
                let old = rpc.block(500, 1_000);
                rpc.node_error(code, message);
                let new = rpc.block(498, 996);
                let method = simulation_succeeds(path, &rpc);
                answer_send(&rpc, &client);
                send_on(path, &client).await.unwrap();
                let pins: Vec<_> = rpc
                    .params(method)
                    .iter()
                    .map(|params| (params[1].clone(), overrides(params)["time"].clone()))
                    .collect();
                assert_eq!(
                    pins,
                    [
                        (block_pin(old), "0x3e9".into()),
                        (block_pin(new), "0x3e5".into())
                    ],
                    "{path:?}"
                );
                assert_eq!(rpc.sent_txs().len(), 1, "{path:?}");
                assert!(rpc.is_drained(), "{path:?}");
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn wrapped_missing_blocks_and_quotas_retry_all_pinned_send_paths() {
        for path in [Path::Estimate, Path::CacheHit, Path::Explicit] {
            for data in WRAPPED_SIMULATION_FAILURES {
                let (client, rpc) = v1_trade_on(path);
                let old = rpc.block(500, 1_000);
                rpc.answers_error(-32603, "Internal error", Some(data));
                let new = rpc.block(501, 1_001);
                let method = simulation_succeeds(path, &rpc);
                answer_send(&rpc, &client);

                send_on(path, &client).await.unwrap();

                assert_eq!(
                    rpc.params("eth_getBlockByNumber").len(),
                    2,
                    "{path:?}: {data}"
                );
                let params = rpc.params(method);
                assert_eq!(params.len(), 2, "{path:?}: {data}");
                assert_eq!(params[0][1], block_pin(old));
                assert_eq!(params[1][1], block_pin(new));
                if matches!(path, Path::CacheHit) {
                    assert!(rpc.params("eth_estimateGas").is_empty());
                }
                assert!(rpc.is_drained());
            }
        }
    }

    const WRAPPED_SIMULATION_FAILURES: [&str; 4] = [
        r#"{"originalError":{"code":-32000,"message":"header not found","data":"0x1f5cfa71"}}"#,
        r#"{"originalError":{"code":-32005,"message":"rate limit exceeded","data":"0x000003e8"}}"#,
        r#"{"originalError":{"code":-32005,"message":"Exceeded max limit of 100 requests per second","data":"0x12345678"}}"#,
        r#"{"originalError":[{"code":-32005,"message":"rate limit exceeded","data":"0x000003e8"}]}"#,
    ];

    #[tokio::test(start_paused = true)]
    async fn wrapped_missing_blocks_and_quotas_are_transient_on_ordinary_sends() {
        for data in WRAPPED_SIMULATION_FAILURES {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            rpc.answers_error(-32603, "Internal error", Some(data));

            let result = client
                .tx(mock::PERP, calldata(v1::Perp::adjustTakerCall::SELECTOR))
                .with_market_era(Era::Upgradeable)
                .send()
                .await;
            assert!(
                matches!(&result,
                    Err(PerpCityError::Transaction(TransactionError::GasUnavailable { reason }))
                    if reason.contains("Internal error")),
                "{data}: {result:?}"
            );
            assert!(rpc.params("eth_getBlockByNumber").is_empty());
            assert_eq!(rpc.params("eth_estimateGas").len(), 1);
            assert!(rpc.is_drained());
        }
    }

    /// A second miss, or a failed head read after the first, is no
    /// answer: transient, nothing sent, and a cached limit kept. Not the
    /// call's failure.
    #[tokio::test(start_paused = true)]
    async fn a_missing_pinned_head_twice_is_no_answer() {
        for path in [Path::Estimate, Path::CacheHit, Path::Explicit] {
            for ((code, message), head_read_fails) in NEW_HEAD
                .into_iter()
                .flat_map(|refusal| [(refusal, false), (refusal, true)])
            {
                let (client, rpc) = v1_trade_on(path);
                rpc.block(500, 1_000);
                rpc.node_error(code, message);
                if head_read_fails {
                    rpc.fails("request timed out");
                } else {
                    rpc.block(500, 1_000);
                    rpc.node_error(code, message);
                }
                let err = send_on(path, &client).await.unwrap_err();
                assert!(
                    matches!(
                        &err,
                        PerpCityError::Transaction(TransactionError::GasUnavailable { reason })
                            if reason.contains(message)
                    ),
                    "{path:?}: {err}"
                );
                assert!(err.is_transient());
                assert!(rpc.sent_txs().is_empty());
                if let Path::CacheHit = path {
                    assert_eq!(
                        cached_limit(&client, v1::Perp::adjustTakerCall::SELECTOR),
                        Some(1_560_000)
                    );
                }
                assert!(rpc.is_drained(), "{path:?}");
            }
        }
    }

    /// A v1 cache hit whose capped preflight fails is estimated again in
    /// the same next block, never at the endpoint's own block: after a
    /// revert that may be the gas running out in a transfer, and after
    /// running out of gas.
    #[tokio::test(start_paused = true)]
    async fn a_v1_cache_hit_that_fails_is_estimated_in_the_next_block() {
        let selector = v1::Perp::adjustTakerCall::SELECTOR;
        for revert in [Some(&TRANSFER_FROM_FAILED[..]), Some(&[][..]), None] {
            let (client, rpc) = cached(selector, 1_300_000);
            rpc.block(500, 1_000);
            if let Some(data) = revert {
                rpc.reverts(data);
            } else {
                rpc.node_error(-32000, "out of gas");
            }
            rpc.quantity(2_000_000);
            answer_send(&rpc, &client);
            client
                .tx(mock::PERP, calldata(selector))
                .with_market_era(Era::V1)
                .send()
                .await
                .unwrap();
            assert_eq!(rpc.call_gas(), [Some(1_300_000)]);
            let estimate = &rpc.params("eth_estimateGas")[0];
            assert!(estimate[0].get("gas").is_none());
            assert_eq!(estimate[1], block_pin(mock::block(500, 1_000).header.hash));
            assert_eq!(
                overrides(estimate),
                &serde_json::json!({"number": "0x1f5", "time": "0x3e9"})
            );
            assert_eq!(rpc.sent_txs()[0].gas_limit(), 2_400_000);
            assert_eq!(cached_limit(&client, selector), Some(2_400_000));
            assert!(rpc.is_drained());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_cached_limit_repeats_the_pair_if_the_capped_call_misses() {
        let (client, rpc) = v1_trade_on(Path::CacheHit);
        let old = rpc.block(500, 1_000);
        rpc.node_error(-32000, MISSING_HASH);
        let new = rpc.block(501, 1_002);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.quantity(1_700_000);
        answer_send(&rpc, &client);
        send_on(Path::CacheHit, &client).await.unwrap();

        let calls = rpc.params("eth_call");
        assert_eq!(calls[0][1], block_pin(old));
        assert_eq!(calls[1][1], block_pin(new));
        assert_eq!(rpc.params("eth_estimateGas")[0][1], block_pin(new));
        assert_eq!(rpc.sent_txs()[0].gas_limit(), 2_040_000);
        assert!(rpc.is_drained());
    }

    #[tokio::test(start_paused = true)]
    async fn a_cached_limit_repeats_the_pair_if_the_estimate_misses() {
        let (client, rpc) = v1_trade_on(Path::CacheHit);
        let old = rpc.block(500, 1_000);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.node_error(-32000, MISSING_HASH);
        let new = rpc.block(501, 1_002);
        rpc.response(&Bytes::new());
        answer_send(&rpc, &client);
        send_on(Path::CacheHit, &client).await.unwrap();

        let calls = rpc.params("eth_call");
        assert_eq!(calls[0][1], block_pin(old));
        assert_eq!(calls[1][1], block_pin(new));
        let estimates = rpc.params("eth_estimateGas");
        assert_eq!(estimates.len(), 1);
        assert_eq!(estimates[0][1], block_pin(old));
        assert_eq!(rpc.sent_txs()[0].gas_limit(), 1_560_000);
        assert!(rpc.is_drained());
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_revert_at_an_explicit_limit_is_checked_without_the_cap() {
        for era in [Era::Upgradeable, Era::V1] {
            for uncapped_reverts in [false, true] {
                let (client, rpc) = v1_trade_on(Path::Explicit);
                let head = (era == Era::V1).then(|| rpc.block(500, 1_000));
                rpc.reverts(&[]);
                if uncapped_reverts {
                    rpc.reverts(&TRANSFER_FROM_FAILED);
                } else {
                    rpc.response(&Bytes::new());
                }
                let error = client
                    .tx(mock::PERP, calldata(v1::Perp::adjustTakerCall::SELECTOR))
                    .with_market_era(era)
                    .with_gas_limit(2_000_000)
                    .send()
                    .await
                    .unwrap_err();
                if uncapped_reverts {
                    assert!(
                        matches!(error, PerpCityError::Transaction(TransactionError::SimulationReverted {
                            selector, ..
                        }) if selector.0 == TRANSFER_FROM_FAILED),
                        "{error}"
                    );
                } else {
                    assert!(
                        matches!(error, PerpCityError::Transaction(TransactionError::GasLimitTooLow {
                            gas_limit: 2_000_000, ref error_name
                        }) if error_name == "execution reverted"),
                        "{error}"
                    );
                }
                assert_eq!(rpc.call_gas(), [Some(2_000_000), None]);
                for call in rpc.params("eth_call") {
                    if let Some(head) = head {
                        assert_eq!(call[1], block_pin(head));
                    } else {
                        assert_eq!(call.as_array().unwrap().len(), 2);
                    }
                }
                assert!(rpc.sent_txs().is_empty());
                assert!(rpc.is_drained());
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_revert_reestimates_cached_v022_and_nonmarket_sends() {
        let selector = [0xa9, 0x05, 0x9c, 0xbb]; // transfer(address,uint256)
        for ordinary in [false, true] {
            let (client, rpc) = cached(selector, 45_415);
            if ordinary {
                rpc.era_probe(Era::Upgradeable);
            } else {
                rpc.reverts(&[]);
            }
            let head = (!ordinary).then(|| rpc.block(500, 1_000));
            rpc.reverts(&[]);
            rpc.quantity(60_000);
            answer_send(&rpc, &client);
            client
                .tx(mock::PERP, calldata(selector))
                .send()
                .await
                .unwrap();
            let estimates = rpc.params("eth_estimateGas");
            assert_eq!(estimates.len(), 1);
            if let Some(head) = head {
                assert_eq!(estimates[0][1], block_pin(head));
            } else {
                assert_eq!(estimates[0].as_array().unwrap().len(), 2);
            }
            assert!(estimates[0][0].get("gas").is_none());
            assert_eq!(cached_limit(&client, selector), Some(72_000));
            assert_eq!(rpc.sent_txs()[0].gas_limit(), 72_000);
            assert!(rpc.is_drained());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cold_v022_sends_cache_the_positive_identity() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let selector = EarlierPerp::openMakerCall::SELECTOR;
        rpc.era_probe(Era::Upgradeable);
        rpc.quantity(200_000);
        answer_send(&rpc, &client);
        rpc.raw(&[]);
        answer_send(&rpc, &client);
        for _ in 0..2 {
            client
                .tx(mock::PERP, calldata(selector))
                .send()
                .await
                .unwrap();
        }
        assert_eq!(rpc.call_gas(), [None, Some(800_000)]);
        assert!(rpc.params("eth_getBlockByNumber").is_empty());
        rpc.call::<EarlierPerp::POOL_IDCall>(&mock::POOL_ID);
        assert_eq!(client.market().era().await.unwrap(), Era::Upgradeable);
        assert_eq!(rpc.call_gas(), [None, Some(800_000), None]);
        assert!(rpc.is_drained());
    }

    /// An explicit limit whose preflight reverts is run once more without
    /// the cap at the same hash. Success there identifies a low limit.
    #[tokio::test(start_paused = true)]
    async fn an_explicit_limit_that_reverts_is_checked_without_the_cap() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        rpc.block(500, 1_000);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.response(&Bytes::new());
        let err = client
            .tx(mock::PERP, calldata(v1::Perp::moveMakerCall::SELECTOR))
            .with_market_era(Era::V1)
            .with_gas_limit(400_000)
            .send()
            .await
            .unwrap_err();
        assert!(
            matches!(
                &err,
                PerpCityError::Transaction(TransactionError::GasLimitTooLow {
                    gas_limit: 400_000,
                    error_name,
                }) if error_name == "TransferFromFailed"
            ),
            "{err}"
        );
        assert!(!err.is_transient());
        assert_eq!(rpc.call_gas(), [Some(400_000), None]);
        for call in rpc.params("eth_call") {
            assert_eq!(call[1], block_pin(mock::block(500, 1_000).header.hash));
            assert_eq!(overrides(&call)["time"], "0x3e9");
        }
        assert!(rpc.sent_txs().is_empty());
        assert!(rpc.is_drained());
    }

    #[tokio::test]
    async fn an_uncapped_decoded_revert_takes_precedence() {
        let (client, rpc) = v1_trade_on(Path::Explicit);
        rpc.block(500, 1_000);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        let real_revert = v1::Perp::DeadlineExpired::SELECTOR;
        rpc.reverts(&real_revert);
        let err = send_on(Path::Explicit, &client).await.unwrap_err();
        assert!(
            matches!(&err, PerpCityError::Transaction(TransactionError::SimulationReverted {
                selector, ..
            }) if selector.0 == real_revert),
            "{err}"
        );
        assert_eq!(rpc.call_gas(), [Some(2_000_000), None]);
        assert!(rpc.sent_txs().is_empty());
        assert!(rpc.is_drained());
    }

    #[tokio::test(start_paused = true)]
    async fn a_v022_capped_revert_is_checked_at_pending() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        rpc.era_probe(Era::Upgradeable);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.reverts(&v1::Perp::DeadlineExpired::SELECTOR);
        let err = client
            .tx(mock::PERP, calldata(PerpV022::liquidateTakerCall::SELECTOR))
            .with_gas_limit(GasLimits::LIQUIDATE)
            .send()
            .await
            .unwrap_err();
        assert!(
            matches!(&err, PerpCityError::Transaction(TransactionError::SimulationReverted {
                selector, ..
            }) if selector.0 == v1::Perp::DeadlineExpired::SELECTOR),
            "{err}"
        );
        let preflight = rpc.params("eth_call").pop().unwrap();
        assert_eq!(preflight[1], "pending");
        assert_eq!(preflight.as_array().unwrap().len(), 2);
        assert_eq!(rpc.methods(), ["eth_call", "eth_call", "eth_call"]);
        assert_eq!(rpc.call_gas(), [None, Some(GasLimits::LIQUIDATE), None]);
        assert!(rpc.is_drained());
    }

    /// A liquidation built for a market and sent through the public
    /// [`PerpClient::tx`] with no era, as a caller liquidating across
    /// markets sends [`MarketReader::liquidation_calldata`](super::super::MarketReader::liquidation_calldata):
    /// the market's era is read, so a `v1.0.0` liquidation, taker or
    /// maker, is preflighted at the head's hash in the next second, and a
    /// `v0.2.2` one, under the same selector, at `pending`.
    #[tokio::test(start_paused = true)]
    async fn a_liquidation_through_tx_reads_the_market_era() {
        for era in [Era::V1, Era::Upgradeable] {
            for selector in [
                v1::Perp::liquidateTakerCall::SELECTOR,
                v1::Perp::liquidateMakerCall::SELECTOR,
            ] {
                let (client, rpc) = mock::client();
                client.chain().set_base_fee(100);
                rpc.era_probe(era);
                let head = (era == Era::V1).then(|| rpc.block(500, 1_000));
                rpc.response(&Bytes::new());
                answer_send(&rpc, &client);
                client
                    .tx(mock::PERP, calldata(selector))
                    .with_gas_limit(GasLimits::LIQUIDATE)
                    .send()
                    .await
                    .unwrap();
                let preflight = rpc.params("eth_call").pop().unwrap();
                assert_eq!(rpc.call_gas().last(), Some(&Some(GasLimits::LIQUIDATE)));
                match head {
                    Some(hash) => {
                        assert_eq!(preflight[1], block_pin(hash), "{era:?}");
                        assert_eq!(preflight.as_array().unwrap().len(), 4);
                        assert_eq!(
                            overrides(&preflight),
                            &serde_json::json!({"number": "0x1f5", "time": "0x3e9"})
                        );
                    }
                    None => {
                        assert_eq!(preflight[1], "pending", "{era:?}");
                        assert_eq!(preflight.as_array().unwrap().len(), 2);
                        assert!(rpc.params("eth_getBlockByNumber").is_empty());
                    }
                }
                assert_eq!(rpc.sent_txs().len(), 1);
                assert!(rpc.is_drained());
            }
        }
    }

    /// A liquidation whose market era cannot be read sends nothing: the
    /// read's failure with no answer from the node is transient.
    #[tokio::test]
    async fn a_liquidation_without_its_era_sends_nothing() {
        for selector in [
            v1::Perp::liquidateMakerCall::SELECTOR,
            v1::Perp::touchCall::SELECTOR,
        ] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            rpc.fails("request timed out");
            let err = client
                .tx(mock::PERP, calldata(selector))
                .with_gas_limit(GasLimits::LIQUIDATE)
                .send()
                .await
                .unwrap_err();
            assert!(err.is_transient(), "{err}");
            assert_eq!(rpc.methods(), ["eth_call"]);
            assert!(rpc.sent_txs().is_empty());
            assert!(rpc.is_drained());
        }
    }

    /// A client bound to a market of one era liquidates on a market of the
    /// other era through [`PerpClient::tx`], with an explicit limit and
    /// without one: the era read goes to the target market, and the
    /// simulation is the target's. On `v1.0.0` it pins the head's hash
    /// with the next block's overrides (four params); on `v0.2.2` it runs
    /// at the endpoint's block with no head read.
    #[tokio::test(start_paused = true)]
    async fn a_liquidation_on_another_market_reads_that_markets_era() {
        let target = Address::repeat_byte(0x77);
        for (own, other) in [(Era::Upgradeable, Era::V1), (Era::V1, Era::Upgradeable)] {
            for explicit in [true, false] {
                let case = format!("own {own:?}, target {other:?}, explicit {explicit}");
                let (client, rpc) = mock::client();
                client.chain().set_base_fee(100);
                rpc.immutables(own);
                assert_eq!(client.market().era().await.unwrap(), own);
                let own_reads = rpc.call_routes().len();
                rpc.era_probe(other);
                let head = (other == Era::V1).then(|| rpc.block(500, 1_000));
                let method = if explicit {
                    rpc.response(&Bytes::new());
                    "eth_call"
                } else {
                    rpc.quantity(400_000);
                    "eth_estimateGas"
                };
                answer_send(&rpc, &client);
                let tx = client.tx(target, calldata(v1::Perp::liquidateMakerCall::SELECTOR));
                let tx = if explicit {
                    tx.with_gas_limit(GasLimits::LIQUIDATE)
                } else {
                    tx
                };
                tx.send().await.unwrap();

                let era_reads = &rpc.call_routes()[own_reads..own_reads + 1];
                assert!(
                    era_reads.iter().all(|(_, to)| *to == Some(target)),
                    "{case}: {era_reads:?}"
                );
                let simulation = rpc.params(method).pop().unwrap();
                assert_eq!(
                    simulation[0]["to"],
                    target.to_string().to_lowercase(),
                    "{case}"
                );
                match head {
                    Some(hash) => {
                        assert_eq!(simulation[1], block_pin(hash), "{case}");
                        assert_eq!(simulation.as_array().unwrap().len(), 4, "{case}");
                        assert_eq!(
                            overrides(&simulation),
                            &serde_json::json!({"number": "0x1f5", "time": "0x3e9"}),
                            "{case}"
                        );
                    }
                    None => {
                        if explicit {
                            assert_eq!(simulation[1], "pending", "{case}");
                        }
                        assert!(simulation.as_array().unwrap().len() < 4, "{case}");
                        assert!(rpc.params("eth_getBlockByNumber").is_empty(), "{case}");
                    }
                }
                assert_eq!(rpc.sent_txs()[0].to(), Some(target), "{case}");
                assert!(rpc.is_drained(), "{case}");
            }
        }
    }

    /// `touch()` accrues the market, and build `58b42b7` shares its
    /// selector: sent through [`PerpClient::tx`] it reads the market's era,
    /// so a `v1.0.0` touch is estimated in the next block and an earlier
    /// build's at the endpoint's block.
    #[tokio::test(start_paused = true)]
    async fn a_touch_through_tx_reads_the_market_era() {
        for era in [Era::V1, Era::Upgradeable] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            rpc.era_probe(era);
            let head = (era == Era::V1).then(|| rpc.block(500, 1_000));
            rpc.quantity(60_000);
            answer_send(&rpc, &client);
            client
                .tx(mock::PERP, v1::Perp::touchCall {}.abi_encode().into())
                .send()
                .await
                .unwrap();
            let estimate = rpc.params("eth_estimateGas").pop().unwrap();
            match head {
                Some(hash) => {
                    assert_eq!(estimate[1], block_pin(hash));
                    assert_eq!(
                        overrides(&estimate),
                        &serde_json::json!({"number": "0x1f5", "time": "0x3e9"})
                    );
                }
                None => {
                    assert!(estimate.as_array().unwrap().len() < 4, "{era:?}");
                    assert!(rpc.params("eth_getBlockByNumber").is_empty());
                }
            }
            assert_eq!(rpc.calls().len(), 1, "{era:?}: the era read");
            assert_eq!(rpc.sent_txs().len(), 1);
            assert!(rpc.is_drained(), "{era:?}");
        }
    }

    /// A typed liquidation whose market era read gets no answer is the
    /// transient [`PerpCityError::Rpc`], as through [`PerpClient::tx`], and
    /// sends nothing.
    #[tokio::test]
    async fn a_typed_liquidation_without_its_era_is_transient() {
        for role in [PositionRole::Taker, PositionRole::Maker] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            rpc.fails("request timed out");
            let recipient = Address::repeat_byte(0x51);
            let err = match role {
                PositionRole::Taker => {
                    client
                        .liquidate_taker(U256::from(7), recipient, Urgency::Normal)
                        .await
                }
                PositionRole::Maker => {
                    client
                        .liquidate_maker(U256::from(7), recipient, Urgency::Normal)
                        .await
                }
            }
            .unwrap_err();
            assert!(matches!(err, PerpCityError::Rpc(_)), "{err:?}");
            assert!(err.is_transient());
            assert_eq!(rpc.methods(), ["eth_call"]);
            assert!(rpc.sent_txs().is_empty());
            assert!(rpc.is_drained());
        }
    }

    /// A revert decoded from the answer's data is the call's answer before
    /// the node's words are read: `Error("out of gas")` under code 3 is a
    /// final [`TransactionError::SimulationReverted`], not gas running out,
    /// on a cache miss's estimate and on an explicit limit's check without
    /// the cap.
    #[tokio::test(start_paused = true)]
    async fn a_decoded_out_of_gas_revert_is_final() {
        let data = format!(
            r#""{}""#,
            alloy::hex::encode_prefixed(alloy::sol_types::Revert::from("out of gas").abi_encode())
        );
        for (path, code, message) in [
            (Path::Estimate, 3, "out of gas"),
            (Path::Explicit, 3, "out of gas"),
            (Path::Estimate, -32000, "execution reverted: out of gas"),
            (Path::Explicit, -32000, "execution reverted: out of gas"),
        ] {
            let (client, rpc) = v1_trade_on(path);
            rpc.block(500, 1_000);
            if let Path::Explicit = path {
                rpc.reverts(&TRANSFER_FROM_FAILED);
            }
            rpc.answers_error(code, message, Some(&data));
            let err = send_on(path, &client).await.unwrap_err();
            assert!(
                matches!(&err, PerpCityError::Transaction(TransactionError::SimulationReverted {
                    selector, ..
                }) if selector.0 == alloy::sol_types::Revert::SELECTOR),
                "{path:?}: {err}"
            );
            assert!(!err.is_transient());
            assert!(rpc.sent_txs().is_empty());
            assert!(rpc.is_drained(), "{path:?}");
        }
    }

    /// A capped call out of gas whose call without the cap fails without
    /// revert data is answered with the uncapped failure, not the capped
    /// one's words, on an explicit limit's check and a cached limit's
    /// estimate.
    #[tokio::test(start_paused = true)]
    async fn after_a_capped_out_of_gas_the_uncapped_failure_is_the_answer() {
        for path in [Path::Explicit, Path::CacheHit] {
            let (client, rpc) = v1_trade_on(path);
            rpc.block(500, 1_000);
            rpc.node_error(-32000, "out of gas");
            rpc.node_error(3, "execution reverted: nope");
            let err = send_on(path, &client).await.unwrap_err();
            assert!(
                matches!(
                    &err,
                    PerpCityError::Transaction(TransactionError::SimulationFailed { reason })
                        if reason.contains("nope") && !reason.contains("out of gas")
                ),
                "{path:?}: {err}"
            );
            assert!(!err.is_transient());
            assert!(rpc.is_drained(), "{path:?}");
        }
    }

    /// An explicit limit whose capped preflight runs out of gas is run
    /// once more without the cap in the same block. A pass is a limit too
    /// low. Otherwise the result of the uncapped simulation applies.
    #[tokio::test(start_paused = true)]
    async fn a_capped_out_of_gas_is_checked_without_the_cap() {
        let (client, rpc) = v1_trade_on(Path::Explicit);
        let head = rpc.block(500, 1_000);
        rpc.node_error(-32000, "out of gas");
        rpc.response(&Bytes::new());
        let err = send_on(Path::Explicit, &client).await.unwrap_err();
        assert!(
            matches!(
                &err,
                PerpCityError::Transaction(TransactionError::GasLimitTooLow {
                    gas_limit: 2_000_000,
                    error_name,
                }) if error_name == "out of gas"
            ),
            "{err}"
        );
        assert!(!err.is_transient());
        assert_eq!(rpc.call_gas(), [Some(2_000_000), None]);
        for call in rpc.params("eth_call") {
            assert_eq!(call[1], block_pin(head));
        }
        assert!(rpc.is_drained());

        for message in ["request timed out", "out of gas"] {
            let (client, rpc) = v1_trade_on(Path::Explicit);
            rpc.block(500, 1_000);
            rpc.node_error(-32000, "out of gas");
            rpc.node_error(-32000, message);
            rpc.block(501, 1_001);
            rpc.node_error(-32000, "out of gas");
            rpc.node_error(-32000, message);
            let err = send_on(Path::Explicit, &client).await.unwrap_err();
            assert!(err.is_transient(), "{message}: {err}");
            assert_eq!(
                rpc.call_gas(),
                [Some(2_000_000), None, Some(2_000_000), None]
            );
            assert!(rpc.sent_txs().is_empty());
            assert!(rpc.is_drained());
        }

        let (client, rpc) = v1_trade_on(Path::Explicit);
        rpc.block(500, 1_000);
        rpc.node_error(-32000, "out of gas");
        rpc.reverts(&v1::Perp::DeadlineExpired::SELECTOR);
        let err = send_on(Path::Explicit, &client).await.unwrap_err();
        assert!(
            matches!(&err, PerpCityError::Transaction(TransactionError::SimulationReverted {
                selector, ..
            }) if selector.0 == v1::Perp::DeadlineExpired::SELECTOR),
            "{err}"
        );
        assert!(rpc.is_drained());
    }

    /// Revert data that a node or a gateway nests under `originalError`
    /// is decoded as the contract's revert, not a failure without data.
    #[tokio::test(start_paused = true)]
    async fn a_revert_under_original_error_is_decoded() {
        let selector = alloy::hex::encode_prefixed(v1::Perp::DeadlineExpired::SELECTOR);
        let data = format!(
            r#"{{"originalError":{{"code":3,"message":"execution reverted","data":"{selector}"}}}}"#
        );
        let (client, rpc) = v1_trade_on(Path::Estimate);
        rpc.block(500, 1_000);
        rpc.answers_error(-32603, "execution reverted", Some(&data));
        let err = send_on(Path::Estimate, &client).await.unwrap_err();
        assert!(
            matches!(&err, PerpCityError::Transaction(TransactionError::SimulationReverted {
                selector, error_name, ..
            }) if selector.0 == v1::Perp::DeadlineExpired::SELECTOR
                && error_name == "DeadlineExpired"),
            "{err}"
        );
        assert!(rpc.sent_txs().is_empty());
        assert!(rpc.is_drained());
    }

    /// The capped call and the call without the cap run in one block. When
    /// either finds its block missing, the pair runs again in a head read
    /// again, once: a capped revert in block 500 with the call without the
    /// cap passing in block 501 is never GasLimitTooLow.
    #[tokio::test(start_paused = true)]
    async fn the_capped_pair_runs_in_one_block() {
        async fn send(client: &PerpClient) -> Result<TransactionReceipt> {
            client
                .tx(mock::PERP, calldata(v1::Perp::moveMakerCall::SELECTOR))
                .with_market_era(Era::V1)
                .with_gas_limit(400_000)
                .send()
                .await
        }
        let pins = |rpc: &mock::Rpc| {
            rpc.params("eth_call")
                .iter()
                .map(|call| (call[1].clone(), overrides(call)["time"].clone()))
                .collect::<Vec<_>>()
        };
        let at = |hash, time: &str| (block_pin(hash), Value::from(time));

        // A reorg at height 500 drops the old hash. The capped call passes at the new hash.
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let old = rpc.block(500, 1_000);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.node_error(-32000, MISSING_HASH);
        let new = rpc.block(500, 1_002);
        assert_ne!(old, new);
        rpc.response(&Bytes::new());
        answer_send(&rpc, &client);
        send(&client).await.unwrap();
        assert_eq!(rpc.call_gas(), [Some(400_000), None, Some(400_000)]);
        assert_eq!(
            pins(&rpc),
            [at(old, "0x3e9"), at(old, "0x3e9"), at(new, "0x3eb")]
        );
        assert!(rpc.is_drained());

        // Both calls run at the new hash before the SDK reports GasLimitTooLow.
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let old = rpc.block(500, 1_000);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.node_error(-32602, "too many arguments, want at most 3");
        let new = rpc.block(500, 1_002);
        assert_ne!(old, new);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.response(&Bytes::new());
        let err = send(&client).await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Transaction(TransactionError::GasLimitTooLow { .. })
            ),
            "{err}"
        );
        assert_eq!(
            pins(&rpc),
            [
                at(old, "0x3e9"),
                at(old, "0x3e9"),
                at(new, "0x3eb"),
                at(new, "0x3eb")
            ]
        );
        assert!(rpc.is_drained());

        // Only once: a miss in the second pair is transient.
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        rpc.block(500, 1_000);
        rpc.node_error(-32000, MISSING_HASH);
        rpc.block(501, 1_002);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.node_error(-32000, MISSING_HASH);
        let err = send(&client).await.unwrap_err();
        assert!(
            matches!(
                &err,
                PerpCityError::Transaction(TransactionError::GasUnavailable { .. })
            ),
            "{err}"
        );
        assert!(err.is_transient());
        assert_eq!(rpc.call_gas(), [Some(400_000), Some(400_000), None]);
        assert!(rpc.is_drained());
    }

    /// The explicit-limit path exists for operations (liquidations) where
    /// `eth_estimateGas` has passed while the real execution ran out of gas.
    /// The preflight can only catch that failure if the simulation is capped
    /// at the pinned limit — an uncapped `eth_call` runs under the node's
    /// default gas cap and succeeds where the pinned limit would OOG.
    #[test]
    fn preflight_request_pins_the_explicit_gas_limit() {
        let from = Address::repeat_byte(1);
        let to = Address::repeat_byte(2);
        let calldata = Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]);

        let pinned = preflight_request(from, to, &calldata, 7, Some(3_000_000));
        assert_eq!(pinned.gas, Some(3_000_000));
        assert_eq!(pinned.from, Some(from));
        assert_eq!(pinned.value, Some(U256::from(7u8)));

        let unpinned = preflight_request(from, to, &calldata, 0, None);
        assert_eq!(unpinned.gas, None, "no limit means the node default cap");
    }

    /// A receipt timeout names the transaction on both exits: the deadline
    /// passing with no receipt, and the deadline passing while the node
    /// errors. The wait starts polling at once, so it gives up exactly at
    /// the timeout: the send path's initial delay is not part of it.
    #[tokio::test(start_paused = true)]
    async fn receipt_timeout_carries_the_hash_on_both_exits() {
        let tx_hash = B256::repeat_byte(0x66);

        let asserter = Asserter::new();
        for _ in 0..32 {
            asserter.push_success(&Option::<TransactionReceipt>::None);
        }
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let start = tokio::time::Instant::now();
        let err = wait_for_receipt(&provider, tx_hash).await.unwrap_err();
        assert_eq!(start.elapsed(), RECEIPT_TIMEOUT);
        assert!(err.is_transient());
        let PerpCityError::Transaction(err) = err else {
            panic!("expected a transaction error, got {err}");
        };
        assert_eq!(err.tx_hash(), Some(tx_hash));
        assert!(
            matches!(&err, TransactionError::ReceiptTimeout { reason, .. } if reason.starts_with("no receipt")),
            "{err}"
        );

        // An empty asserter answers every poll with an RPC error.
        let provider = ProviderBuilder::new().connect_mocked_client(Asserter::new());
        let err = wait_for_receipt(&provider, tx_hash).await.unwrap_err();
        let PerpCityError::Transaction(err) = err else {
            panic!("expected a transaction error, got {err}");
        };
        assert_eq!(err.tx_hash(), Some(tx_hash));
        assert!(
            matches!(&err, TransactionError::ReceiptTimeout { reason, .. } if reason.starts_with("last receipt poll failed")),
            "{err}"
        );
    }

    /// A receipt of another hash than the one polled is a failed poll: the
    /// wait goes on to the hash's own receipt, and times out on receipts
    /// of other hashes alone.
    #[tokio::test(start_paused = true)]
    async fn a_receipt_of_another_hash_is_a_failed_poll() {
        use alloy::consensus::{Receipt, ReceiptEnvelope};

        let tx_hash = B256::repeat_byte(0x66);
        let receipt_of = |hash| TransactionReceipt {
            inner: ReceiptEnvelope::Eip1559(
                Receipt {
                    status: true.into(),
                    cumulative_gas_used: 21_000,
                    logs: Vec::<alloy::rpc::types::Log>::new(),
                }
                .with_bloom(),
            ),
            transaction_hash: hash,
            transaction_index: Some(0),
            block_hash: Some(B256::with_last_byte(1)),
            block_number: Some(92),
            gas_used: 21_000,
            effective_gas_price: 80,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::repeat_byte(1),
            to: Some(Address::repeat_byte(2)),
            contract_address: None,
        };
        let other = B256::repeat_byte(0x0e);

        let asserter = Asserter::new();
        asserter.push_success(&receipt_of(other));
        asserter.push_success(&receipt_of(tx_hash));
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let start = tokio::time::Instant::now();
        let receipt = wait_for_receipt(&provider, tx_hash).await.unwrap();
        assert_eq!(receipt.transaction_hash, tx_hash);
        assert_eq!(start.elapsed(), RECEIPT_POLL_INTERVAL);
        assert!(asserter.read_q().is_empty());

        let asserter = Asserter::new();
        for _ in 0..32 {
            asserter.push_success(&receipt_of(other));
        }
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let err = wait_for_receipt(&provider, tx_hash).await.unwrap_err();
        let PerpCityError::Transaction(err) = err else {
            panic!("expected a transaction error, got {err}");
        };
        assert_eq!(err.tx_hash(), Some(tx_hash));
        assert!(
            matches!(&err, TransactionError::ReceiptTimeout { reason, .. } if reason.contains(&other.to_string())),
            "{err}"
        );
    }

    /// The node's answers to eight `v1.0.0` reverts, each triggered on the
    /// deployed Sepolia bytecode on an Anvil fork and checked against
    /// `forge inspect` at the tag (`src/errors/fixtures/v1-reverts.json`,
    /// which records its fork block and each call; the script that made it
    /// is not in this repository). Each answer, sent through
    /// the simulation classifier as the node sent it, is a typed revert with
    /// the error's name, its binding's selector, and its class.
    #[test]
    fn v1_fork_reverts_are_typed_with_their_class() {
        let fixtures: Value =
            serde_json::from_str(include_str!("../errors/fixtures/v1-reverts.json")).unwrap();
        assert_eq!(fixtures["contracts_tag"], "v1.0.0");
        assert_eq!(fixtures["chain_id"], 421_614);
        let expected = [
            (
                v1::Perp::DeadlineExpired::SELECTOR,
                "DeadlineExpired",
                RevertClass::Transient,
            ),
            (
                v1::Perp::StaleBeaconIndex::SELECTOR,
                "StaleBeaconIndex",
                RevertClass::Transient,
            ),
            (
                v1::Perp::PriceOutsideRange::SELECTOR,
                "PriceOutsideRange",
                RevertClass::BeaconFault,
            ),
            (
                v1::Perp::BeaconIndexUnreadable::SELECTOR,
                "BeaconIndexUnreadable",
                RevertClass::BeaconFault,
            ),
            (
                v1::Perp::TvlCapExceeded::SELECTOR,
                "TvlCapExceeded",
                RevertClass::Hold,
            ),
            (
                v1::Perp::MaxTotalFeeExceeded::SELECTOR,
                "MaxTotalFeeExceeded",
                RevertClass::Answer,
            ),
            (
                v1::Perp::LongCloseCapacityExceeded::SELECTOR,
                "LongCloseCapacityExceeded",
                RevertClass::Answer,
            ),
            (
                v1::Perp::ShortCloseCapacityExceeded::SELECTOR,
                "ShortCloseCapacityExceeded",
                RevertClass::Answer,
            ),
        ];
        let reverts = fixtures["reverts"].as_array().unwrap();
        assert_eq!(reverts.len(), expected.len());
        for (selector, name, class) in expected {
            let fixture = reverts
                .iter()
                .find(|r| r["name"] == name)
                .unwrap_or_else(|| panic!("no {name} fixture"));
            assert_eq!(
                fixture["selector"],
                format!("0x{}", alloy::hex::encode(selector)),
                "{name}"
            );
            let answer = &fixture["eth_call_error"];
            let payload = alloy::rpc::json_rpc::ErrorPayload {
                code: answer["code"].as_i64().unwrap(),
                message: answer["message"].as_str().unwrap().to_owned().into(),
                data: Some(RawValue::from_string(answer["data"].to_string()).unwrap()),
            };
            let classified = classify_simulation_failure(
                &alloy::transports::TransportError::ErrorResp(payload),
                "eth_call",
            );
            let TransactionError::SimulationReverted {
                error_name,
                selector: typed,
                revert_data,
            } = &classified
            else {
                panic!("{name}: {classified}");
            };
            assert_eq!(error_name, name);
            assert_eq!(typed.0, selector, "{name}");
            assert_eq!(revert_data, &None, "{name} has no arguments");
            assert_eq!(classified.revert_class(), Some(class), "{name}");
            let err: PerpCityError = classified.into();
            assert_eq!(err.is_transient(), class.is_transient(), "{name}");
        }
    }

    #[tokio::test]
    async fn liquidation_probes_check_an_empty_revert_without_the_gas_cap() {
        for maker in [true, false] {
            for uncapped_passes in [true, false] {
                let (client, rpc) = mock::client();
                rpc.immutables(Era::Legacy);
                rpc.reverts(&[]);
                if uncapped_passes {
                    rpc.response(&Bytes::new());
                } else {
                    rpc.reverts(&EarlierPerp::NotLiquidatable::SELECTOR);
                }
                let from = client.address();
                let error = if maker {
                    client.simulate_liquidate_maker(U256::ONE, from).await
                } else {
                    client.simulate_liquidate_taker(U256::ONE, from).await
                }
                .unwrap_err();

                if uncapped_passes {
                    assert!(
                        matches!(&error,
                            PerpCityError::Transaction(TransactionError::GasLimitTooLow {
                                gas_limit: GasLimits::LIQUIDATE,
                                error_name,
                            }) if error_name == "execution reverted"),
                        "{error:?}"
                    );
                } else {
                    assert!(
                        matches!(&error, PerpCityError::Transaction(error)
                            if error.is_revert::<EarlierPerp::NotLiquidatable>()),
                        "{error:?}"
                    );
                }
                assert_eq!(rpc.call_gas()[2..], [Some(GasLimits::LIQUIDATE), None]);
                assert!(
                    rpc.params("eth_call")[2..]
                        .iter()
                        .all(|params| params[1] == "pending")
                );
                assert!(rpc.is_drained());
            }
        }
    }

    #[tokio::test]
    async fn liquidation_probes_keep_position_size_read_quotas_transient() {
        for era in [Era::Upgradeable, Era::V1] {
            for maker in [true, false] {
                for execution in [false, true] {
                    let (client, rpc) = mock::client();
                    rpc.immutables(era);
                    if execution {
                        rpc.reverts(&EarlierPerp::NotLiquidatable::SELECTOR);
                    } else {
                        rpc.node_error(-32005, "Too Many Requests");
                    }
                    let from = client.address();
                    let error = if maker {
                        client.simulate_liquidate_maker(U256::ONE, from).await
                    } else {
                        client.simulate_liquidate_taker(U256::ONE, from).await
                    }
                    .unwrap_err();

                    assert_eq!(
                        error.is_transient(),
                        !execution,
                        "{era:?} maker={maker}: {error:?}"
                    );
                    assert_eq!(rpc.call_gas(), [None; 3]);
                    assert!(rpc.is_drained());
                }
            }
        }
    }

    /// Arbitrum One's answers to a transfer from an empty account, as
    /// `eth_estimateGas` and `eth_call` give them.
    const INSUFFICIENT_FUNDS_ESTIMATE: &str = "failed with 50001848 gas: insufficient funds for gas * price + value: address 0x0000000000000000000000000000000000133723 have 0 want 1";
    const INSUFFICIENT_FUNDS_CALL: &str = "err: insufficient funds for gas * price + value: address 0x0000000000000000000000000000000000133723 have 0 want 1 (supplied gas 50001848)";
    /// Arbitrum One's answer to an `eth_call` capped at 20999 gas.
    const INTRINSIC_GAS_TOO_LOW: &str =
        "err: intrinsic gas too low: have 20999, want 21000 (supplied gas 20999)";

    /// An explicit limit under the call's intrinsic gas is a cap too
    /// small, as gas running out is: the call runs once more without the
    /// cap at the same block, and a pass is
    /// [`TransactionError::GasLimitTooLow`], no answer transient.
    #[tokio::test(start_paused = true)]
    async fn an_intrinsic_gas_refusal_at_an_explicit_limit_is_checked_without_the_cap() {
        async fn send(client: &PerpClient) -> Result<TransactionReceipt> {
            client
                .tx(mock::PERP, calldata(v1::Perp::adjustTakerCall::SELECTOR))
                .with_market_era(Era::V1)
                .with_gas_limit(20_999)
                .send()
                .await
        }
        let (client, rpc) = v1_trade_on(Path::Explicit);
        let head = rpc.block(500, 1_000);
        rpc.node_error(-32000, INTRINSIC_GAS_TOO_LOW);
        rpc.response(&Bytes::new());
        let err = send(&client).await.unwrap_err();
        assert!(
            matches!(
                &err,
                PerpCityError::Transaction(TransactionError::GasLimitTooLow {
                    gas_limit: 20_999,
                    error_name,
                }) if error_name == "intrinsic gas too low"
            ),
            "{err:?}"
        );
        assert!(!err.is_transient());
        assert_eq!(rpc.call_gas(), [Some(20_999), None]);
        for call in rpc.params("eth_call") {
            assert_eq!(call[1], block_pin(head));
        }
        assert!(rpc.sent_txs().is_empty());
        assert!(rpc.is_drained());

        let (client, rpc) = v1_trade_on(Path::Explicit);
        rpc.block(500, 1_000);
        rpc.node_error(-32000, INTRINSIC_GAS_TOO_LOW);
        rpc.node_error(-32000, "request timed out");
        rpc.block(501, 1_001);
        rpc.node_error(-32000, INTRINSIC_GAS_TOO_LOW);
        rpc.node_error(-32000, "request timed out");
        let err = send(&client).await.unwrap_err();
        assert!(
            matches!(
                &err,
                PerpCityError::Transaction(TransactionError::GasUnavailable { .. })
            ),
            "{err:?}"
        );
        assert!(err.is_transient());
        assert_eq!(rpc.call_gas(), [Some(20_999), None, Some(20_999), None]);
        assert!(rpc.sent_txs().is_empty());
        assert!(rpc.is_drained());
    }

    #[tokio::test(start_paused = true)]
    async fn busy_replies_are_transient_on_every_ordinary_path() {
        fn gas_unavailable(result: Result<()>, message: &str) {
            let error = result.unwrap_err();
            assert!(
                matches!(&error, PerpCityError::Transaction(TransactionError::GasUnavailable { reason })
                    if reason.contains(message)),
                "{message}: {error}"
            );
            assert!(error.is_transient());
        }

        for (code, message) in [
            (-32005, "Exceeded max limit of 100 requests per second"),
            (-32005, "daily request count exceeded"),
            (-32001, "Too many concurrent requests, try again later"),
            (-32602, "invalid argument"),
            (-32000, "context deadline exceeded"),
            (
                429,
                "Your app has exceeded its compute units per second capacity",
            ),
            (-32603, "Internal error"),
        ] {
            let selector = v1::Perp::adjustTakerCall::SELECTOR;
            for path in [Path::Estimate, Path::CacheHit, Path::Explicit] {
                let (client, rpc) = v1_trade_on(path);
                rpc.immutables(Era::Upgradeable);
                client.market().era().await.unwrap();
                rpc.node_error(code, message);
                let tx = client.tx(mock::PERP, calldata(selector));
                let (tx, method) = match path {
                    Path::Estimate => (tx, "eth_estimateGas"),
                    Path::CacheHit => (tx, "eth_call"),
                    Path::Explicit => (tx.with_gas_limit(2_000_000), "eth_call"),
                };
                gas_unavailable(tx.send().await.map(drop), message);
                assert!(rpc.params("eth_getBlockByNumber").is_empty(), "{path:?}");
                let params = rpc.params(method);
                assert_eq!(params.len(), if method == "eth_call" { 3 } else { 1 });
                assert_eq!(params.last().unwrap().as_array().unwrap().len(), 2);
                if let Path::CacheHit = path {
                    assert_eq!(cached_limit(&client, selector), Some(1_560_000));
                    assert!(rpc.params("eth_estimateGas").is_empty());
                }
                assert!(rpc.sent_txs().is_empty());
                assert!(rpc.is_drained());
            }

            for role in [PositionRole::Maker, PositionRole::Taker] {
                let (client, rpc) = mock::client();
                rpc.immutables(Era::Legacy);
                rpc.node_error(code, message);
                let market = client.market();
                let from = client.address();
                let result = match role {
                    PositionRole::Maker => {
                        market.simulate_liquidate_maker(from, U256::ONE, from).await
                    }
                    PositionRole::Taker => {
                        market.simulate_liquidate_taker(from, U256::ONE, from).await
                    }
                };
                gas_unavailable(result, message);
                assert_eq!(rpc.call_gas().last(), Some(&Some(GasLimits::LIQUIDATE)));
                assert!(rpc.is_drained());
            }

            let (client, rpc) = mock::client();
            rpc.quantity(500);
            let head = rpc.block(500, 1_000);
            rpc.node_error(code, message);
            let state = client.market().state_at_head().await.unwrap();
            gas_unavailable(
                state
                    .simulate_from(client.address(), &calldata(selector))
                    .await,
                message,
            );
            assert_eq!(rpc.params("eth_call")[0][1], block_pin(head));
            assert!(rpc.is_drained());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pinned_simulation_outcomes() {
        #[derive(Clone, Copy, Debug)]
        enum Outcome {
            Final,
            Retry,
            Decoded([u8; 4]),
        }
        use Outcome::{Decoded, Final, Retry};

        let unknown = (-32000, "some refusal no list names", None);
        let cases = [
            (
                (-32000, "Transaction reverted without a reason string", None),
                Final,
            ),
            (
                (
                    -32000,
                    "VM Exception while processing transaction: revert",
                    None,
                ),
                Final,
            ),
            (
                (-32000, "reverted with custom error 'Denied()'", None),
                Final,
            ),
            ((-32000, "reverted with panic code 0x11", None), Final),
            (
                (-32000, "invalid opcode: opcode 0xfe not defined", None),
                Final,
            ),
            ((-32000, "invalid jump destination", None), Final),
            ((-32000, "stack underflow (0 <=> 1)", None), Final),
            ((-32000, "write protection", None), Final),
            ((-32000, "return data out of bounds", None), Final),
            ((-32005, "rate limit exceeded", Some(r#""0x01""#)), Retry),
            (
                (-32005, "rate limit exceeded", Some(r#""0xdeadbeef""#)),
                Retry,
            ),
            ((-32000, "header not found", Some(r#""0xdeadbeef""#)), Retry),
            ((-32000, "stack limit reached 1024 (1023)", None), Final),
            ((-32000, "VM execution error.", None), Final),
            ((-32015, "VM execution error.", None), Final),
            (
                (
                    -32603,
                    "Internal error",
                    Some(
                        r#"{"originalError":{"code":3,"message":"execution reverted, data: \"0xbcffc83f\""}}"#,
                    ),
                ),
                Decoded([0xbc, 0xff, 0xc8, 0x3f]),
            ),
            (
                (
                    -32602,
                    "invalid argument 1: block overrides not allowed",
                    Some(
                        r#"[{"to":"0x0000000000000000000000000000000000000001","data":"0x12345678"},"0x8aa36943",{"number":"0x6","time":"0x9"}]"#,
                    ),
                ),
                Retry,
            ),
            (
                (
                    -32000,
                    "header not found",
                    Some(r#""0x8aa3694300000000000000000000000000000000000000000000000000000000""#),
                ),
                Retry,
            ),
            (
                (
                    -32602,
                    "invalid argument",
                    Some(r#""0x8aa3694300000000000000000000000000000000000000000000000000000000""#),
                ),
                Retry,
            ),
            (
                (
                    3,
                    "execution reverted",
                    Some(r#""0x8aa3694300000000000000000000000000000000000000000000000000000000""#),
                ),
                Decoded([0x8a, 0xa3, 0x69, 0x43]),
            ),
            ((3, "execution reverted", None), Final),
            (
                (3, "execution reverted, data: \"0xbcffc83f\"", None),
                Decoded([0xbc, 0xff, 0xc8, 0x3f]),
            ),
            ((3, "execution reverted: unknown block", None), Final),
            (
                (-32000, "execution reverted: header not found", None),
                Final,
            ),
            ((-32000, "execution reverted: out of gas", None), Final),
            (
                (
                    -32000,
                    "VM Exception while processing transaction: reverted with reason string 'out of gas'",
                    None,
                ),
                Final,
            ),
            (
                (
                    -32603,
                    "Internal error",
                    Some(r#"{"code":3,"message":"execution reverted","data":"0x7939f424"}"#),
                ),
                Decoded(TRANSFER_FROM_FAILED),
            ),
            (
                (
                    -32603,
                    "Internal error",
                    Some(
                        r#"{"originalError":{"code":3,"message":"execution reverted","data":"0x"}}"#,
                    ),
                ),
                Final,
            ),
            (
                (
                    -32603,
                    "Internal error",
                    Some(r#"{"originalError":[{"originalError":{"code":3}}]}"#),
                ),
                Final,
            ),
            (
                (
                    -32603,
                    "Internal error",
                    Some(r#"[{"message":"reverted with reason string 'denied'"}]"#),
                ),
                Final,
            ),
            ((-32602, "invalid argument", Some(r#""0x08c379a0""#)), Retry),
            (
                (
                    -32602,
                    "invalid argument",
                    Some(r#"{"originalError":{"data":"0xb2c86a29"}}"#),
                ),
                Retry,
            ),
            (
                (-32603, "Internal error", Some(r#""Reverted 0xdeadbeef""#)),
                Retry,
            ),
            (
                (
                    -32015,
                    "VM execution error.",
                    Some(r#""Reverted 0xdeadbeef""#),
                ),
                Final,
            ),
            (
                (3, "execution reverted", Some(r#""Reverted 0xdeadbeef""#)),
                Decoded([0xde, 0xad, 0xbe, 0xef]),
            ),
            (
                (
                    -32000,
                    "execution reverted",
                    Some(r#""Reverted 0xdeadbeef""#),
                ),
                Decoded([0xde, 0xad, 0xbe, 0xef]),
            ),
            ((3, "execution reverted", Some(r#""Reverted 0x12""#)), Final),
            (
                (-32000, "Reverted 0xdeadbeef", None),
                Decoded([0xde, 0xad, 0xbe, 0xef]),
            ),
            ((-32000, INSUFFICIENT_FUNDS_CALL, None), Final),
            ((-32000, INSUFFICIENT_FUNDS_ESTIMATE, None), Final),
            ((-32000, INTRINSIC_GAS_TOO_LOW, None), Final),
            ((-32000, "nonce too low", None), Final),
            ((-32000, "Nonce too high", None), Final),
            ((-32000, "already known", None), Final),
            (unknown, Retry),
            ((-32000, "out of gas", None), Retry),
            (
                (-32000, "gas required exceeds allowance (30000000)", None),
                Retry,
            ),
            ((-32000, "upstream request timeout", None), Retry),
            ((-32000, "upstream timeout", None), Retry),
            ((-32000, "request timed out", None), Retry),
            ((-32000, "header not found", None), Retry),
            ((-32000, "header for hash not found", None), Retry),
            (
                (
                    -32000,
                    "requested block ahead of current block and the hash is not currently canonical",
                    None,
                ),
                Retry,
            ),
            ((-32000, "unsupported block number 500", None), Retry),
            ((-32000, "unknown block", None), Retry),
            ((-32000, "missing trie node", None), Retry),
            ((-32004, "block overrides are not supported", None), Retry),
            ((-32005, "limit exceeded", None), Retry),
            ((-32005, "Too Many Requests", None), Retry),
            (
                (
                    -32005,
                    "Exceeded max limit of 100 requests per second",
                    None,
                ),
                Retry,
            ),
            (
                (-32005, "query returned more than 10000 results", None),
                Retry,
            ),
            ((-32603, "Internal error", None), Retry),
            ((-32602, "too many arguments, want at most 3", None), Retry),
            ((-32602, "invalid argument 0: unknown field", None), Retry),
            (
                (
                    -32602,
                    "invalid argument",
                    Some(r#"{"request":{"data":"0x12345678aabbccdd"}}"#),
                ),
                Retry,
            ),
            (
                (
                    -32602,
                    "invalid argument",
                    Some(
                        r#"[{"to":"0x0000000000000000000000000000000000000004","data":"0x12345678aabbccdd"}]"#,
                    ),
                ),
                Retry,
            ),
            (
                (
                    -32602,
                    "invalid argument",
                    Some(r#"{"originalError":[{"data":"0x12345678aabbccdd"}]}"#),
                ),
                Retry,
            ),
            (
                (
                    -32602,
                    "invalid argument",
                    Some(r#"{"originalError":{"data":[{"data":"0x12345678aabbccdd"}]}}"#),
                ),
                Retry,
            ),
            (
                (
                    -32602,
                    "invalid argument",
                    Some(
                        r#"{"params":[{"data":"0x12345678aabbccdd"}],"requestId":"0x0123456789"}"#,
                    ),
                ),
                Retry,
            ),
            (
                (
                    -32603,
                    "Internal error",
                    Some(r#"{"originalError":[{"to":"0x1","code":3,"data":"0xdeadbeef"}]}"#),
                ),
                Retry,
            ),
            ((503, "service unavailable", None), Retry),
        ];
        let revert_data_cases = [
            (r#""0x08c379a0""#, Some("0x08c379a0")),
            (
                r#"{"originalError":{"code":3,"data":"0xb2c86a2900"}}"#,
                Some("0xb2c86a2900"),
            ),
            (
                r#"{"data":"0x01","originalError":{"data":["0xdeadbeef"]}}"#,
                None,
            ),
            (r#"{"blockHash":"0xdeadbeefdeadbeef"}"#, None),
            (r#"{"requestId":"0xdeadbeefdeadbeef"}"#, None),
            (r#"{"request":{"data":"0x12345678aabbccdd"}}"#, None),
            (
                r#"{"originalError":{"params":[{"data":"0x12345678aabbccdd"}]}}"#,
                None,
            ),
            (r#"{"data":[{"data":"0x12345678"}]}"#, None),
            (
                r#"{"to":"0x0000000000000000000000000000000000000004","data":"0x12345678aabbccdd"}"#,
                None,
            ),
            (
                r#"{"message":"execution reverted","data":"0xbcffc83f"}"#,
                Some("0xbcffc83f"),
            ),
            (
                r#"{"code":3,"message":"execution reverted","data":"0xbcffc83f"}"#,
                Some("0xbcffc83f"),
            ),
            (r#"[{"data":"0x12345678"}]"#, None),
            (
                r#"[{"message":"execution reverted","data":"0x12345678"}]"#,
                Some("0x12345678"),
            ),
            (
                r#"[{"to":"0x0000000000000000000000000000000000000004","code":3,"data":"0x12345678"}]"#,
                None,
            ),
            (
                r#"{"originalError":[{"code":3,"data":"0xdeadbeef"}]}"#,
                Some("0xdeadbeef"),
            ),
            (
                r#"{"originalError":{"data":[{"code":3,"data":"0xdeadbeef"}]}}"#,
                Some("0xdeadbeef"),
            ),
            (r#"[{"request":{"data":"0x12345678"}}]"#, None),
            (
                r#"{"originalError":{"request":{"data":"0x12345678"}}}"#,
                None,
            ),
            (
                r#"{"originalError":{"originalError":{"data":"0xb2c86a29"}}}"#,
                None,
            ),
            (
                r#"{"originalError":{"code":-32005,"message":"rate limit exceeded","data":"0xdeadbeef"}}"#,
                None,
            ),
            (r#""0x1234""#, None),
            (r#""0xnothex00""#, None),
        ].map(|(data, expected)| {
            let outcome = expected.map_or(Final, |hex| Decoded(decode::parse_selector(hex).unwrap()));
            ((3, "execution reverted", Some(data)), outcome)
        });
        let busy_replies = [
            (
                429,
                "Your app has exceeded its compute units per second capacity",
            ),
            (-32_016, "Rate limit exceeded"),
            (-32_000, "Capacity Exceeded"),
            (-32_007, "Per-second request limit reached"),
            (-32_008, "Per-minute request limit reached"),
            (-32_011, "Method rate limit reached"),
            (-32_012, "Credits limited to 6000/sec"),
            (-32_005, "Exceeded project rate limit"),
            (-32_005, "Too Many Requests"),
            (-32_005, "Limit Exceeded"),
            (-32_000, "context deadline exceeded"),
            (-32_000, "Request timed out"),
            (-32_000, "read: connection reset by peer"),
            (-32_000, "No server available"),
            (-32_000, "The node is under too much load"),
            (500, "Internal Server Error"),
            (502, "Bad Gateway"),
            (503, "Service Unavailable"),
            (504, "Gateway Timeout"),
            (-32_603, "Internal error"),
        ]
        .map(|(code, message)| ((code, message, None), Retry));
        let other_replies = [
            (-32_005, "Query returned more than 10000 results"),
            (-32_005, "Block range limit exceeded"),
            (-32_005, "Log response size limit exceeded"),
            (-32_602, "Invalid params"),
            (-32_601, "Method unavailable"),
            (3, "execution reverted: Too Many Requests"),
            (-32_000, "execution reverted: gateway timeout"),
        ]
        .map(|(code, message)| {
            let outcome = if message.starts_with("execution reverted") {
                Final
            } else {
                Retry
            };
            ((code, message, None), outcome)
        });
        let runs = cases
            .into_iter()
            .chain(revert_data_cases)
            .chain(busy_replies)
            .chain(other_replies)
            .map(|(answer, outcome)| (answer, matches!(outcome, Retry).then_some(answer), outcome))
            .chain([
                (unknown, Some((3, "execution reverted", None)), Final),
                (
                    unknown,
                    Some((-32000, INSUFFICIENT_FUNDS_ESTIMATE, None)),
                    Final,
                ),
                (unknown, Some(unknown), Retry),
            ]);
        for (first, second, outcome) in runs {
            let (client, rpc) = v1_trade_on(Path::Estimate);
            let pins: Vec<_> = [Some(first), second]
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(index, (code, message, data))| {
                    let pin = rpc.block(500 + index as u64, 1_000 + index as u64);
                    rpc.answers_error(code, message, data);
                    pin
                })
                .collect();
            let error = send_on(Path::Estimate, &client).await.unwrap_err();
            let PerpCityError::Transaction(failure) = error else {
                panic!("{first:?}: {error:?}");
            };
            let cause = second.unwrap_or(first).1;
            match (outcome, failure) {
                (Retry, TransactionError::GasUnavailable { reason })
                | (Final, TransactionError::SimulationFailed { reason }) => {
                    assert!(reason.contains(cause), "{first:?}: {reason}");
                }
                (Decoded(expected), TransactionError::SimulationReverted { selector, .. }) => {
                    assert_eq!(selector.0, expected, "{first:?}");
                }
                (_, other) => panic!("{first:?}: expected {outcome:?}, got {other:?}"),
            }
            assert_eq!(
                rpc.params("eth_getBlockByNumber").len(),
                pins.len(),
                "{first:?}"
            );
            let estimates = rpc.params("eth_estimateGas");
            assert_eq!(estimates.len(), pins.len(), "{first:?}");
            for (estimate, pin) in estimates.iter().zip(pins) {
                assert_eq!(estimate[1], block_pin(pin), "{first:?}");
                assert_eq!(estimate.as_array().unwrap().len(), 4, "{first:?}");
            }
            assert!(rpc.sent_txs().is_empty());
            assert!(rpc.is_drained(), "{first:?}");
        }

        for status in [408, 429, 500, 502, 503, 504] {
            let error = TransportErrorKind::http_error(status, "gateway reply".into());
            let failure = classify_simulation_failure(&error, "eth_estimateGas");
            assert!(
                matches!(failure, TransactionError::GasUnavailable { reason }
                if reason.contains("gateway reply"))
            );
        }

        let (client, rpc) = v1_trade_on(Path::Estimate);
        for number in [500, 501] {
            rpc.block(number, 1_000);
            rpc.fails("request timed out");
        }
        let error = send_on(Path::Estimate, &client).await.unwrap_err();
        assert!(
            matches!(error, PerpCityError::Transaction(TransactionError::GasUnavailable { reason })
            if reason.contains("request timed out"))
        );
        assert_eq!(rpc.params("eth_getBlockByNumber").len(), 2);
        assert!(rpc.is_drained());
    }

    #[tokio::test(start_paused = true)]
    async fn raw_sends_pin_v1_and_unidentified_targets() {
        let (reader, active) = mock::client();
        active.immutables(Era::V1);
        assert_eq!(reader.market().era().await.unwrap(), Era::V1);

        for identity in [
            "cached v1",
            "fresh v1",
            "empty",
            "revert",
            "behind",
            "legacy",
        ] {
            let (sender, rpc) = mock::client();
            sender.chain().set_base_fee(100);
            let data = match identity {
                "cached v1" => {
                    rpc.immutables(Era::V1);
                    assert_eq!(sender.market().era().await.unwrap(), Era::V1);
                    v1::Perp::donateCall { amount: 1 }.abi_encode()
                }
                "fresh v1" => {
                    rpc.era_probe(Era::V1);
                    v1::Perp::touchCall {}.abi_encode()
                }
                _ => {
                    match identity {
                        "empty" => rpc.raw(&[]),
                        "revert" => rpc.node_error(3, "execution reverted"),
                        "behind" => rpc.node_error(-32000, "header not found"),
                        "legacy" => rpc.era_probe(Era::Legacy),
                        _ => unreachable!(),
                    }
                    v1::Perp::liquidateMakerCall {
                        posId: U256::from(17),
                        liquidationFeeRecipient: sender.address(),
                        liquidityAmount: 1,
                    }
                    .abi_encode()
                }
            };
            let hash = rpc.block(500, 1_000);
            rpc.response(&Bytes::new());
            answer_send(&rpc, &sender);
            sender
                .tx(mock::PERP, data.into())
                .with_gas_limit(900_000)
                .send()
                .await
                .unwrap();
            let call = rpc.params("eth_call").pop().unwrap();
            assert_eq!(call[1], block_pin(hash), "{identity}");
            assert_eq!(
                overrides(&call),
                &serde_json::json!({"number": "0x1f5", "time": "0x3e9"}),
                "{identity}"
            );
            assert_eq!(rpc.sent_txs().len(), 1);
            assert!(rpc.is_drained(), "{identity}");
        }

        // Round 9 also covered a positive v1 identity followed by a missing head.
        let (sender, rpc) = mock::client();
        rpc.era_probe(Era::V1);
        rpc.raw(&[]);
        let error = sender
            .tx(mock::PERP, calldata(v1::Perp::touchCall::SELECTOR))
            .with_gas_limit(900_000)
            .send()
            .await
            .unwrap_err();
        assert!(error.is_transient());
        assert!(rpc.sent_txs().is_empty());
        assert!(rpc.is_drained());
    }
}
