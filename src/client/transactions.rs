//! Transaction preparation, signing, broadcasting, and receipt polling.

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

impl PerpClient {
    /// Prepare, sign, send, and wait for a transaction receipt.
    ///
    /// If `gas_limit` is `None`, the gas limit is resolved from the
    /// estimate cache (keyed by 4-byte selector) or via `eth_estimateGas`.
    pub(crate) async fn send_tx(
        &self,
        to: Address,
        calldata: Bytes,
        gas_limit: Option<u64>,
        urgency: Urgency,
    ) -> Result<alloy::rpc::types::TransactionReceipt> {
        self.send_tx_with_value(to, calldata, 0, gas_limit, urgency)
            .await
    }

    /// Like `send_tx` but with an explicit ETH value to attach.
    pub(crate) async fn send_tx_with_value(
        &self,
        to: Address,
        calldata: Bytes,
        value: u128,
        gas_limit: Option<u64>,
        urgency: Urgency,
    ) -> Result<alloy::rpc::types::TransactionReceipt> {
        let now = super::now_ms();

        // Resolve gas limit: explicit override → cached estimate → eth_estimateGas
        let resolved_gas_limit = match gas_limit {
            Some(limit) => limit,
            None => self.resolve_gas_limit(to, &calldata, value, now).await?,
        };

        // Prepare via pipeline (zero RPC)
        let prepared = {
            let pipeline = self.pipeline.lock().unwrap();
            let fee_cache = self.fee_cache.lock().unwrap();
            pipeline.prepare(
                TxRequest {
                    to: to.into_array(),
                    calldata: calldata.to_vec(),
                    value,
                    gas_limit: resolved_gas_limit,
                    urgency,
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
            %to,
            ?urgency,
            "tx prepared"
        );

        // Build EIP-1559 transaction
        let tx = TransactionRequest::default()
            .with_to(to)
            .with_input(calldata)
            .with_value(U256::from(prepared.request.value))
            .with_nonce(prepared.nonce)
            .with_gas_limit(prepared.gas_limit)
            .with_max_fee_per_gas(prepared.gas_fees.max_fee_per_gas as u128)
            .with_max_priority_fee_per_gas(prepared.gas_fees.max_priority_fee_per_gas as u128)
            .with_chain_id(self.chain_id);

        // Sign and send
        let tx_envelope =
            tx.build(&self.wallet)
                .await
                .map_err(|e| TransactionError::SigningFailed {
                    reason: format!("{e}"),
                })?;

        let pending = self.provider.send_tx_envelope(tx_envelope).await?;
        let tx_hash_b256 = *pending.tx_hash();
        let tx_hash_bytes: [u8; 32] = tx_hash_b256.into();

        tracing::debug!(tx_hash = %tx_hash_b256, nonce = prepared.nonce, ?urgency, "tx broadcast");

        // Record in pipeline
        {
            let mut pipeline = self.pipeline.lock().unwrap();
            pipeline.record_submission(tx_hash_bytes, prepared, now);
        }

        // Wait for receipt via manual polling (avoids Alloy's background eth_blockNumber poller)
        let receipt = match self.poll_receipt(tx_hash_b256).await {
            Ok(receipt) => receipt,
            Err(e) => {
                // Evict the failed transaction so it doesn't permanently
                // consume an in-flight slot.
                let mut pipeline = self.pipeline.lock().unwrap();
                pipeline.fail(&tx_hash_bytes);
                return Err(e);
            }
        };

        // Confirm in pipeline
        {
            let mut pipeline = self.pipeline.lock().unwrap();
            pipeline.resolve(&tx_hash_bytes);
        }

        // Check if reverted
        if !receipt.status() {
            tracing::warn!(tx_hash = %tx_hash_b256, "tx reverted");
            let mut pipeline = self.pipeline.lock().unwrap();
            pipeline.fail(&tx_hash_bytes);
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

    /// Poll for a transaction receipt with intervals tuned for Base's ~2s block time.
    ///
    /// Uses direct `get_transaction_receipt` instead of Alloy's `pending.get_receipt()`
    /// to avoid triggering the background `eth_blockNumber` poller that persists for
    /// the provider's lifetime.
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

    /// Resolve gas limit from cache or via `eth_estimateGas`.
    ///
    /// Extracts the 4-byte function selector from calldata, checks the
    /// estimate cache, and falls back to an RPC call on cache miss.
    pub(crate) async fn resolve_gas_limit(
        &self,
        to: Address,
        calldata: &Bytes,
        value: u128,
        now: u64,
    ) -> Result<u64> {
        if calldata.len() < 4 {
            return Err(ValidationError::InvalidConfig {
                reason: "calldata too short to extract function selector".into(),
            }
            .into());
        }
        let selector: [u8; 4] = calldata[..4].try_into().unwrap();

        // Check cache
        {
            let cache = self.gas_limit_cache.lock().unwrap();
            if let Some(limit) = cache.get(&selector, now) {
                tracing::trace!(selector = %alloy::primitives::hex::encode(selector), limit, "gas estimate cache hit");
                return Ok(limit);
            }
        }

        // Cache miss — call eth_estimateGas
        let tx = TransactionRequest::default()
            .with_from(self.address)
            .with_to(to)
            .with_input(calldata.clone())
            .with_value(U256::from(value));

        let raw_estimate = self.provider.estimate_gas(tx).await.map_err(|e| {
            let error_str = e.to_string();
            if let Some((name, selector, data)) = decode::try_extract_revert(&error_str) {
                TransactionError::SimulationReverted {
                    error_name: name,
                    selector,
                    revert_data: data,
                }
            } else {
                TransactionError::GasUnavailable {
                    reason: format!("eth_estimateGas failed: {e}"),
                }
            }
        })?;

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
}
