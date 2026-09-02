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

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;

use crate::errors::{Result, TransactionError, ValidationError, decode};
use crate::hft::gas::Urgency;
use crate::hft::pipeline::TxRequest;

use super::PerpClient;

/// Default receipt polling timeout.
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Wait one block (~2s on Base) before first receipt poll.
const RECEIPT_POLL_INITIAL_DELAY: Duration = Duration::from_secs(2);

/// Poll for receipt every ~2s (Base block time).
const RECEIPT_POLL_INTERVAL: Duration = Duration::from_secs(2);

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
}

impl<'a> TxBuilder<'a> {
    /// Attach ETH value to the transaction.
    pub fn with_value(mut self, value: u128) -> Self {
        self.value = value;
        self
    }

    /// Set an explicit gas limit, skipping gas estimation.
    ///
    /// The transaction is still simulated via `eth_call` before broadcast,
    /// so a would-be revert surfaces as a decoded
    /// [`TransactionError::SimulationReverted`] instead of a mined failure.
    /// Use this when the estimate cannot be trusted (e.g. liquidations that
    /// have gone out-of-gas on Arbitrum where the estimate passed).
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
    pub async fn send(self) -> Result<alloy::rpc::types::TransactionReceipt> {
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
                .provider
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

        // Simulate + resolve gas limit. An explicit gas_limit skips only the
        // estimation: the preflight still runs so a would-be revert is
        // decoded before broadcast instead of burning the pinned gas.
        let resolved_gas_limit = match self.gas_limit {
            Some(0) => {
                return Err(ValidationError::InvalidConfig {
                    reason: "gas_limit must be > 0".into(),
                }
                .into());
            }
            Some(limit) => {
                // Preflight at the pinned limit itself: a call that runs out
                // of gas at this limit must fail here, not on-chain.
                self.client
                    .preflight_call(self.to, &self.calldata, self.value, Some(limit))
                    .await?;
                limit
            }
            None => {
                self.client
                    .simulate(self.to, &self.calldata, self.value, now)
                    .await?
            }
        };

        // Prepare via pipeline (zero RPC)
        let prepared = {
            let pipeline = self.client.pipeline.lock().unwrap();
            let fee_cache = self.client.fee_cache.lock().unwrap();
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
            .with_chain_id(self.client.chain_id);

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
        // chain once nothing is in flight.
        let pending = match self.client.provider.send_tx_envelope(tx_envelope).await {
            Ok(pending) => pending,
            Err(e) => {
                let pipeline = self.client.pipeline.lock().unwrap();
                pipeline.mark_desynced_prepared();
                return Err(e.into());
            }
        };
        let tx_hash_b256 = *pending.tx_hash();
        let tx_hash_bytes: [u8; 32] = tx_hash_b256.into();

        tracing::debug!(tx_hash = %tx_hash_b256, nonce = prepared.nonce, urgency = ?self.urgency, "tx broadcast");

        // Record in pipeline
        {
            let mut pipeline = self.client.pipeline.lock().unwrap();
            pipeline.record_submission(tx_hash_bytes, prepared, now);
        }

        // Wait for receipt. A timeout does NOT mean the transaction died —
        // it is broadcast and may still mine later, so its nonce must never
        // be rewound or reused (`fail` would). Stop tracking it and flag the
        // doubt; the next send resyncs from chain, which counts the
        // transaction if and only if it is still live.
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

        // Check if reverted
        if !receipt.status() {
            tracing::warn!(tx_hash = %tx_hash_b256, "tx reverted");
            return Err(TransactionError::Reverted {
                reason: format!("transaction {} reverted", tx_hash_b256),
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
fn preflight_request(
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
        }
    }

    /// Poll for a transaction receipt with intervals tuned for Base's ~2s block time.
    async fn poll_receipt(&self, tx_hash: B256) -> Result<alloy::rpc::types::TransactionReceipt> {
        tokio::time::sleep(RECEIPT_POLL_INITIAL_DELAY).await;
        let deadline = tokio::time::Instant::now() + RECEIPT_TIMEOUT;
        loop {
            match self.provider.get_transaction_receipt(tx_hash).await {
                Ok(Some(receipt)) => return Ok(receipt),
                Ok(None) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(TransactionError::ReceiptTimeout {
                            reason: format!("receipt timeout for {tx_hash}"),
                        }
                        .into());
                    }
                    tokio::time::sleep(RECEIPT_POLL_INTERVAL).await;
                }
                Err(e) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(TransactionError::ReceiptTimeout {
                            reason: format!("failed to get receipt: {e}"),
                        }
                        .into());
                    }
                    tracing::warn!(tx_hash = %tx_hash, error = %e, "receipt poll RPC error, retrying");
                    tokio::time::sleep(RECEIPT_POLL_INTERVAL).await;
                }
            }
        }
    }

    /// Run an `eth_call` simulation to verify a transaction won't revert.
    ///
    /// When `gas_limit` is set, the simulation is capped at exactly the
    /// limit the transaction will broadcast with, so an execution that
    /// cannot finish inside a pinned limit fails preflight instead of
    /// burning the gas on-chain — the one failure a fixed limit like
    /// [`GasLimits::LIQUIDATE`](crate::hft::gas::GasLimits::LIQUIDATE)
    /// exists to prevent. Without a limit the node simulates with its
    /// default gas cap.
    pub(super) async fn preflight_call(
        &self,
        to: Address,
        calldata: &Bytes,
        value: u128,
        gas_limit: Option<u64>,
    ) -> std::result::Result<(), TransactionError> {
        let tx = preflight_request(self.address, to, calldata, value, gas_limit);

        self.provider
            .call(tx)
            .await
            .map_err(|e| classify_simulation_failure(&e, "eth_call"))?;
        Ok(())
    }

    /// Simulate a transaction and return a gas limit.
    ///
    /// On cache miss: `eth_estimateGas` provides both the gas estimate and
    /// simulation (reverts are detected as a side effect).
    /// On cache hit: returns the cached gas limit after verifying the
    /// transaction via `preflight_call()`.
    ///
    /// Every code path guarantees the transaction has been simulated.
    async fn simulate(&self, to: Address, calldata: &Bytes, value: u128, now: u64) -> Result<u64> {
        // Transactions with fewer than 4 bytes of calldata carry no function
        // selector to key the estimate cache on — in practice plain value
        // transfers, which cannot revert from contract logic. Estimate their
        // gas directly rather than assuming a fixed 21_000: on Arbitrum the
        // intrinsic gas folds in an L1 data component, so a hardcoded 21_000 is
        // rejected as "intrinsic gas too low". Apply the same 20% buffer the
        // cache uses.
        if calldata.len() < 4 {
            let raw = self.estimate_gas(to, calldata, value).await?;
            return Ok(raw + raw / 5);
        }
        let selector: [u8; 4] = calldata[..4].try_into().unwrap();

        // Check cache — if hit, simulate via eth_call to verify still valid.
        // Drop the guard before the async preflight call.
        let cached_limit = {
            let cache = self.gas_limit_cache.lock().unwrap();
            cache.get(&selector, now)
        };
        if let Some(limit) = cached_limit {
            tracing::trace!(selector = %alloy::primitives::hex::encode(selector), limit, "gas estimate cache hit");
            // Cap the preflight at the cached limit the transaction will be
            // sent with, so a stale (too-small) cached estimate surfaces as
            // a failed preflight rather than an on-chain out-of-gas.
            match self.preflight_call(to, calldata, value, Some(limit)).await {
                Ok(()) => return Ok(limit),
                Err(revert @ TransactionError::SimulationReverted { .. }) => {
                    return Err(revert.into());
                }
                Err(error @ TransactionError::SimulationFailed { .. }) => {
                    // The node ran the call and it failed inside the cached
                    // limit without a contract revert: the estimate has gone
                    // stale (a bigger trade crossing more ticks, say). Evict
                    // it and fall through to a fresh estimate instead of
                    // failing every send for this selector until the TTL
                    // expires.
                    tracing::debug!(
                        selector = %alloy::primitives::hex::encode(selector),
                        limit,
                        %error,
                        "cached gas limit failed preflight; re-estimating"
                    );
                    self.gas_limit_cache.lock().unwrap().invalidate(&selector);
                }
                // The call never reached the node (a timeout, a rate limit):
                // nothing disproved the cached estimate, so keep it and let
                // the caller retry instead of paying for a re-estimate.
                Err(transient) => return Err(transient.into()),
            }
        }

        // Cache miss — call eth_estimateGas
        let raw_estimate = self.estimate_gas(to, calldata, value).await?;

        // Cache with buffer
        {
            let mut cache = self.gas_limit_cache.lock().unwrap();
            cache.put(selector, raw_estimate, now);
        }

        let buffered = {
            let cache = self.gas_limit_cache.lock().unwrap();
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

    /// Run `eth_estimateGas`, decoding contract reverts into structured errors.
    async fn estimate_gas(&self, to: Address, calldata: &Bytes, value: u128) -> Result<u64> {
        let tx = TransactionRequest::default()
            .with_from(self.address)
            .with_to(to)
            .with_input(calldata.clone())
            .with_value(U256::from(value));

        self.provider
            .estimate_gas(tx)
            .await
            .map_err(|e| classify_simulation_failure(&e, "eth_estimateGas").into())
    }
}

/// JSON-RPC error codes a node answers an execution failure with: `3`
/// (geth's `execution reverted`, data attached when the contract supplied
/// any) and `-32000` (the generic server error geth-lineage nodes use for
/// `out of gas` and data-less reverts). Rate limits and replica hiccups
/// use other codes (`-32005`, `-32029`, `429`, …) or are transport-level,
/// and stay transient.
fn is_execution_outcome(resp: &alloy::rpc::json_rpc::ErrorPayload) -> bool {
    matches!(resp.code, 3 | -32000)
}

/// Sort a failed `eth_call` / `eth_estimateGas` into the three things it
/// can mean: a decodable contract revert
/// ([`TransactionError::SimulationReverted`]), the node's definitive
/// execution failure without revert data
/// ([`TransactionError::SimulationFailed`], not transient), or a failure to
/// get an answer at all ([`TransactionError::GasUnavailable`], transient).
fn classify_simulation_failure(
    e: &alloy::transports::TransportError,
    what: &str,
) -> TransactionError {
    let error_str = e.to_string();
    if let Some((name, selector, data)) = decode::try_extract_revert(&error_str) {
        return TransactionError::SimulationReverted {
            error_name: name,
            selector: selector.into(),
            revert_data: data,
        };
    }
    if e.as_error_resp().is_some_and(is_execution_outcome) {
        TransactionError::SimulationFailed {
            reason: format!("{what} failed: {e}"),
        }
    } else {
        TransactionError::GasUnavailable {
            reason: format!("{what} failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn error_resp(code: i64, message: &'static str) -> alloy::transports::TransportError {
        alloy::transports::TransportError::ErrorResp(alloy::rpc::json_rpc::ErrorPayload {
            code,
            message: message.into(),
            data: None,
        })
    }

    /// One variant used to carry both "the node said the call fails" and
    /// "the node could not be reached", and `is_transient()` could not
    /// tell them apart — a liquidation bot retrying on it would loop on an
    /// empty revert forever. The three outcomes must stay distinct.
    #[test]
    fn simulation_failures_are_sorted_by_what_the_node_said() {
        use alloy::transports::TransportErrorKind;

        let decoded = classify_simulation_failure(
            &error_resp(3, "execution reverted, data: \"0xbcffc83f\""),
            "eth_call",
        );
        assert!(
            matches!(decoded, TransactionError::SimulationReverted { .. }),
            "{decoded}"
        );

        let empty_revert =
            classify_simulation_failure(&error_resp(3, "execution reverted"), "eth_call");
        assert!(
            matches!(empty_revert, TransactionError::SimulationFailed { .. }),
            "{empty_revert}"
        );
        let out_of_gas = classify_simulation_failure(&error_resp(-32000, "out of gas"), "eth_call");
        assert!(
            matches!(out_of_gas, TransactionError::SimulationFailed { .. }),
            "{out_of_gas}"
        );

        let rate_limited =
            classify_simulation_failure(&error_resp(-32005, "limit exceeded"), "eth_call");
        assert!(
            matches!(rate_limited, TransactionError::GasUnavailable { .. }),
            "{rate_limited}"
        );
        let timeout = classify_simulation_failure(
            &TransportErrorKind::custom_str("request timed out"),
            "eth_estimateGas",
        );
        assert!(
            matches!(timeout, TransactionError::GasUnavailable { .. }),
            "{timeout}"
        );

        let transient: crate::errors::PerpCityError = timeout.into();
        assert!(transient.is_transient());
        let deterministic: crate::errors::PerpCityError = empty_revert.into();
        assert!(!deterministic.is_transient());
    }
}
