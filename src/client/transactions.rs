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
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::transports::TransportError;

use crate::constants::{RECEIPT_POLL_INTERVAL, RECEIPT_TIMEOUT};
use crate::errors::{Result, TransactionError, ValidationError, decode};
use crate::hft::gas::{Urgency, is_out_of_gas, is_shared_liquidation, is_v1_trade};
use crate::hft::pipeline::TxRequest;
use crate::transport::fault::{is_execution_error, is_gas_exhaustion, revert_data};

use super::PerpClient;
use super::chain::{NextBlock, wants_new_head};
use super::queries::Era;
use super::v1_trades::era_read_error;

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
    /// Use the known market era for calls that share selectors across eras.
    pub(super) fn with_market_era(mut self, era: Era) -> Self {
        self.market_era = Some(era);
        self
    }

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
        // A liquidation's selectors are the same on `v0.2.2` and `v1.0.0`,
        // so calldata built for either is told apart by the market's era.
        let market_era = match self.market_era {
            None if selector.as_ref().is_some_and(is_shared_liquidation) => Some(
                self.client
                    .chain()
                    .market(self.to)
                    .era()
                    .await
                    .map_err(era_read_error)?,
            ),
            era => era,
        };
        let v1 = market_era.map_or_else(
            || selector.as_ref().is_some_and(is_v1_trade),
            |era| era == Era::V1,
        );
        let next = if v1 {
            Some(self.client.chain().next_block().await?)
        } else {
            None
        };

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
                        next.as_ref(),
                    )
                    .await?;
                limit
            }
            None => {
                self.client
                    .simulate(self.to, &self.calldata, self.value, now, next.as_ref())
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
    /// [`PerpCityError::tx_hash`](crate::PerpCityError::tx_hash). On timeout it returns
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

    /// Simulate a transaction and return a gas limit, in `next` when it is
    /// given (a `v1.0.0` trade's next block).
    ///
    /// On cache miss: `eth_estimateGas` provides both the gas estimate and
    /// simulation (reverts are detected as a side effect).
    /// On cache hit: returns the cached gas limit after verifying the
    /// transaction via `preflight_call()` capped at the limit without the
    /// cache's buffer. A capped call that fails, out of gas or with a
    /// revert, is estimated again without the cap: running short of gas
    /// inside a token transfer reverts as the transfer's own error, so only
    /// the uncapped answer tells a revert from a stale limit. After a
    /// capped revert the uncapped estimate's failure is sorted by
    /// [`classify_uncapped_failure`]: running out of gas or no execution
    /// answer is [`TransactionError::GasUnavailable`], and an execution
    /// error without revert data leaves the capped revert as the answer. When either RPC needs a new
    /// head, repeat the simulation from the capped call.
    ///
    /// Every code path guarantees the transaction has been simulated.
    async fn simulate(
        &self,
        to: Address,
        calldata: &Bytes,
        value: u128,
        now: u64,
        next: Option<&NextBlock>,
    ) -> Result<u64> {
        let selector: Option<[u8; 4]> = calldata.get(..4).map(|bytes| bytes.try_into().unwrap());
        let cached = selector.and_then(|selector| {
            let cache = self.gas_limit_cache.lock().unwrap();
            cache
                .get(&selector, now)
                .map(|limit| (limit, cache.preflight_limit(limit)))
        });
        let chain = self.chain();
        let simulate = |next| async move {
            let mut capped_failure = None;
            if let Some((limit, cap)) = cached {
                // Leave the buffer for the gas the simulation does not see.
                let tx = preflight_request(self.address, to, calldata, value, Some(cap));
                let error = match chain.call_in(tx, next).await {
                    Ok(()) => return Ok(Ok((limit, false))),
                    Err(error) if next.is_some() && wants_new_head(&error) => return Err(error),
                    Err(error) => classify_simulation_failure(&error, "eth_call"),
                };
                if matches!(error, TransactionError::GasUnavailable { .. }) {
                    return Ok(Err(error));
                }
                tracing::debug!(limit, %error, "cached gas limit failed preflight; re-estimating");
                capped_failure = Some(error);
            }

            let tx = preflight_request(self.address, to, calldata, value, None);
            let estimate = match chain.estimate_gas_in(tx, next).await {
                Err(error) if next.is_some() && wants_new_head(&error) => return Err(error),
                estimate => estimate,
            };
            if matches!(
                capped_failure,
                Some(TransactionError::SimulationFailed { .. })
            ) && let Some(selector) = selector
            {
                self.gas_limit_cache.lock().unwrap().invalidate(&selector);
            }
            let resolved = match (estimate, capped_failure) {
                (Ok(raw), _) => Ok((raw, true)),
                (Err(error), Some(revert @ TransactionError::SimulationReverted { .. })) => {
                    Err(classify_uncapped_failure(&error, "eth_estimateGas", revert))
                }
                (Err(error), _) => Err(classify_simulation_failure(&error, "eth_estimateGas")),
            };
            Ok(resolved)
        };

        let (limit, estimated) = match next {
            Some(next) => {
                chain
                    .in_next_block("gas simulation", next, |next| simulate(Some(next)))
                    .await?
            }
            None => simulate(None).await,
        }
        .map_err(|e| classify_simulation_failure(&e, "gas simulation"))??;

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

/// The failure of a call without the cap run after the same call `capped`
/// failed at a fixed limit. The call running out of gas without the cap
/// ([`is_gas_exhaustion`]) leaves the limit untested, so it is
/// [`TransactionError::GasUnavailable`] with its cause; the node's
/// execution failure without revert data leaves `capped` as the answer;
/// any other failure is sorted as [`classify_simulation_failure`] sorts it.
pub(super) fn classify_uncapped_failure(
    e: &TransportError,
    what: &str,
    capped: TransactionError,
) -> TransactionError {
    if e.as_error_resp().is_some_and(is_gas_exhaustion) {
        return TransactionError::GasUnavailable {
            reason: format!("{what} without the cap ran out of gas: {e}"),
        };
    }
    match classify_simulation_failure(e, what) {
        TransactionError::SimulationFailed { .. } => capped,
        error => error,
    }
}

/// Sort a failed `eth_call` / `eth_estimateGas` into the three things it
/// can mean: a decodable contract revert
/// ([`TransactionError::SimulationReverted`]), the node's definitive
/// execution failure without revert data
/// ([`TransactionError::SimulationFailed`], not transient), or a failure to
/// get an answer at all ([`TransactionError::GasUnavailable`], transient).
/// The revert data is read from the error answer's structured `data`
/// ([`revert_data`], nested under `originalError` too) first, then from the
/// error's text.
pub(super) fn classify_simulation_failure(e: &TransportError, what: &str) -> TransactionError {
    let structured = e.as_error_resp().and_then(revert_data).and_then(|data| {
        let (name, selector) = decode::decode_revert(&data)?;
        Some((name, selector, (data.len() > 10).then_some(data)))
    });
    if let Some((name, selector, data)) =
        structured.or_else(|| decode::try_extract_revert(&e.to_string()))
    {
        return TransactionError::SimulationReverted {
            error_name: name,
            selector: selector.into(),
            revert_data: data,
        };
    }
    if e.as_error_resp().is_some_and(is_execution_error) {
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
    use alloy::consensus::{Receipt, ReceiptEnvelope, Transaction as _};
    use alloy::primitives::{I256, aliases::I24};
    use alloy::providers::ProviderBuilder;
    use alloy::sol_types::{SolCall, SolError};
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
        let selector = EarlierPerp::openMakerCall::SELECTOR;
        let (client, rpc) = cached(selector, 500_000);
        assert_eq!(cached_limit(&client, selector), Some(GasLimits::OPEN_MAKER));
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.quantity(700_000);
        answer_send(&rpc, &client);
        let receipt = client
            .tx(mock::PERP, calldata(selector))
            .send()
            .await
            .unwrap();
        assert!(receipt.status());
        assert_eq!(
            rpc.methods(),
            [
                "eth_call",
                "eth_estimateGas",
                "eth_sendRawTransaction",
                "eth_getTransactionReceipt"
            ]
        );
        assert_eq!(rpc.call_gas(), [Some(666_667)]);
        assert!(rpc.params("eth_estimateGas")[0][0].get("gas").is_none());
        let sent = rpc.sent_txs();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].gas_limit(), 840_000);
        assert_eq!(cached_limit(&client, selector), Some(840_000));
        assert!(rpc.is_drained());

        let (client, rpc) = cached(selector, 500_000);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        let err = client
            .tx(mock::PERP, calldata(selector))
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
        assert_eq!(cached_limit(&client, selector), Some(GasLimits::OPEN_MAKER));
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

    /// The head read for a v1 trade finds no block: no simulation runs at
    /// the endpoint's own block instead, and nothing is sent.
    #[tokio::test]
    async fn no_head_block_is_no_answer() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        rpc.no_block();
        let err = client
            .tx(mock::PERP, calldata(v1::Perp::openTakerCall::SELECTOR))
            .send()
            .await
            .unwrap_err();
        assert!(
            matches!(
                &err,
                PerpCityError::Transaction(TransactionError::GasUnavailable { reason })
                    if reason.contains("no head block")
            ),
            "{err}"
        );
        assert!(err.is_transient());
        assert_eq!(rpc.methods(), ["eth_getBlockByNumber"]);
        assert!(rpc.is_drained());
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
        let (client, rpc) = cached(selector, 500_000);
        rpc.node_error(-32000, "out of gas");
        rpc.fails("request timed out");
        let err = client
            .tx(mock::PERP, calldata(selector))
            .send()
            .await
            .unwrap_err();
        assert!(err.is_transient(), "{err}");
        assert_eq!(cached_limit(&client, selector), None);
        assert!(rpc.is_drained());
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

    /// Error answers to a call without the cap that are no execution
    /// answer, so a capped revert before them cannot stand.
    const NO_EXECUTION_ANSWER: [(i64, &str); 4] = [
        (-32000, "upstream request timeout"),
        (-32005, "limit exceeded"),
        (-32603, "internal error"),
        (-32000, "request timed out"),
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
        let tx = client.tx(mock::PERP, calldata(v1::Perp::adjustTakerCall::SELECTOR));
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
        for revert in [true, false] {
            let (client, rpc) = cached(selector, 1_300_000);
            rpc.block(500, 1_000);
            if revert {
                rpc.reverts(&TRANSFER_FROM_FAILED);
            } else {
                rpc.node_error(-32000, "out of gas");
            }
            rpc.quantity(1_700_000);
            answer_send(&rpc, &client);
            client
                .tx(mock::PERP, calldata(selector))
                .send()
                .await
                .unwrap();
            let estimate = &rpc.params("eth_estimateGas")[0];
            assert_eq!(estimate[1], block_pin(mock::block(500, 1_000).header.hash));
            assert_eq!(
                overrides(estimate),
                &serde_json::json!({"number": "0x1f5", "time": "0x3e9"})
            );
            assert_eq!(rpc.sent_txs()[0].gas_limit(), 2_040_000);
            assert_eq!(cached_limit(&client, selector), Some(2_040_000));
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

    #[tokio::test]
    async fn missing_block_words_in_a_revert_do_not_request_another_head() {
        let (client, rpc) = v1_trade_on(Path::Estimate);
        rpc.block(500, 1_000);
        rpc.node_error(3, "execution reverted: unknown block");
        let err = send_on(Path::Estimate, &client).await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Transaction(TransactionError::SimulationFailed { .. })
            ),
            "{err}"
        );
        assert_eq!(rpc.methods(), ["eth_getBlockByNumber", "eth_estimateGas"]);
        assert!(rpc.is_drained());
    }

    #[tokio::test]
    async fn an_estimate_gateway_timeout_is_transient() {
        let (client, rpc) = v1_trade_on(Path::Estimate);
        rpc.block(500, 1_000);
        rpc.node_error(-32000, "upstream timeout");
        let err = send_on(Path::Estimate, &client).await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Transaction(TransactionError::GasUnavailable { .. })
            ),
            "{err}"
        );
        assert!(err.is_transient());
        assert_eq!(rpc.methods(), ["eth_getBlockByNumber", "eth_estimateGas"]);
        assert!(rpc.is_drained());
    }

    /// After a capped revert, an uncapped estimate the node answers with
    /// an execution error but no revert data leaves the capped call's
    /// decoded revert as the answer; one that gets no execution answer
    /// (a timeout, an upstream error under `-32000`, a rate limit, a bare
    /// internal error) is transient, on a v1 trade and an earlier build's.
    #[tokio::test(start_paused = true)]
    async fn a_capped_revert_stands_when_the_estimate_answers_without_data() {
        for (selector, v1) in [
            (v1::Perp::adjustTakerCall::SELECTOR, true),
            (EarlierPerp::openMakerCall::SELECTOR, false),
        ] {
            for (code, message) in NO_EXECUTION_ANSWER {
                let (client, rpc) = cached(selector, 1_300_000);
                if v1 {
                    rpc.block(500, 1_000);
                }
                rpc.reverts(&TRANSFER_FROM_FAILED);
                rpc.node_error(code, message);
                let err = client
                    .tx(mock::PERP, calldata(selector))
                    .send()
                    .await
                    .unwrap_err();
                assert!(
                    matches!(
                        &err,
                        PerpCityError::Transaction(TransactionError::GasUnavailable { .. })
                    ),
                    "{code} {message}: {err}"
                );
                assert!(err.is_transient());
                assert!(rpc.sent_txs().is_empty());
                assert!(rpc.is_drained());
            }
        }
        let selector = v1::Perp::adjustTakerCall::SELECTOR;
        let (client, rpc) = cached(selector, 1_300_000);
        rpc.block(500, 1_000);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.node_error(-32603, "execution reverted");
        let err = client
            .tx(mock::PERP, calldata(selector))
            .send()
            .await
            .unwrap_err();
        assert!(
            matches!(&err, PerpCityError::Transaction(TransactionError::SimulationReverted {
                selector, ..
            }) if selector.0 == TRANSFER_FROM_FAILED),
            "{err}"
        );
        assert_eq!(cached_limit(&client, selector), Some(1_560_000));
        assert!(rpc.is_drained());

        rpc.block(500, 1_000);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.fails("request timed out");
        let err = client
            .tx(mock::PERP, calldata(selector))
            .send()
            .await
            .unwrap_err();
        assert!(err.is_transient(), "{err}");
        assert!(rpc.sent_txs().is_empty());
        assert!(rpc.is_drained());
    }

    /// An explicit limit whose preflight reverts is run once more without
    /// the cap, in the same next block: a success there is a limit too
    /// low, typed as such, and a revert there leaves the capped revert.
    #[tokio::test(start_paused = true)]
    async fn an_explicit_limit_that_reverts_is_checked_without_the_cap() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        rpc.block(500, 1_000);
        rpc.reverts(&TRANSFER_FROM_FAILED);
        rpc.response(&Bytes::new());
        let err = client
            .tx(mock::PERP, calldata(v1::Perp::moveMakerCall::SELECTOR))
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

        for (code, message) in [(3, "execution reverted"), (-32603, "execution reverted")] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            rpc.block(500, 1_000);
            rpc.reverts(&TRANSFER_FROM_FAILED);
            rpc.node_error(code, message);
            let err = client
                .tx(mock::PERP, calldata(v1::Perp::moveMakerCall::SELECTOR))
                .with_gas_limit(400_000)
                .send()
                .await
                .unwrap_err();
            assert!(
                matches!(&err, PerpCityError::Transaction(TransactionError::SimulationReverted {
                    selector, ..
                }) if selector.0 == TRANSFER_FROM_FAILED),
                "{message}: {err}"
            );
            assert_eq!(rpc.call_gas(), [Some(400_000), None]);
            assert!(rpc.is_drained());
        }
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

    /// After a capped revert of an explicit limit, a call without the cap
    /// that gets no execution answer leaves the limit untested: transient,
    /// not the capped revert, and nothing sent.
    #[tokio::test(start_paused = true)]
    async fn an_unanswered_uncapped_check_is_transient() {
        for (code, message) in NO_EXECUTION_ANSWER {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            rpc.block(500, 1_000);
            rpc.reverts(&TRANSFER_FROM_FAILED);
            rpc.node_error(code, message);
            let err = client
                .tx(mock::PERP, calldata(v1::Perp::moveMakerCall::SELECTOR))
                .with_gas_limit(400_000)
                .send()
                .await
                .unwrap_err();
            assert!(
                matches!(
                    &err,
                    PerpCityError::Transaction(TransactionError::GasUnavailable { reason })
                        if reason.contains(message)
                ),
                "{message}: {err}"
            );
            assert!(err.is_transient());
            assert!(rpc.sent_txs().is_empty());
            assert!(rpc.is_drained());
        }
    }

    /// An earlier build's explicit limit (a liquidation) is preflighted at
    /// the endpoint's `pending` block, with no one block to run a check
    /// without the cap in: its capped revert stands, and nothing else is
    /// asked, so a call that passes in a later block is never typed
    /// GasLimitTooLow. A `v0.2.2` liquidation, whose selector `v1.0.0`
    /// shares, reads the market's era first; build `58b42b7`'s does not.
    #[tokio::test]
    async fn a_legacy_capped_revert_at_pending_stands_unchecked() {
        for (selector, era) in [
            (EarlierPerp::liquidateTakerCall::SELECTOR, None),
            (
                PerpV022::liquidateTakerCall::SELECTOR,
                Some(Era::Upgradeable),
            ),
        ] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            if let Some(era) = era {
                rpc.immutables(era);
            }
            rpc.reverts(&TRANSFER_FROM_FAILED);
            let err = client
                .tx(mock::PERP, calldata(selector))
                .with_gas_limit(GasLimits::LIQUIDATE)
                .send()
                .await
                .unwrap_err();
            assert!(
                matches!(&err, PerpCityError::Transaction(TransactionError::SimulationReverted {
                    selector, ..
                }) if selector.0 == TRANSFER_FROM_FAILED),
                "{err}"
            );
            let preflight = rpc.params("eth_call").pop().unwrap();
            assert_eq!(preflight[1], "pending");
            assert_eq!(preflight.as_array().unwrap().len(), 2);
            let reads = if era.is_some() { 2 } else { 0 };
            assert_eq!(rpc.methods().len(), reads + 1);
            assert_eq!(rpc.call_gas().last(), Some(&Some(GasLimits::LIQUIDATE)));
            assert!(rpc.is_drained());
        }
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
                rpc.immutables(era);
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
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        rpc.fails("request timed out");
        let err = client
            .tx(mock::PERP, calldata(v1::Perp::liquidateMakerCall::SELECTOR))
            .with_gas_limit(GasLimits::LIQUIDATE)
            .send()
            .await
            .unwrap_err();
        assert!(err.is_transient(), "{err}");
        assert_eq!(rpc.methods(), ["eth_call"]);
        assert!(rpc.sent_txs().is_empty());
        assert!(rpc.is_drained());
    }

    /// After a capped revert, the call without the cap running out of gas
    /// leaves the limit untested: transient, with its cause, not the
    /// capped revert, on an explicit limit's check and a cached limit's
    /// estimate.
    #[tokio::test(start_paused = true)]
    async fn an_uncapped_check_out_of_gas_is_transient() {
        for path in [Path::Explicit, Path::CacheHit] {
            for message in ["out of gas", "gas required exceeds allowance (32000000)"] {
                let (client, rpc) = v1_trade_on(path);
                rpc.block(500, 1_000);
                rpc.reverts(&TRANSFER_FROM_FAILED);
                rpc.node_error(-32000, message);
                let err = send_on(path, &client).await.unwrap_err();
                assert!(
                    matches!(
                        &err,
                        PerpCityError::Transaction(TransactionError::GasUnavailable { reason })
                            if reason.contains("ran out of gas") && reason.contains(message)
                    ),
                    "{path:?} {message}: {err}"
                );
                assert!(err.is_transient());
                assert!(rpc.sent_txs().is_empty());
                assert!(rpc.is_drained());
            }
        }
    }

    /// An explicit limit whose capped preflight runs out of gas is run
    /// once more without the cap in the same block: a pass is a limit too
    /// low, no answer or gas running out again is transient, and a decoded
    /// revert is the call's answer.
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

        for (code, message) in [(-32000, "request timed out"), (-32000, "out of gas")] {
            let (client, rpc) = v1_trade_on(Path::Explicit);
            rpc.block(500, 1_000);
            rpc.node_error(-32000, "out of gas");
            rpc.node_error(code, message);
            let err = send_on(Path::Explicit, &client).await.unwrap_err();
            assert!(
                matches!(
                    &err,
                    PerpCityError::Transaction(TransactionError::GasUnavailable { .. })
                ),
                "{message}: {err}"
            );
            assert!(err.is_transient());
            assert_eq!(rpc.call_gas(), [Some(2_000_000), None]);
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

    fn error_resp(code: i64, message: &'static str) -> alloy::transports::TransportError {
        alloy::transports::TransportError::ErrorResp(alloy::rpc::json_rpc::ErrorPayload {
            code,
            message: message.into(),
            data: None,
        })
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

        let transient: PerpCityError = timeout.into();
        assert!(transient.is_transient());
        let deterministic: PerpCityError = empty_revert.into();
        assert!(!deterministic.is_transient());
    }
}
