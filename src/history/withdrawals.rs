//! Controller history for the new contract version. Legacy event decoding is unchanged.

use std::collections::BTreeMap;

use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use serde::{Deserialize, Serialize};

use crate::contracts::IERC20;
use crate::contracts::withdrawal_events::WithdrawalEvents as Events;
use crate::errors::{ContractError, PerpCityError, Result, TransactionError, ValidationError};
use crate::events::decode_raw;
use crate::math::BlockContext;
use crate::units::UsdcAtoms;

use super::scan::check_block_range;
use super::transfers::decode_transfer;
use super::{History, TokenTransfer};

/// One current-build controller event, with canonical chain identity at read time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithdrawalEvent {
    /// The controller that emitted the event.
    pub controller: Address,
    /// Canonical block context, including the event's timestamp.
    pub block: BlockContext,
    /// Transaction that emitted the event.
    pub tx_hash: B256,
    /// Position in the block, not in the transaction.
    pub log_index: u64,
    /// Exact event contents.
    pub kind: WithdrawalEventKind,
}

/// Controller event types for the new version. Amounts are USDC atoms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WithdrawalEventKind {
    /// A payout entered escrow.
    ClaimCreated {
        /// The affected claim.
        claim_id: U256,
        /// The payout recipient.
        recipient: Address,
        /// The payout in USDC atoms.
        amount: UsdcAtoms,
    },
    /// A payout consumed the instant bucket.
    InstantPayout {
        /// The payout recipient.
        recipient: Address,
        /// The payout in USDC atoms.
        amount: UsdcAtoms,
        /// Remaining instant capacity after payout.
        available_after: UsdcAtoms,
    },
    /// A pending claim was paid out.
    ClaimReleased {
        /// The affected claim.
        claim_id: U256,
        /// The payout recipient.
        recipient: Address,
        /// The payout in USDC atoms.
        amount: UsdcAtoms,
    },
    /// The removed amount was paid to the named guardian.
    ClaimReduced {
        /// The affected claim.
        claim_id: U256,
        /// Claim amount immediately before the reduction.
        old_amount: UsdcAtoms,
        /// Claim amount immediately after the reduction.
        new_amount: UsdcAtoms,
        /// Hash of the off-chain evidence supplied by the guardian.
        evidence_hash: B256,
        /// Owner that received the removed funds.
        guardian: Address,
    },
    /// The guardian paused one claim.
    ClaimPaused {
        /// The affected claim.
        claim_id: U256,
    },
    /// The governor unpaused one claim.
    ClaimUnpaused {
        /// The affected claim.
        claim_id: U256,
    },
    /// All releases were paused.
    ReleasesPaused,
    /// Releases were unpaused.
    ReleasesUnpaused,
    /// The instant-payout bucket changed.
    RateLimitSet {
        /// Maximum instant-payout burst.
        capacity: UsdcAtoms,
        /// USDC atoms restored to the bucket each second.
        refill_per_second: UsdcAtoms,
    },
    /// The live delay applied to pending claims changed.
    ChallengeWindowSet {
        /// New delay in seconds.
        seconds: u64,
    },
    /// The controller was permanently bound to a market.
    ControllerInitialized {
        /// The permanently bound market.
        perp: Address,
        /// Maximum instant-payout burst.
        capacity: UsdcAtoms,
        /// USDC atoms restored to the bucket each second.
        refill_per_second: UsdcAtoms,
        /// Initial claim delay in seconds.
        challenge_window: u64,
    },
}

/// A positive claim reduction from a successful receipt on the canonical chain.
/// The receipt has enough confirmations for this history handle. It does not
/// prove repayment or current ownership.
#[derive(Debug, Clone, Serialize)]
pub struct ReductionEvidence {
    event: WithdrawalEvent,
    claim_id: U256,
    guardian: Address,
    recovered: UsdcAtoms,
}

impl ReductionEvidence {
    /// The exact source log and its canonical block.
    pub fn event(&self) -> &WithdrawalEvent {
        &self.event
    }
    /// The claim reduced by the source log.
    pub fn claim_id(&self) -> U256 {
        self.claim_id
    }
    /// The owner that received the reduction, as recorded by the controller.
    pub fn guardian(&self) -> Address {
        self.guardian
    }
    /// The old claim amount minus the new amount.
    pub fn recovered(&self) -> UsdcAtoms {
        self.recovered
    }
}

/// One exact repayment transfer in a successful canonical receipt.
/// It proves this payment's contents, not that another payment never occurred.
#[derive(Debug, Clone, Serialize)]
pub struct RepaymentEvidence {
    transfer: TokenTransfer,
    block: BlockContext,
    token: Address,
}

impl RepaymentEvidence {
    /// Exact transfer identity, sender, recipient and amount.
    pub fn transfer(&self) -> &TokenTransfer {
        &self.transfer
    }
    /// The canonical block containing the repayment.
    pub fn block(&self) -> BlockContext {
        self.block
    }
    /// The USDC contract that emitted the transfer.
    pub fn token(&self) -> Address {
        self.token
    }
}

impl<P: Provider> History<P> {
    /// Read controller events for the new version in chain order.
    ///
    /// Check the end header before and after scanning. Each log must identify its
    /// canonical block and transaction. Duplicate or malformed logs fail the scan.
    /// Replace the rolling window on each sweep to discard events removed by a
    /// reorg. The caller must check the controller's code and market binding
    /// separately. This reader does not verify that an address implements the ABI.
    ///
    /// # Errors
    ///
    /// Return an error for an invalid range or address, malformed or duplicate logs,
    /// a changed block hash, or a failed header read or log scan.
    pub async fn withdrawal_events(
        &self,
        controller: Address,
        from_block: u64,
        to_block: Option<u64>,
    ) -> Result<Vec<WithdrawalEvent>> {
        nonzero(controller)?;
        let to = self.resolve(to_block).await?;
        check_block_range(from_block, to)?;
        let anchor = self.block(to).await?;
        if anchor.number != to {
            return Err(malformed("controller history anchor names another height"));
        }
        let filter = Filter::new().address(controller).event_signature(topics());
        let logs = self.logs(&filter, from_block, Some(to)).await?;
        let mut blocks = BTreeMap::from([(to, anchor)]);
        let mut rows = Vec::with_capacity(logs.len());
        let mut previous = None;
        for log in logs {
            let number = log
                .block_number
                .ok_or_else(|| malformed("missing block number"))?;
            if !(from_block..=to).contains(&number) {
                return Err(malformed("event outside requested block range"));
            }
            let block = match blocks.get(&number) {
                Some(block) => *block,
                None => {
                    let block = self.block(number).await?;
                    blocks.insert(number, block);
                    block
                }
            };
            let event = decode(&log, controller, block)?;
            let point = (event.block.number, event.log_index);
            if previous.is_some_and(|last| point <= last) {
                return Err(malformed("duplicate or out-of-order controller event"));
            }
            previous = Some(point);
            rows.push(event);
        }
        if self.block(to).await? != anchor {
            return Err(ContractError::ChainReorganized { number: to }.into());
        }
        Ok(rows)
    }

    /// Verify a supplied USDC repayment through a pinned lagged block.
    /// The caller verifies the reduction controller's permanent market binding.
    /// `None` means the receipt is absent or newer than `at`, never proof of nonpayment.
    ///
    /// # Errors
    /// Reject an unconfirmed or changed endpoint, stale reduction evidence, failed or
    /// foreign receipt, malformed transfer, wrong amount or ambiguous matching logs.
    pub async fn confirmed_repayment(
        &self,
        tx_hash: B256,
        token: Address,
        market: Address,
        reduction: &ReductionEvidence,
        at: BlockContext,
    ) -> Result<Option<RepaymentEvidence>> {
        nonzero(token)?;
        nonzero(market)?;
        let confirmed_through = self.tip().await?;
        if at.number > confirmed_through {
            return Err(ContractError::BlockUnconfirmed {
                number: at.number,
                confirmed_through,
            }
            .into());
        }
        if self.block(at.number).await? != at {
            return Err(ContractError::ChainReorganized { number: at.number }.into());
        }
        let source = reduction.event().block;
        if source.number > at.number {
            return Err(malformed(
                "reduction evidence is outside the canonical endpoint",
            ));
        }
        if self.block(source.number).await? != source {
            return Err(ContractError::ChainReorganized {
                number: source.number,
            }
            .into());
        }
        let Some(receipt) = self.provider.get_transaction_receipt(tx_hash).await? else {
            return Ok(None);
        };
        if receipt.transaction_hash != tx_hash {
            return Err(malformed("repayment receipt names another transaction"));
        }
        if !receipt.status() {
            return Err(TransactionError::Reverted {
                tx_hash,
                reason: "repayment receipt reverted".into(),
            }
            .into());
        }
        let number = receipt
            .block_number
            .ok_or_else(|| malformed("repayment receipt has no block"))?;
        if number > at.number {
            return Ok(None);
        }
        if number < source.number {
            return Err(malformed("repayment precedes the reduction"));
        }
        let block = self.block(number).await?;
        if block.number != number || receipt.block_hash.is_none() {
            return Err(malformed("repayment receipt has invalid block identity"));
        }
        if receipt.block_hash != Some(block.hash) {
            return Err(ContractError::ChainReorganized { number }.into());
        }
        let mut matching = Vec::new();
        for log in receipt.inner.logs() {
            if log.address() != token || log.topic0() != Some(&IERC20::Transfer::SIGNATURE_HASH) {
                continue;
            }
            if log.removed
                || log.block_number != Some(number)
                || log.block_hash != Some(block.hash)
                || log.transaction_hash != Some(tx_hash)
                || log
                    .block_timestamp
                    .is_some_and(|timestamp| timestamp != block.timestamp)
            {
                return Err(malformed("repayment log is not canonical"));
            }
            let transfer = decode_transfer(log)?;
            if transfer.from == reduction.guardian()
                && transfer.to == market
                && transfer.value == U256::from(reduction.recovered().atoms())
            {
                if (number, transfer.log_index) <= (source.number, reduction.event().log_index) {
                    return Err(malformed("repayment transfer precedes the reduction"));
                }
                matching.push(transfer);
            }
        }
        let [transfer] = matching.as_slice() else {
            return Err(malformed(
                "repayment receipt needs exactly one matching transfer",
            ));
        };
        for expected in [block, source, at] {
            if self.block(expected.number).await? != expected {
                return Err(ContractError::ChainReorganized {
                    number: expected.number,
                }
                .into());
            }
        }
        Ok(Some(RepaymentEvidence {
            transfer: *transfer,
            block,
            token,
        }))
    }

    /// Identify one reduction in a successful transaction on the canonical chain.
    ///
    /// `None` means the receipt is absent or lacks the confirmations this handle
    /// requires. It does not mean the transaction failed. The caller must verify
    /// which market the controller belongs to and who received the reduction.
    /// The caller must also check whether repayment has already occurred.
    /// The result uses the amount recorded in the receipt.
    ///
    /// # Errors
    ///
    /// Return an error if the receipt reverted, its identifiers differ from the request,
    /// the reduction is malformed, the chain reorganized, or an RPC call failed.
    pub async fn confirmed_reduction(
        &self,
        tx_hash: B256,
        controller: Address,
        claim_id: U256,
        log_index: u64,
    ) -> Result<Option<ReductionEvidence>> {
        nonzero(controller)?;
        let Some(receipt) = self.provider.get_transaction_receipt(tx_hash).await? else {
            return Ok(None);
        };
        if receipt.transaction_hash != tx_hash {
            return Err(malformed("reduction receipt names another transaction"));
        }
        if !receipt.status() {
            return Err(TransactionError::Reverted {
                tx_hash,
                reason: "reduction receipt reverted".into(),
            }
            .into());
        }
        let number = receipt
            .block_number
            .ok_or_else(|| malformed("receipt has no block"))?;
        if number > self.tip().await? {
            return Ok(None);
        }
        let block = self.block(number).await?;
        if block.number != number || receipt.block_hash.is_none() {
            return Err(malformed("reduction receipt has invalid block identity"));
        }
        if receipt.block_hash != Some(block.hash) {
            return Err(ContractError::ChainReorganized { number }.into());
        }
        let mut matching = receipt
            .inner
            .logs()
            .iter()
            .filter(|log| log.log_index == Some(log_index));
        let log = matching
            .next()
            .ok_or_else(|| ContractError::EventNotFound {
                event_name: format!("ClaimReduced at log {log_index}"),
            })?;
        if matching.next().is_some() {
            return Err(malformed("duplicate reduction receipt log index"));
        }
        if log.transaction_hash != Some(tx_hash) {
            return Err(malformed("reduction log names another transaction"));
        }
        let event = decode(log, controller, block)?;
        let WithdrawalEventKind::ClaimReduced {
            claim_id: actual_id,
            old_amount,
            new_amount,
            guardian,
            ..
        } = &event.kind
        else {
            return Err(malformed("receipt log is not ClaimReduced"));
        };
        if *actual_id != claim_id {
            return Err(malformed("reduction log names another claim"));
        }
        let recovered = old_amount
            .atoms()
            .checked_sub(new_amount.atoms())
            .filter(|amount| *amount != 0)
            .ok_or_else(|| malformed("claim reduction has no positive delta"))?;
        let guardian = *guardian;
        if guardian.is_zero() {
            return Err(malformed("claim reduction has zero guardian"));
        }
        if self.block(number).await? != block {
            return Err(ContractError::ChainReorganized { number }.into());
        }
        Ok(Some(ReductionEvidence {
            event,
            claim_id,
            guardian,
            recovered: UsdcAtoms::new(recovered),
        }))
    }
}

fn nonzero(controller: Address) -> Result<()> {
    if controller.is_zero() {
        return Err(ValidationError::InvalidConfig {
            reason: "controller address is zero".into(),
        }
        .into());
    }
    Ok(())
}

fn malformed(context: &str) -> PerpCityError {
    ValidationError::DecodeFailed {
        context: context.into(),
    }
    .into()
}

fn topics() -> Vec<B256> {
    vec![
        Events::ClaimCreated::SIGNATURE_HASH,
        Events::InstantPayout::SIGNATURE_HASH,
        Events::ClaimReleased::SIGNATURE_HASH,
        Events::ClaimReduced::SIGNATURE_HASH,
        Events::ClaimPaused::SIGNATURE_HASH,
        Events::ClaimUnpaused::SIGNATURE_HASH,
        Events::ReleasesPaused::SIGNATURE_HASH,
        Events::ReleasesUnpaused::SIGNATURE_HASH,
        Events::RateLimitSet::SIGNATURE_HASH,
        Events::ChallengeWindowSet::SIGNATURE_HASH,
        Events::ControllerInitialized::SIGNATURE_HASH,
    ]
}

fn decode(log: &Log, controller: Address, block: BlockContext) -> Result<WithdrawalEvent> {
    if log.address() != controller
        || log.removed
        || log.block_hash != Some(block.hash)
        || log.block_number != Some(block.number)
        || log
            .block_timestamp
            .is_some_and(|timestamp| timestamp != block.timestamp)
    {
        return Err(malformed(
            "controller event identity differs from canonical block",
        ));
    }
    let tx_hash = log
        .transaction_hash
        .ok_or_else(|| malformed("missing transaction hash"))?;
    let log_index = log
        .log_index
        .ok_or_else(|| malformed("missing log index"))?;
    let topic = log
        .topic0()
        .ok_or_else(|| malformed("missing controller event topic"))?;
    let kind = if *topic == Events::ClaimCreated::SIGNATURE_HASH {
        let e = decode_raw::<Events::ClaimCreated>(log)?;
        WithdrawalEventKind::ClaimCreated {
            claim_id: e.claimId,
            recipient: e.recipient,
            amount: UsdcAtoms::new(e.amount),
        }
    } else if *topic == Events::InstantPayout::SIGNATURE_HASH {
        let e = decode_raw::<Events::InstantPayout>(log)?;
        WithdrawalEventKind::InstantPayout {
            recipient: e.recipient,
            amount: UsdcAtoms::new(e.amount),
            available_after: UsdcAtoms::new(e.availableAfter),
        }
    } else if *topic == Events::ClaimReleased::SIGNATURE_HASH {
        let e = decode_raw::<Events::ClaimReleased>(log)?;
        WithdrawalEventKind::ClaimReleased {
            claim_id: e.claimId,
            recipient: e.recipient,
            amount: UsdcAtoms::new(e.amount),
        }
    } else if *topic == Events::ClaimReduced::SIGNATURE_HASH {
        let e = decode_raw::<Events::ClaimReduced>(log)?;
        if e.newAmount >= e.oldAmount {
            return Err(malformed("non-decreasing ClaimReduced"));
        }
        WithdrawalEventKind::ClaimReduced {
            claim_id: e.claimId,
            old_amount: UsdcAtoms::new(e.oldAmount),
            new_amount: UsdcAtoms::new(e.newAmount),
            evidence_hash: e.evidenceHash,
            guardian: e.guardian,
        }
    } else if *topic == Events::ClaimPaused::SIGNATURE_HASH {
        let e = decode_raw::<Events::ClaimPaused>(log)?;
        WithdrawalEventKind::ClaimPaused {
            claim_id: e.claimId,
        }
    } else if *topic == Events::ClaimUnpaused::SIGNATURE_HASH {
        let e = decode_raw::<Events::ClaimUnpaused>(log)?;
        WithdrawalEventKind::ClaimUnpaused {
            claim_id: e.claimId,
        }
    } else if *topic == Events::ReleasesPaused::SIGNATURE_HASH {
        decode_raw::<Events::ReleasesPaused>(log)?;
        WithdrawalEventKind::ReleasesPaused
    } else if *topic == Events::ReleasesUnpaused::SIGNATURE_HASH {
        decode_raw::<Events::ReleasesUnpaused>(log)?;
        WithdrawalEventKind::ReleasesUnpaused
    } else if *topic == Events::RateLimitSet::SIGNATURE_HASH {
        let e = decode_raw::<Events::RateLimitSet>(log)?;
        WithdrawalEventKind::RateLimitSet {
            capacity: UsdcAtoms::new(e.capacity),
            refill_per_second: UsdcAtoms::new(e.refillPerSecond),
        }
    } else if *topic == Events::ChallengeWindowSet::SIGNATURE_HASH {
        let e = decode_raw::<Events::ChallengeWindowSet>(log)?;
        WithdrawalEventKind::ChallengeWindowSet {
            seconds: e.challengeWindow.to::<u64>(),
        }
    } else if *topic == Events::ControllerInitialized::SIGNATURE_HASH {
        let e = decode_raw::<Events::ControllerInitialized>(log)?;
        WithdrawalEventKind::ControllerInitialized {
            perp: e.perp,
            capacity: UsdcAtoms::new(e.capacity),
            refill_per_second: UsdcAtoms::new(e.refillPerSecond),
            challenge_window: e.challengeWindow.to::<u64>(),
        }
    } else {
        return Err(malformed("unknown controller event topic"));
    };
    Ok(WithdrawalEvent {
        controller,
        block,
        tx_hash,
        log_index,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use alloy::consensus::{Receipt, ReceiptEnvelope};
    use alloy::primitives::{Bytes, Log as PrimitiveLog, LogData, b256};
    use alloy::providers::ProviderBuilder;
    use alloy::rpc::types::{Block, TransactionReceipt};
    use alloy::sol_types::SolValue;
    use alloy::transports::mock::Asserter;

    use super::*;
    use crate::history::test_support::{FakeNode, hash_of, timestamp_of};

    const CONTROLLER: Address = Address::repeat_byte(0x11);
    const GUARDIAN: Address = Address::repeat_byte(0x22);
    const TX: B256 = B256::repeat_byte(0x33);
    // Independent `cast keccak` of the source ABI at 5141c2f, not the decoder's constants.
    const CREATED: B256 = b256!("77dd07aa5019a7b0e605b437e1f8ad5a0deadf8dda64522419ab0de18e50c75f");
    const REDUCED: B256 = b256!("0dfde72f7060517680f306c25ca3828565eb80ca0d47b68d2aabb6dc4e40e750");
    const INSTANT: B256 = b256!("d9a1d5251230456f9abd0e2a5019c4306119467d275cca3412a717b2658b8f0e");

    fn log(topics: Vec<B256>, data: Vec<u8>, number: u64, index: u64) -> Log {
        Log {
            inner: PrimitiveLog {
                address: CONTROLLER,
                data: LogData::new_unchecked(topics, Bytes::from(data)),
            },
            block_hash: Some(hash_of(number)),
            block_number: Some(number),
            block_timestamp: None,
            transaction_hash: Some(TX),
            transaction_index: Some(0),
            log_index: Some(index),
            removed: false,
        }
    }

    fn created(number: u64) -> Log {
        log(
            vec![CREATED, B256::from(U256::from(7)), GUARDIAN.into_word()],
            U256::from(150_000_001).abi_encode(),
            number,
            1,
        )
    }

    fn reduction() -> Log {
        log(
            vec![REDUCED, B256::from(U256::from(7))],
            (
                100_000_001u128,
                40_000_000u128,
                B256::repeat_byte(0x77),
                GUARDIAN,
            )
                .abi_encode(),
            10,
            3,
        )
    }

    fn header(number: u64) -> Block<B256> {
        let mut block = Block::<B256>::default();
        block.header.hash = hash_of(number);
        block.header.inner.number = number;
        block.header.inner.timestamp = timestamp_of(number);
        block
    }

    fn receipt(log: Log, success: bool) -> TransactionReceipt {
        TransactionReceipt {
            inner: ReceiptEnvelope::Legacy(
                Receipt {
                    status: success.into(),
                    cumulative_gas_used: 30_000,
                    logs: vec![log],
                }
                .with_bloom(),
            ),
            transaction_hash: TX,
            transaction_index: Some(0),
            block_hash: Some(hash_of(10)),
            block_number: Some(10),
            gas_used: 30_000,
            effective_gas_price: 1,
            blob_gas_used: None,
            blob_gas_price: None,
            from: GUARDIAN,
            to: Some(CONTROLLER),
            contract_address: None,
        }
    }

    #[tokio::test]
    async fn scans_exact_controller_outflows_through_provider_span_limits() {
        let instant = log(
            vec![INSTANT, GUARDIAN.into_word()],
            (15_000_001u128, 9_999_999u128).abi_encode(),
            4,
            2,
        );
        let node = FakeNode::new(vec![created(2), instant], 2);
        let rows = History::new(node.provider())
            .withdrawal_events(CONTROLLER, 1, Some(5))
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].kind,
            WithdrawalEventKind::ClaimCreated {
                claim_id: U256::from(7),
                recipient: GUARDIAN,
                amount: UsdcAtoms::new(150_000_001),
            }
        );
        assert_eq!(
            rows[1].kind,
            WithdrawalEventKind::InstantPayout {
                recipient: GUARDIAN,
                amount: UsdcAtoms::new(15_000_001),
                available_after: UsdcAtoms::new(9_999_999),
            }
        );
        assert_eq!(rows[0].block.hash, hash_of(2));
        assert_eq!(rows[1].block.timestamp, timestamp_of(4));
        assert_eq!(rows[1].tx_hash, TX);
        assert_eq!(rows[1].log_index, 2);
        assert!(
            node.requests().iter().any(|(from, to)| to - from + 1 > 2),
            "provider rejected a wide request before the scanner narrowed"
        );
    }

    #[tokio::test]
    async fn malformed_reorged_and_duplicate_logs_never_become_a_partial_window() {
        let good = created(2);
        let mut missing_tx = good.clone();
        missing_tx.transaction_hash = None;
        let mut wrong_hash = good.clone();
        wrong_hash.block_hash = Some(B256::ZERO);
        let mut removed = good.clone();
        removed.removed = true;
        let mut bad_data = good.clone();
        bad_data.inner.data.data = Bytes::new();
        for logs in [
            vec![missing_tx],
            vec![wrong_hash],
            vec![removed],
            vec![bad_data],
            vec![good.clone(), good],
        ] {
            let node = FakeNode::new(logs, 10);
            assert!(matches!(
                History::new(node.provider())
                    .withdrawal_events(CONTROLLER, 1, Some(3))
                    .await,
                Err(PerpCityError::Validation(
                    ValidationError::DecodeFailed { .. }
                ))
            ));
        }
    }

    #[tokio::test]
    async fn even_an_empty_window_rejects_a_changed_anchor() {
        let rpc = Asserter::new();
        rpc.push_success(&header(10));
        rpc.push_success(&Vec::<Log>::new());
        let mut changed = header(10);
        changed.header.hash = B256::repeat_byte(0x99);
        rpc.push_success(&changed);
        let history = History::new(ProviderBuilder::new().connect_mocked_client(rpc));
        let error = history
            .withdrawal_events(CONTROLLER, 1, Some(10))
            .await
            .unwrap_err();
        assert_reorganized(&error, 10);
    }

    fn assert_reorganized(error: &PerpCityError, expected: u64) {
        assert!(
            matches!(error, PerpCityError::Contract(ContractError::ChainReorganized { number }) if *number == expected),
            "{error}"
        );
        assert!(error.is_transient());
        assert!(!error.is_unconfirmed());
        assert_eq!(error.tx_hash(), None);
    }

    #[tokio::test]
    async fn repayment_endpoint_distinguishes_confirmation_lag_and_canonicality() {
        for changed_block in [None, Some(20), Some(10)] {
            let rpc = Asserter::new();
            rpc.push_success(&"0x14");
            let mut endpoint = header(20);
            let mut source = header(10);
            if changed_block == Some(20) {
                endpoint.header.hash = B256::ZERO;
            }
            if changed_block == Some(10) {
                source.header.hash = B256::ZERO;
            }
            if changed_block.is_some() {
                rpc.push_success(&endpoint);
                if changed_block == Some(10) {
                    rpc.push_success(&source);
                }
            }
            let history = History::new(ProviderBuilder::new().connect_mocked_client(rpc.clone()))
                .with_lag(if changed_block.is_some() { 0 } else { 8 });
            let error = history
                .confirmed_repayment(REPAY_TX, TOKEN, MARKET, &reduction_evidence(), context(20))
                .await
                .unwrap_err();
            if let Some(number) = changed_block {
                assert_reorganized(&error, number);
            } else {
                assert!(
                    matches!(
                        error,
                        PerpCityError::Contract(ContractError::BlockUnconfirmed {
                            number: 20,
                            confirmed_through: 12
                        })
                    ),
                    "{error}"
                );
                assert!(error.is_transient());
                assert!(!error.is_unconfirmed());
                assert_eq!(error.tx_hash(), None);
            }
            assert!(rpc.read_q().is_empty());
        }
    }

    #[tokio::test]
    async fn reduction_rechecks_the_receipt_block_after_decoding() {
        let rpc = Asserter::new();
        rpc.push_success(&Some(receipt(reduction(), true)));
        rpc.push_success(&"0x14");
        rpc.push_success(&header(10));
        let mut changed = header(10);
        changed.header.hash = B256::ZERO;
        rpc.push_success(&changed);
        let error = History::new(ProviderBuilder::new().connect_mocked_client(rpc.clone()))
            .confirmed_reduction(TX, CONTROLLER, U256::from(7), 3)
            .await
            .unwrap_err();
        assert_reorganized(&error, 10);
        assert!(rpc.read_q().is_empty());
    }

    const TOKEN: Address = Address::repeat_byte(0x55);
    const MARKET: Address = Address::repeat_byte(0x66);
    const REPAY_TX: B256 = B256::repeat_byte(0x44);

    fn context(number: u64) -> BlockContext {
        BlockContext {
            number,
            hash: hash_of(number),
            timestamp: timestamp_of(number),
        }
    }

    fn reduction_evidence() -> ReductionEvidence {
        ReductionEvidence {
            event: decode(&reduction(), CONTROLLER, context(10)).unwrap(),
            claim_id: U256::from(7),
            guardian: GUARDIAN,
            recovered: UsdcAtoms::new(60_000_001),
        }
    }

    fn repayment() -> TransactionReceipt {
        let mut transfer = log(
            vec![
                IERC20::Transfer::SIGNATURE_HASH,
                GUARDIAN.into_word(),
                MARKET.into_word(),
            ],
            U256::from(60_000_001).abi_encode(),
            11,
            4,
        );
        transfer.inner.address = TOKEN;
        transfer.transaction_hash = Some(REPAY_TX);
        let mut receipt = receipt(transfer, true);
        receipt.transaction_hash = REPAY_TX;
        receipt.block_number = Some(11);
        receipt.block_hash = Some(hash_of(11));
        receipt.to = Some(TOKEN);
        receipt
    }

    fn repayment_answers(rpc: &Asserter, receipt: Option<TransactionReceipt>) {
        rpc.push_success(&"0x14");
        rpc.push_success(&header(20));
        rpc.push_success(&header(10));
        rpc.push_success(&receipt);
        if receipt.as_ref().is_some_and(|r| r.block_number <= Some(20)) {
            rpc.push_success(&header(11));
            rpc.push_success(&header(11));
            rpc.push_success(&header(10));
            rpc.push_success(&header(20));
        }
    }

    #[tokio::test]
    async fn repayment_proves_one_exact_transfer_through_the_pinned_endpoint() {
        for safe_executor in [false, true] {
            let mut receipt = repayment();
            if safe_executor {
                receipt.from = Address::repeat_byte(0xaa);
                receipt.to = Some(GUARDIAN);
            }
            let rpc = Asserter::new();
            repayment_answers(&rpc, Some(receipt));
            let history =
                History::new(ProviderBuilder::new().connect_mocked_client(rpc.clone())).with_lag(0);
            let result = history
                .confirmed_repayment(REPAY_TX, TOKEN, MARKET, &reduction_evidence(), context(20))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.transfer().tx_hash, REPAY_TX);
            assert_eq!(result.transfer().log_index, 4);
            assert_eq!(result.transfer().from, GUARDIAN);
            assert_eq!(result.transfer().to, MARKET);
            assert_eq!(result.transfer().value, U256::from(60_000_001));
            assert_eq!(result.block(), context(11));
            assert_eq!(result.token(), TOKEN);
            assert!(rpc.read_q().is_empty());
        }
    }

    #[tokio::test]
    async fn repayment_refuses_foreign_failed_ambiguous_and_reorged_evidence() {
        let mut cases = Vec::new();
        let mut bad = repayment();
        bad.transaction_hash = TX;
        cases.push((bad, "another transaction"));
        let mut bad = repayment();
        bad.block_hash = Some(B256::ZERO);
        cases.push((bad, "reorganized"));
        for mutation in 0..7 {
            let mut bad = repayment();
            let mut logs = bad.inner.logs().to_vec();
            match mutation {
                0 => {
                    logs[0].inner.address = CONTROLLER;
                }
                1 => {
                    logs[0].inner.data = LogData::new_unchecked(
                        logs[0].topics().to_vec(),
                        U256::from(60_000_000).abi_encode().into(),
                    );
                }
                2 => {
                    logs.push(logs[0].clone());
                }
                3 => {
                    logs[0].block_hash = Some(B256::ZERO);
                }
                5 | 6 => {
                    let mut topics = logs[0].topics().to_vec();
                    topics[if mutation == 5 { 1 } else { 2 }] =
                        Address::repeat_byte(0x99).into_word();
                    logs[0].inner.data =
                        LogData::new_unchecked(topics, logs[0].data().data.clone());
                }
                _ => {}
            }
            bad.inner = ReceiptEnvelope::Legacy(
                Receipt {
                    status: (mutation != 4).into(),
                    cumulative_gas_used: 30_000,
                    logs,
                }
                .with_bloom(),
            );
            cases.push((
                bad,
                match mutation {
                    3 => "log is not canonical",
                    4 => "receipt reverted",
                    _ => "exactly one matching transfer",
                },
            ));
        }
        for (receipt, expected) in cases {
            let receipt_block = 11;
            let expected_tx = REPAY_TX;
            let rpc = Asserter::new();
            repayment_answers(&rpc, Some(receipt));
            let history =
                History::new(ProviderBuilder::new().connect_mocked_client(rpc)).with_lag(0);
            let error = history
                .confirmed_repayment(REPAY_TX, TOKEN, MARKET, &reduction_evidence(), context(20))
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            match expected {
                "reorganized" => assert_reorganized(&error, receipt_block),
                "receipt reverted" => {
                    assert!(matches!(
                        error,
                        PerpCityError::Transaction(TransactionError::Reverted { .. })
                    ));
                    assert_eq!(error.tx_hash(), Some(expected_tx));
                    assert!(!error.is_transient());
                    assert!(!error.is_unconfirmed());
                }
                _ => {
                    assert!(
                        matches!(
                            error,
                            PerpCityError::Validation(ValidationError::DecodeFailed { .. })
                        ),
                        "{error}"
                    );
                    assert!(!error.is_transient());
                }
            }
        }
        for changed_block in [10, 11, 20] {
            let rpc = Asserter::new();
            rpc.push_success(&"0x14");
            rpc.push_success(&header(20));
            rpc.push_success(&header(10));
            rpc.push_success(&Some(repayment()));
            rpc.push_success(&header(11));
            for number in [11, 10, 20] {
                let mut block = header(number);
                if number == changed_block {
                    block.header.hash = B256::ZERO;
                }
                rpc.push_success(&block);
            }
            let error = History::new(ProviderBuilder::new().connect_mocked_client(rpc))
                .with_lag(0)
                .confirmed_repayment(REPAY_TX, TOKEN, MARKET, &reduction_evidence(), context(20))
                .await
                .unwrap_err();
            assert_reorganized(&error, changed_block);
        }
    }

    #[tokio::test]
    async fn repayment_absence_and_receipts_after_the_endpoint_remain_pending() {
        let mut future = repayment();
        future.block_number = Some(21);
        for receipt in [None, Some(future)] {
            let rpc = Asserter::new();
            repayment_answers(&rpc, receipt);
            assert!(
                History::new(ProviderBuilder::new().connect_mocked_client(rpc))
                    .with_lag(0)
                    .confirmed_repayment(
                        REPAY_TX,
                        TOKEN,
                        MARKET,
                        &reduction_evidence(),
                        context(20)
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn recovery_is_the_confirmed_event_delta_not_a_requested_amount() {
        let rpc = Asserter::new();
        rpc.push_success(&Some(receipt(reduction(), true)));
        rpc.push_success(&"0x14");
        rpc.push_success(&header(10));
        rpc.push_success(&header(10));
        let evidence = History::new(ProviderBuilder::new().connect_mocked_client(rpc))
            .confirmed_reduction(TX, CONTROLLER, U256::from(7), 3)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evidence.recovered(), UsdcAtoms::new(60_000_001));
        assert_eq!(evidence.guardian(), GUARDIAN);
        assert_eq!(evidence.claim_id(), U256::from(7));
        assert_eq!(evidence.event().tx_hash, TX);
        assert_eq!(evidence.event().block.hash, hash_of(10));
    }

    #[tokio::test]
    async fn absent_and_recent_receipts_are_pending_not_failed_or_recoverable() {
        let absent = Asserter::new();
        absent.push_success(&Option::<TransactionReceipt>::None);
        assert!(
            History::new(ProviderBuilder::new().connect_mocked_client(absent))
                .confirmed_reduction(TX, CONTROLLER, U256::from(7), 3)
                .await
                .unwrap()
                .is_none()
        );
        let recent = Asserter::new();
        recent.push_success(&Some(receipt(reduction(), true)));
        recent.push_success(&"0xa");
        assert!(
            History::new(ProviderBuilder::new().connect_mocked_client(recent))
                .confirmed_reduction(TX, CONTROLLER, U256::from(7), 3)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn recovery_refuses_failed_foreign_and_reorged_evidence() {
        for (receipt, controller, claim, expected) in [
            (
                receipt(reduction(), false),
                CONTROLLER,
                U256::from(7),
                "receipt reverted",
            ),
            (
                receipt(reduction(), true),
                Address::repeat_byte(0x99),
                U256::from(7),
                "identity differs",
            ),
            (
                receipt(reduction(), true),
                CONTROLLER,
                U256::from(8),
                "another claim",
            ),
            (
                {
                    let mut receipt = receipt(reduction(), true);
                    receipt.block_hash = Some(B256::ZERO);
                    receipt
                },
                CONTROLLER,
                U256::from(7),
                "reorganized",
            ),
        ] {
            let receipt_block = 10;
            let expected_tx = TX;
            let rpc = Asserter::new();
            rpc.push_success(&Some(receipt));
            rpc.push_success(&"0x14");
            rpc.push_success(&header(10));
            let error = History::new(ProviderBuilder::new().connect_mocked_client(rpc))
                .confirmed_reduction(TX, controller, claim, 3)
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            match expected {
                "reorganized" => assert_reorganized(&error, receipt_block),
                "receipt reverted" => {
                    assert!(matches!(
                        error,
                        PerpCityError::Transaction(TransactionError::Reverted { .. })
                    ));
                    assert_eq!(error.tx_hash(), Some(expected_tx));
                    assert!(!error.is_transient());
                    assert!(!error.is_unconfirmed());
                }
                _ => {
                    assert!(
                        matches!(
                            error,
                            PerpCityError::Validation(ValidationError::DecodeFailed { .. })
                        ),
                        "{error}"
                    );
                    assert!(!error.is_transient());
                }
            }
        }
    }
}
