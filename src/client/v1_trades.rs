//! `v1.0.0` taker writes: open, adjust and close with a deadline, a fee
//! bound and reduce-only, and the replacement of a stuck one. Each returns
//! the receipt's parsed event, so no `v1.0.0` taker write reports success
//! without it.

use std::time::Duration;

use alloy::consensus::Transaction as ConsensusTransaction;
use alloy::network::{TransactionBuilder, TransactionResponse as _};
use alloy::primitives::{Address, B256, Bytes, I256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol_types::SolCall;
use serde::{Deserialize, Serialize};

use crate::constants::{RECEIPT_POLL_INTERVAL, RECEIPT_TIMEOUT};
use crate::contracts::v1::{AdjustTakerParams, OpenTakerParams, Perp};
use crate::errors::{ContractError, PerpCityError, Result, TransactionError, ValidationError};
use crate::events::receipt::{V1TradeResult, parse_v1_receipt, parse_v1_receipt_for};
use crate::hft::gas::{GasFees, Urgency};
use crate::math::swap::MAX_TOTAL_FEE_SLACK;
use crate::units::{PerpDelta, UsdcAtoms, UsdcDelta};

use super::queries::Era;
use super::trades::{ExactAdjustTakerParams, ExactOpenTakerParams, check_opening_margin};
use super::{PerpClient, now_ms};

/// How long a `v1.0.0` trade stays valid after it is signed: its
/// `deadline` is the signing time plus this, and the contract reverts
/// `DeadlineExpired` after it. A replacement signs a new one.
pub const V1_TRADE_DEADLINE: Duration = Duration::from_secs(600);

/// The `maxTotalFee` a `v1.0.0` taker trade carries: the most LP,
/// protocol, creator and insurance fee its swap may charge to margin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MaxTotalFee {
    /// The bound for a swap that moves at most this much USDC, from the
    /// market's rates read at the send
    /// ([`TakerFeeRates::max_total_fee`](crate::TakerFeeRates::max_total_fee)
    /// with [`MAX_TOTAL_FEE_SLACK`]). A buy's `amt1_limit` is such a
    /// volume; a sell's volume is its quote.
    ForVolume(UsdcAtoms),
    /// This bound, as it is.
    Exact(UsdcAtoms),
}

/// What a `v1.0.0` taker adjustment mined as: the position still open, or
/// closed because no perp amount is left.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum V1TakerAdjustment {
    /// The position is still open.
    Adjusted(V1TradeResult<Perp::TakerAdjusted>),
    /// The position is closed and its owner cleared.
    Closed(V1TradeResult<Perp::TakerClosed>),
}

/// What a replaced `v1.0.0` taker trade mined as. The result's `tx_hash`
/// is the replacement's, or the stuck transaction's when that one mined
/// first.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum V1TakerReplacement {
    /// An open.
    Opened(V1TradeResult<Perp::TakerOpened>),
    /// An adjustment or a close.
    Adjustment(V1TakerAdjustment),
}

/// The taker call a stuck transaction carries, as its replacement needs
/// it to parse the receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StuckCall {
    Open { holder: Address },
    Adjust { pos_id: U256 },
}

/// The deadline of a trade signed at `now_secs`.
fn deadline_after(now_secs: u64) -> u64 {
    now_secs.saturating_add(V1_TRADE_DEADLINE.as_secs())
}

/// `openTaker` for `holder`.
fn open_call(
    holder: Address,
    params: &ExactOpenTakerParams,
    max_total_fee: UsdcAtoms,
    deadline: u64,
) -> Perp::openTakerCall {
    Perp::openTakerCall {
        params: OpenTakerParams {
            holder,
            margin: params.margin.atoms(),
            perpDelta: I256::try_from(params.perp_delta.atoms()).expect("i128 fits I256"),
            amt1Limit: U256::from(params.amt1_limit.atoms()),
            maxTotalFee: U256::from(max_total_fee.atoms()),
            deadline,
        },
    }
}

/// `adjustTaker`.
fn adjust_call(
    params: &ExactAdjustTakerParams,
    max_total_fee: UsdcAtoms,
    reduce_only: bool,
    deadline: u64,
) -> Perp::adjustTakerCall {
    Perp::adjustTakerCall {
        params: AdjustTakerParams {
            posId: params.pos_id,
            marginDelta: params.margin_delta.atoms(),
            perpDelta: I256::try_from(params.perp_delta.atoms()).expect("i128 fits I256"),
            amt1Limit: U256::from(params.amt1_limit.atoms()),
            maxTotalFee: U256::from(max_total_fee.atoms()),
            reduceOnly: reduce_only,
            deadline,
        },
    }
}

/// The contract's reduce-only rule (`Perp._checkReduceOnly`): the change
/// lies between zero and the delta that closes `position` exactly, so it
/// neither grows the position nor flips it through zero.
fn check_reduce_only(
    position: PerpDelta,
    perp_delta: PerpDelta,
) -> std::result::Result<(), ValidationError> {
    let closing = closing_delta(position);
    let (lowest, highest) = if closing < I256::ZERO {
        (closing, I256::ZERO)
    } else {
        (I256::ZERO, closing)
    };
    if (lowest..=highest).contains(&I256::try_from(perp_delta.atoms()).expect("i128 fits I256")) {
        Ok(())
    } else {
        Err(ValidationError::ReduceOnlyViolated {
            position: position.atoms(),
            perp_delta: perp_delta.atoms(),
        })
    }
}

/// Reverse the stored perp amount in a type wide enough for either sign.
fn closing_delta(position: PerpDelta) -> I256 {
    -I256::try_from(position.atoms()).expect("i128 fits I256")
}

/// Identify the v1 taker call and renew its deadline when it can advance.
/// A mined call can still be parsed when its deadline cannot be renewed.
fn replacement_calldata(input: &[u8], now_secs: u64) -> Option<(StuckCall, Option<Bytes>)> {
    let renewed = |old: u64| {
        old.checked_add(1)
            .map(|next| deadline_after(now_secs).max(next))
    };
    match input.get(..4)?.try_into().ok()? {
        Perp::openTakerCall::SELECTOR => {
            let mut call = Perp::openTakerCall::abi_decode(input).ok()?;
            let kind = StuckCall::Open {
                holder: call.params.holder,
            };
            let calldata = renewed(call.params.deadline).map(|deadline| {
                call.params.deadline = deadline;
                call.abi_encode().into()
            });
            Some((kind, calldata))
        }
        Perp::adjustTakerCall::SELECTOR => {
            let mut call = Perp::adjustTakerCall::abi_decode(input).ok()?;
            let kind = StuckCall::Adjust {
                pos_id: call.params.posId,
            };
            let calldata = renewed(call.params.deadline).map(|deadline| {
                call.params.deadline = deadline;
                call.abi_encode().into()
            });
            Some((kind, calldata))
        }
        _ => None,
    }
}

/// A replacement's fees: each at least an eighth above the stuck one's,
/// which a node needs to replace a pending transaction, and at least what
/// the urgency asks of the current base fee when the cache has one.
fn replacement_fees(max_fee: u128, priority: u128, current: Option<GasFees>) -> (u128, u128) {
    let bump = |fee: u128| fee.saturating_add(fee / 8).saturating_add(1);
    let (mut max_fee, mut priority) = (bump(max_fee), bump(priority));
    if let Some(current) = current {
        max_fee = max_fee.max(u128::from(current.max_fee_per_gas));
        priority = priority.max(u128::from(current.max_priority_fee_per_gas));
    }
    (max_fee.max(priority), priority)
}

/// The adjustment `receipt` mined for `pos_id` on `perp`.
fn taker_adjustment(
    receipt: &TransactionReceipt,
    perp: Address,
    pos_id: U256,
) -> Result<V1TakerAdjustment> {
    match parse_v1_receipt::<Perp::TakerClosed>(receipt, perp, Some(pos_id)) {
        Ok(closed) => Ok(V1TakerAdjustment::Closed(closed)),
        Err(PerpCityError::Contract(ContractError::MinedEventMissing { .. })) => {
            match parse_v1_receipt::<Perp::TakerAdjusted>(receipt, perp, Some(pos_id)) {
                Ok(adjusted) => Ok(V1TakerAdjustment::Adjusted(adjusted)),
                Err(PerpCityError::Contract(ContractError::MinedEventMissing {
                    tx_hash, ..
                })) => Err(ContractError::MinedEventMissing {
                    tx_hash,
                    event: "TakerAdjusted or TakerClosed",
                }
                .into()),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

impl PerpClient {
    /// Open a taker on a `v1.0.0` market, with a deadline
    /// [`V1_TRADE_DEADLINE`] after now and the `maxTotalFee` that
    /// `max_total_fee` names. The market's era is read first; nothing is
    /// sent to another build.
    ///
    /// Returns the receipt's `TakerOpened` for this client, the minted id
    /// and the margin left after the swap's fees.
    ///
    /// # Errors
    ///
    /// [`ContractError::UnsupportedEra`] for an earlier build and
    /// [`ContractError::UnknownBuild`] for an unknown one, before anything
    /// else; [`ValidationError::InvalidMargin`] below the opening minimum;
    /// the send's errors, with the contract's revert decoded at the
    /// preflight; [`ContractError::MinedEventMissing`] or
    /// [`ContractError::MinedEventInvalid`] for a mined receipt without a
    /// readable event: the trade mined, so do not send it again.
    pub async fn open_taker_v1(
        &self,
        params: &ExactOpenTakerParams,
        max_total_fee: MaxTotalFee,
        urgency: Urgency,
    ) -> Result<V1TradeResult<Perp::TakerOpened>> {
        self.require_v1("open_taker_v1").await?;
        check_opening_margin(params.margin)?;
        let max_total_fee = self.resolve_max_total_fee(max_total_fee).await?;
        let call = open_call(
            self.address,
            params,
            max_total_fee,
            deadline_after(now_ms() / 1_000),
        );
        tracing::debug!(
            margin_atoms = params.margin.atoms(),
            perp_delta_atoms = params.perp_delta.atoms(),
            max_total_fee = max_total_fee.atoms(),
            deadline = call.params.deadline,
            ?urgency,
            "opening v1 taker position"
        );
        let receipt = self
            .tx(self.market.perp(), call.abi_encode().into())
            .with_urgency(urgency)
            .send()
            .await?;
        parse_v1_receipt_for(&receipt, self.market.perp(), None, Some(self.address))
    }

    /// Adjust a taker on a `v1.0.0` market, with a deadline
    /// [`V1_TRADE_DEADLINE`] after now, the `maxTotalFee` that
    /// `max_total_fee` names, and `reduce_only` on the wire.
    ///
    /// With `reduce_only`, the position is read first and a change that
    /// would grow it or flip it through zero is refused before anything is
    /// sent; the contract checks the same rule again at the block it mines
    /// in (`ReduceOnlyViolated`).
    ///
    /// # Errors
    ///
    /// As [`Self::open_taker_v1`]; [`ValidationError::ReduceOnlyViolated`]
    /// as above; [`ContractError::PositionNotFound`] for a closed id when
    /// `reduce_only` reads it.
    pub async fn adjust_taker_v1(
        &self,
        params: &ExactAdjustTakerParams,
        max_total_fee: MaxTotalFee,
        reduce_only: bool,
        urgency: Urgency,
    ) -> Result<V1TakerAdjustment> {
        self.require_v1("adjust_taker_v1").await?;
        if reduce_only {
            let position = self.market.get_position(params.pos_id).await?;
            check_reduce_only(position.perp, params.perp_delta)?;
        }
        let receipt = self
            .send_adjust_taker_v1(params, max_total_fee, reduce_only, urgency)
            .await?;
        taker_adjustment(&receipt, self.market.perp(), params.pos_id)
    }

    /// Close a taker on a `v1.0.0` market: its perp amount read now and
    /// reversed to the atom, `reduce_only` set, so a size that moved before
    /// the block can shrink the close but never flip the position.
    ///
    /// `amt1_limit` is the directional USDC limit of the closing swap; `None`
    /// is a market close (any proceeds for a sell, any cost for a buy).
    ///
    /// # Errors
    ///
    /// As [`Self::adjust_taker_v1`]; [`ValidationError::Overflow`] when the
    /// closing delta exceeds the signed 128-bit amount range;
    /// [`TransactionError::TakerNotClosed`], transient, when the close mined
    /// as an adjustment because the position changed after the read.
    pub async fn close_taker_v1(
        &self,
        pos_id: U256,
        amt1_limit: Option<UsdcAtoms>,
        max_total_fee: MaxTotalFee,
        urgency: Urgency,
    ) -> Result<V1TradeResult<Perp::TakerClosed>> {
        self.require_v1("close_taker_v1").await?;
        let position = self.market.get_position(pos_id).await?;
        let perp_delta =
            PerpDelta::new(i128::try_from(closing_delta(position.perp)).map_err(|_| {
                ValidationError::Overflow {
                    context: format!("closing perp delta for position {pos_id} exceeds int128"),
                }
            })?);
        let market_limit = if perp_delta.atoms() > 0 { u128::MAX } else { 0 };
        let params = ExactAdjustTakerParams {
            pos_id,
            margin_delta: UsdcDelta::ZERO,
            perp_delta,
            amt1_limit: amt1_limit.unwrap_or(UsdcAtoms::new(market_limit)),
        };
        let receipt = self
            .send_adjust_taker_v1(&params, max_total_fee, true, urgency)
            .await?;
        match taker_adjustment(&receipt, self.market.perp(), pos_id)? {
            V1TakerAdjustment::Closed(closed) => Ok(closed),
            V1TakerAdjustment::Adjusted(adjusted) => Err(TransactionError::TakerNotClosed {
                tx_hash: adjusted.tx_hash,
                pos_id,
            }
            .into()),
        }
    }

    /// Sign a stuck `v1.0.0` taker trade of this client again: the same
    /// nonce, the same call with a new deadline [`V1_TRADE_DEADLINE`] after
    /// now or one second after the original deadline, whichever is later,
    /// with fees at least an eighth higher and a new signature.
    ///
    /// Call it with the hash of a send that returned
    /// [`TransactionError::ReceiptTimeout`] or
    /// [`TransactionError::BroadcastFailed`]. When the stuck transaction has
    /// mined already, nothing is signed and its outcome is returned. Else
    /// the replacement is preflighted at the stuck one's gas limit. A
    /// failed preflight checks the original receipt. A broadcast attempt
    /// checks both hashes and parses the first receipt it finds.
    ///
    /// # Errors
    ///
    /// Earlier and unknown builds are refused as in [`Self::open_taker_v1`].
    /// [`TransactionError::NotReplaceable`] when the node does not know the
    /// hash, when it is not this client's `openTaker` or `adjustTaker` on
    /// this market, when its send still waits for its receipt, or when it
    /// is not an EIP-1559 transaction or its deadline cannot advance;
    /// [`TransactionError::ReplacementUnconfirmed`] when receipt checks
    /// cannot establish the outcome; the signing and parse errors of
    /// [`Self::open_taker_v1`] and [`Self::adjust_taker_v1`].
    pub async fn replace_v1_taker(
        &self,
        stuck: B256,
        urgency: Urgency,
    ) -> Result<V1TakerReplacement> {
        self.require_v1("replace_v1_taker").await?;
        let refuse = |reason| -> PerpCityError {
            TransactionError::NotReplaceable {
                tx_hash: stuck,
                reason,
            }
            .into()
        };
        let perp = self.market.perp();
        let provider = self.chain().provider();
        let tx = provider
            .get_transaction_by_hash(stuck)
            .await?
            .ok_or_else(|| refuse("the node does not know it"))?;
        if tx.from() != self.address || tx.to() != Some(perp) {
            return Err(refuse("not this client's trade on this market"));
        }
        let (call, calldata) = replacement_calldata(tx.input(), now_ms() / 1_000)
            .ok_or_else(|| refuse("not a v1.0.0 openTaker or adjustTaker"))?;
        if tx.block_number.is_some() {
            let receipt = self.poll_receipt(stuck).await?;
            return self.replacement_outcome(call, &receipt);
        }
        let calldata = calldata.ok_or_else(|| refuse("its deadline cannot advance"))?;
        if self.pipeline.lock().unwrap().is_in_flight(&stuck.0) {
            return Err(refuse("its send still waits for the receipt"));
        }
        let old_priority = tx
            .max_priority_fee_per_gas()
            .ok_or_else(|| refuse("not an EIP-1559 transaction"))?;
        let current = self
            .chain()
            .fee_cache()
            .lock()
            .unwrap()
            .fees_for(urgency, now_ms());
        let (max_fee, priority) = replacement_fees(
            ConsensusTransaction::max_fee_per_gas(&tx),
            old_priority,
            current,
        );
        let gas_limit = tx.gas_limit();
        if let Err(source) = self
            .chain()
            .preflight_call(self.address, perp, &calldata, 0, Some(gas_limit))
            .await
        {
            if let Ok(Some(receipt)) = provider.get_transaction_receipt(stuck).await {
                return self.replacement_outcome(call, &receipt);
            }
            return Err(TransactionError::ReplacementUnconfirmed {
                original: stuck,
                replacement: None,
                source: Some(Box::new(source)),
            }
            .into());
        }

        let request = TransactionRequest::default()
            .with_to(perp)
            .with_input(calldata)
            .with_value(U256::ZERO)
            .with_nonce(tx.nonce())
            .with_gas_limit(gas_limit)
            .with_max_fee_per_gas(max_fee)
            .with_max_priority_fee_per_gas(priority)
            .with_chain_id(self.chain().chain_id());
        let envelope =
            request
                .build(&self.wallet)
                .await
                .map_err(|e| TransactionError::SigningFailed {
                    reason: format!("{e}"),
                })?;
        let replacement = *envelope.tx_hash();
        let broadcast_error = provider.send_tx_envelope(envelope).await.err();
        tracing::debug!(
            %stuck,
            %replacement,
            nonce = tx.nonce(),
            max_fee,
            priority,
            ?urgency,
            broadcast_acknowledged = broadcast_error.is_none(),
            "sent v1 taker replacement"
        );
        match self.first_receipt(replacement, stuck).await {
            Some(receipt) => self.replacement_outcome(call, &receipt),
            None => Err(TransactionError::ReplacementUnconfirmed {
                original: stuck,
                replacement: Some(replacement),
                source: broadcast_error.map(|source| {
                    Box::new(TransactionError::BroadcastFailed {
                        tx_hash: replacement,
                        source,
                    })
                }),
            }
            .into()),
        }
    }

    /// Refuse anything but a `v1.0.0` market, after its era read.
    async fn require_v1(&self, operation: &'static str) -> Result<()> {
        match self.market.era().await? {
            Era::V1 => Ok(()),
            era => Err(ContractError::UnsupportedEra { era, operation }.into()),
        }
    }

    /// The bound `max_total_fee` names, reading the market's rates for a
    /// volume.
    async fn resolve_max_total_fee(&self, max_total_fee: MaxTotalFee) -> Result<UsdcAtoms> {
        match max_total_fee {
            MaxTotalFee::Exact(fee) => Ok(fee),
            MaxTotalFee::ForVolume(volume) => Ok(self
                .market
                .get_taker_fee_rates()
                .await?
                .max_total_fee(volume, MAX_TOTAL_FEE_SLACK)?),
        }
    }

    /// Sign, send and wait for one `adjustTaker`.
    async fn send_adjust_taker_v1(
        &self,
        params: &ExactAdjustTakerParams,
        max_total_fee: MaxTotalFee,
        reduce_only: bool,
        urgency: Urgency,
    ) -> Result<TransactionReceipt> {
        let max_total_fee = self.resolve_max_total_fee(max_total_fee).await?;
        let call = adjust_call(
            params,
            max_total_fee,
            reduce_only,
            deadline_after(now_ms() / 1_000),
        );
        tracing::debug!(
            pos_id = %params.pos_id,
            margin_delta_atoms = params.margin_delta.atoms(),
            perp_delta_atoms = params.perp_delta.atoms(),
            max_total_fee = max_total_fee.atoms(),
            reduce_only,
            deadline = call.params.deadline,
            ?urgency,
            "adjusting v1 taker position"
        );
        self.tx(self.market.perp(), call.abi_encode().into())
            .with_urgency(urgency)
            .send()
            .await
    }

    /// The first receipt of either hash, polled until [`RECEIPT_TIMEOUT`].
    async fn first_receipt(&self, replacement: B256, stuck: B256) -> Option<TransactionReceipt> {
        let provider = self.chain().provider();
        let deadline = tokio::time::Instant::now() + RECEIPT_TIMEOUT;
        loop {
            for hash in [replacement, stuck] {
                if let Ok(Some(receipt)) = provider.get_transaction_receipt(hash).await {
                    return Some(receipt);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(RECEIPT_POLL_INTERVAL).await;
        }
    }

    /// Parse a replaced trade's receipt as the call it carried.
    fn replacement_outcome(
        &self,
        call: StuckCall,
        receipt: &TransactionReceipt,
    ) -> Result<V1TakerReplacement> {
        if !receipt.status() {
            return Err(TransactionError::Reverted {
                tx_hash: receipt.transaction_hash,
                reason: format!("transaction {} reverted", receipt.transaction_hash),
            }
            .into());
        }
        let perp = self.market.perp();
        match call {
            StuckCall::Open { holder } => parse_v1_receipt_for(receipt, perp, None, Some(holder))
                .map(V1TakerReplacement::Opened),
            StuckCall::Adjust { pos_id } => {
                taker_adjustment(receipt, perp, pos_id).map(V1TakerReplacement::Adjustment)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::consensus::transaction::Recovered;
    use alloy::consensus::{Receipt, ReceiptEnvelope};
    use alloy::eips::BlockId;
    use alloy::network::TransactionBuilder;
    use alloy::primitives::{Address, B256, Bytes, I256, U256};
    use alloy::rpc::types::{Transaction, TransactionReceipt, TransactionRequest};
    use alloy::sol_types::{SolCall, SolEvent};

    use super::{
        MaxTotalFee, StuckCall, V1_TRADE_DEADLINE, V1TakerAdjustment, V1TakerReplacement,
        adjust_call, check_reduce_only, open_call, replacement_calldata, replacement_fees,
    };
    use crate::client::queries::Era;
    use crate::client::trades::{ExactAdjustTakerParams, ExactOpenTakerParams};
    use crate::client::{
        ARBITRUM_CHAIN_ID, ARBITRUM_SEPOLIA_CHAIN_ID, ChainDeployments, PerpClient, mock,
    };
    use crate::constants::{RECEIPT_POLL_INTERVAL, RECEIPT_TIMEOUT};
    use crate::contracts::{Perp as EarlierPerp, v1};
    use crate::convert::pack_balance_delta;
    use crate::errors::{ContractError, PerpCityError, TransactionError, ValidationError};
    use crate::hft::gas::{GasFees, Urgency};
    use crate::history::test_support::mined_event_log;
    use crate::units::{PerpDelta, UsdcAtoms, UsdcDelta};

    const HOLDER: Address = Address::repeat_byte(0x77);
    const NOW: u64 = 1_763_600_000;

    fn open() -> ExactOpenTakerParams {
        ExactOpenTakerParams {
            margin: UsdcAtoms::new(10_000_000),
            perp_delta: PerpDelta::new(400_000),
            amt1_limit: UsdcAtoms::new(20_342_065),
        }
    }

    fn adjust(perp_delta: i128) -> ExactAdjustTakerParams {
        ExactAdjustTakerParams {
            pos_id: U256::from(2u8),
            margin_delta: UsdcDelta::new(-1_000_000),
            perp_delta: PerpDelta::new(perp_delta),
            amt1_limit: UsdcAtoms::ZERO,
        }
    }

    /// The trade build test: the `v1.0.0` selectors carry the fee bound,
    /// the deadline and reduce-only as the caller set them.
    #[test]
    fn the_calldata_carries_the_fee_bound_the_deadline_and_reduce_only() {
        let opened = open_call(HOLDER, &open(), UsdcAtoms::new(177_076), NOW + 600);
        let bytes = opened.abi_encode();
        assert_eq!(bytes[..4], v1::Perp::openTakerCall::SELECTOR);
        assert_ne!(bytes[..4], EarlierPerp::openTakerCall::SELECTOR);
        let decoded = v1::Perp::openTakerCall::abi_decode(&bytes).unwrap().params;
        assert_eq!(decoded.holder, HOLDER);
        assert_eq!(decoded.margin, 10_000_000);
        assert_eq!(decoded.perpDelta, I256::try_from(400_000).unwrap());
        assert_eq!(decoded.amt1Limit, U256::from(20_342_065u64));
        assert_eq!(decoded.maxTotalFee, U256::from(177_076u64));
        assert_eq!(decoded.deadline, NOW + 600);

        for reduce_only in [true, false] {
            let call = adjust_call(&adjust(-100_000), UsdcAtoms::new(1_000), reduce_only, NOW);
            let bytes = call.abi_encode();
            assert_eq!(bytes[..4], v1::Perp::adjustTakerCall::SELECTOR);
            let decoded = v1::Perp::adjustTakerCall::abi_decode(&bytes)
                .unwrap()
                .params;
            assert_eq!(decoded.posId, U256::from(2u8));
            assert_eq!(decoded.marginDelta, -1_000_000);
            assert_eq!(decoded.perpDelta, I256::try_from(-100_000).unwrap());
            assert_eq!(decoded.maxTotalFee, U256::from(1_000u64));
            assert_eq!(decoded.reduceOnly, reduce_only);
            assert_eq!(decoded.deadline, NOW);
        }
        assert_eq!(V1_TRADE_DEADLINE.as_secs(), 600);
    }

    /// The contract's reduce-only rule, both sides: a change toward zero up
    /// to the exact close passes; growth or a flip through zero fails.
    #[test]
    fn reduce_only_allows_a_reduction_and_refuses_growth_and_a_flip() {
        let long = PerpDelta::new(400_000);
        for ok in [0, -1, -399_999, -400_000] {
            assert!(check_reduce_only(long, PerpDelta::new(ok)).is_ok(), "{ok}");
        }
        for refused in [1, -400_001, -800_000] {
            assert!(
                matches!(
                    check_reduce_only(long, PerpDelta::new(refused)),
                    Err(ValidationError::ReduceOnlyViolated {
                        position: 400_000,
                        perp_delta,
                    }) if perp_delta == refused
                ),
                "{refused}"
            );
        }
        let short = PerpDelta::new(-250_000);
        assert!(check_reduce_only(short, PerpDelta::new(250_000)).is_ok());
        assert!(check_reduce_only(short, PerpDelta::new(250_001)).is_err());
        assert!(check_reduce_only(short, PerpDelta::new(-1)).is_err());
        assert!(check_reduce_only(PerpDelta::ZERO, PerpDelta::ZERO).is_ok());
        assert!(check_reduce_only(PerpDelta::ZERO, PerpDelta::new(1)).is_err());
        let min = PerpDelta::new(i128::MIN);
        assert!(check_reduce_only(min, PerpDelta::new(i128::MAX)).is_ok());
        assert!(check_reduce_only(min, PerpDelta::new(-1)).is_err());
        assert!(check_reduce_only(PerpDelta::new(i128::MAX), min).is_err());
    }

    /// A replacement is the same call with a new deadline, now plus 600 s,
    /// and never earlier than one second after the stuck one's; any other
    /// call is not replaced.
    #[test]
    fn a_replacement_renews_only_the_deadline() {
        let stuck = open_call(HOLDER, &open(), UsdcAtoms::new(177_076), NOW + 600).abi_encode();
        let (call, bytes) = replacement_calldata(&stuck, NOW + 30).unwrap();
        let bytes = bytes.unwrap();
        assert_eq!(call, StuckCall::Open { holder: HOLDER });
        assert_ne!(bytes.as_ref(), stuck.as_slice());
        let mut expected = v1::Perp::openTakerCall::abi_decode(&stuck).unwrap();
        expected.params.deadline = NOW + 630;
        assert_eq!(bytes.as_ref(), expected.abi_encode().as_slice());

        // Signed again in the same second: the deadline still moves.
        let (_, same_second) = replacement_calldata(&stuck, NOW).unwrap();
        let renewed = v1::Perp::openTakerCall::abi_decode(&same_second.unwrap()).unwrap();
        assert_eq!(renewed.params.deadline, NOW + 601);

        let stuck = adjust_call(&adjust(-400_000), UsdcAtoms::new(1_000), true, NOW + 600);
        let (call, bytes) = replacement_calldata(&stuck.abi_encode(), NOW + 60).unwrap();
        assert_eq!(
            call,
            StuckCall::Adjust {
                pos_id: U256::from(2u8)
            }
        );
        let renewed = v1::Perp::adjustTakerCall::abi_decode(&bytes.unwrap())
            .unwrap()
            .params;
        assert_eq!(renewed.deadline, NOW + 660);
        assert!(renewed.reduceOnly);
        assert_eq!(renewed.perpDelta, stuck.params.perpDelta);

        let earlier = EarlierPerp::openTakerCall {
            params: crate::contracts::OpenTakerParams {
                holder: HOLDER,
                margin: 10_000_000,
                perpDelta: I256::ONE,
                amt1Limit: U256::ZERO,
            },
        }
        .abi_encode();
        assert!(replacement_calldata(&earlier, NOW).is_none());
        assert!(replacement_calldata(&[0x1d, 0x92], NOW).is_none());
    }

    /// Fees rise by at least an eighth, and to the urgency's current fees
    /// when those are higher.
    #[test]
    fn replacement_fees_rise_by_an_eighth_or_to_the_current_fees() {
        assert_eq!(replacement_fees(800, 80, None), (901, 91));
        let current = GasFees {
            base_fee: 1_000,
            max_priority_fee_per_gas: 200,
            max_fee_per_gas: 2_200,
            updated_at_ms: 0,
        };
        assert_eq!(replacement_fees(800, 80, Some(current)), (2_200, 200));
    }

    #[tokio::test]
    async fn v1_writes_refuse_earlier_and_unknown_builds_before_other_requests() {
        for (chain, chain_id, other_chain) in [
            (
                ChainDeployments::ARBITRUM,
                ARBITRUM_CHAIN_ID,
                ChainDeployments::ARBITRUM_SEPOLIA,
            ),
            (
                ChainDeployments::ARBITRUM_SEPOLIA,
                ARBITRUM_SEPOLIA_CHAIN_ID,
                ChainDeployments::ARBITRUM,
            ),
        ] {
            for (era, hook) in [
                (Some(Era::Legacy), Address::ZERO),
                (Some(Era::Upgradeable), chain.v022_perp_guard_hook),
                (None, Address::repeat_byte(0xff)),
                (None, other_chain.v022_perp_guard_hook),
                (None, other_chain.v1_perp_guard_hook),
            ] {
                for operation in [
                    "open_taker_v1",
                    "adjust_taker_v1",
                    "close_taker_v1",
                    "replace_v1_taker",
                ] {
                    let (client, rpc) = mock::client_on(chain, chain_id);
                    if let Some(era) = era {
                        rpc.immutables_with_hooks(
                            era,
                            chain.v022_perp_guard_hook,
                            chain.v1_perp_guard_hook,
                        );
                    } else {
                        rpc.call::<EarlierPerp::poolKeyCall>(&mock::pool_key_with_hook(30, hook));
                    }
                    let fee = MaxTotalFee::ForVolume(UsdcAtoms::new(1));
                    let adjust = adjust(-1);
                    let err = match operation {
                        "open_taker_v1" => client
                            .open_taker_v1(&open(), fee, Urgency::Normal)
                            .await
                            .unwrap_err(),
                        "adjust_taker_v1" => client
                            .adjust_taker_v1(&adjust, fee, true, Urgency::Normal)
                            .await
                            .unwrap_err(),
                        "close_taker_v1" => client
                            .close_taker_v1(adjust.pos_id, None, fee, Urgency::Normal)
                            .await
                            .unwrap_err(),
                        _ => client
                            .replace_v1_taker(B256::ZERO, Urgency::Normal)
                            .await
                            .unwrap_err(),
                    };
                    match era {
                        Some(expected) => assert!(
                            matches!(err,
                            PerpCityError::Contract(ContractError::UnsupportedEra { era, operation: named })
                                if era == expected && named == operation),
                            "{err}"
                        ),
                        None => assert!(
                            matches!(err,
                            PerpCityError::Contract(ContractError::UnknownBuild { hook: named, .. })
                                if named == hook),
                            "{err}"
                        ),
                    }
                    assert!(rpc.is_drained());
                }
            }
        }
    }

    #[tokio::test]
    async fn the_fee_module_and_both_rates_use_one_block_hash() {
        let (client, rpc) = mock::client();
        rpc.immutables(Era::V1);
        rpc.quantity(92);
        let hash = rpc.block(92, NOW);
        rpc.raw(&mock::returns::<EarlierPerp::modulesCall>(&mock::modules()));
        rpc.aggregate(
            92,
            [
                mock::returns::<v1::IFees::maxSwapFeeCall>(&U256::from(8_600)),
                mock::returns::<v1::Perp::protocolFeeCall>(&U256::from(100)),
            ],
        );
        let rates = client.market().get_taker_fee_rates().await.unwrap();
        assert_eq!(rates.max_swap_fee.e6(), 8_600);
        assert_eq!(rates.protocol_fee.e6(), 100);
        assert_eq!(rpc.call_blocks()[2..], [BlockId::hash(hash); 2]);
        assert!(rpc.is_drained());
    }

    async fn pending_trade(client: &PerpClient, input: Vec<u8>) -> (B256, Transaction) {
        let envelope = TransactionRequest::default()
            .with_to(client.market().perp())
            .with_input(Bytes::from(input))
            .with_nonce(12)
            .with_gas_limit(200_000)
            .with_max_fee_per_gas(800)
            .with_max_priority_fee_per_gas(80)
            .with_chain_id(client.chain().chain_id())
            .build(&client.wallet)
            .await
            .unwrap();
        let hash = *envelope.tx_hash();
        (
            hash,
            Transaction {
                inner: Recovered::new_unchecked(envelope, client.address()),
                block_hash: None,
                block_number: None,
                transaction_index: None,
                effective_gas_price: None,
            },
        )
    }

    fn mined_trade_receipt(
        client: &PerpClient,
        hash: B256,
        event: &impl SolEvent,
    ) -> TransactionReceipt {
        let mut log = mined_event_log(event, client.market().perp(), 92, 0, Some(NOW));
        log.transaction_hash = Some(hash);
        let receipt = Receipt {
            status: true.into(),
            cumulative_gas_used: 123_000,
            logs: vec![log],
        };
        TransactionReceipt {
            inner: ReceiptEnvelope::Eip1559(receipt.with_bloom()),
            transaction_hash: hash,
            transaction_index: Some(0),
            block_hash: Some(B256::with_last_byte(1)),
            block_number: Some(92),
            gas_used: 123_000,
            effective_gas_price: 80,
            blob_gas_used: None,
            blob_gas_price: None,
            from: client.address(),
            to: Some(client.market().perp()),
            contract_address: None,
        }
    }

    #[tokio::test]
    async fn an_original_open_that_mines_before_broadcast_is_returned() {
        let (client, rpc) = mock::client();
        let open = open_call(
            client.address(),
            &open(),
            UsdcAtoms::new(177_076),
            NOW + 600,
        );
        let (hash, tx) = pending_trade(&client, open.abi_encode()).await;
        let event = v1::Perp::TakerOpened {
            posId: U256::from(2),
            owner: client.address(),
            margin: 10_000_000,
            sr: zero_swap(),
            lastCumlFundingX96: I256::ZERO,
        };
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.raw(&[]);
        rpc.fails("nonce too low");
        rpc.response(&None::<TransactionReceipt>);
        rpc.response(&mined_trade_receipt(&client, hash, &event));
        let result = client
            .replace_v1_taker(hash, Urgency::Normal)
            .await
            .unwrap();
        let V1TakerReplacement::Opened(opened) = result else {
            panic!("expected the original open: {result:?}");
        };
        assert_eq!(opened.tx_hash, hash);
        assert_eq!(opened.event, event);
        assert!(rpc.is_drained());
    }

    #[tokio::test]
    async fn an_original_close_that_mines_before_preflight_is_returned() {
        let (client, rpc) = mock::client();
        let call = adjust_call(&adjust(-400_000), UsdcAtoms::new(1_000), true, NOW + 600);
        let (hash, tx) = pending_trade(&client, call.abi_encode()).await;
        let event = v1::Perp::TakerClosed {
            posId: call.params.posId,
            owner: client.address(),
            marginDelta: 0,
            sr: zero_swap(),
            funding: I256::ZERO,
            utilFees: U256::ZERO,
            delta: I256::ZERO,
            margin: 0,
            lastCumlFundingX96: I256::ZERO,
        };
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.fails("execution reverted");
        rpc.response(&mined_trade_receipt(&client, hash, &event));
        let result = client
            .replace_v1_taker(hash, Urgency::Normal)
            .await
            .unwrap();
        let V1TakerReplacement::Adjustment(V1TakerAdjustment::Closed(closed)) = result else {
            panic!("expected the original close: {result:?}");
        };
        assert_eq!(closed.tx_hash, hash);
        assert_eq!(closed.event, event);
        assert!(rpc.is_drained());
    }

    #[tokio::test(start_paused = true)]
    async fn uncertain_broadcast_with_no_receipts_reports_both_hashes() {
        let (client, rpc) = mock::client();
        let call = open_call(
            client.address(),
            &open(),
            UsdcAtoms::new(177_076),
            NOW + 600,
        );
        let (hash, tx) = pending_trade(&client, call.abi_encode()).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.raw(&[]);
        rpc.fails("nonce too low");
        for _ in 0..=RECEIPT_TIMEOUT.as_secs() / RECEIPT_POLL_INTERVAL.as_secs() {
            rpc.response(&None::<TransactionReceipt>);
            rpc.response(&None::<TransactionReceipt>);
        }
        let err = client
            .replace_v1_taker(hash, Urgency::Normal)
            .await
            .unwrap_err();
        assert!(err.is_unconfirmed());
        assert!(err.is_transient());
        let replacement_hash = err.tx_hash().unwrap();
        assert_ne!(replacement_hash, hash);
        assert!(
            matches!(err, PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
            original, replacement: Some(replacement), source: Some(_),
        }) if original == hash && replacement == replacement_hash)
        );
        assert!(rpc.is_drained());
    }

    #[tokio::test]
    async fn a_pending_trade_at_the_max_deadline_is_refused() {
        let (client, rpc) = mock::client();
        let call = open_call(client.address(), &open(), UsdcAtoms::new(177_076), u64::MAX);
        let (hash, tx) = pending_trade(&client, call.abi_encode()).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        let err = client
            .replace_v1_taker(hash, Urgency::Normal)
            .await
            .unwrap_err();
        assert!(
            matches!(err, PerpCityError::Transaction(TransactionError::NotReplaceable {
            tx_hash, reason: "its deadline cannot advance",
        }) if tx_hash == hash)
        );
        assert!(rpc.is_drained());
    }

    fn zero_swap() -> v1::SwapResult {
        v1::SwapResult {
            delta: I256::ZERO,
            ammPrice: U256::ZERO,
            totalFeeAmt: I256::ZERO,
            lpFeeAmt: U256::ZERO,
            protocolFeeAmt: U256::ZERO,
            creatorFeeAmt: U256::ZERO,
            insuranceFeeAmt: U256::ZERO,
            feeFundFeeAmt: U256::ZERO,
        }
    }

    #[tokio::test]
    async fn a_close_delta_outside_int128_is_refused() {
        let (client, rpc) = mock::client();
        let pos_id = U256::from(2);
        rpc.immutables(Era::V1);
        rpc.aggregate3(vec![
            mock::ok_row(mock::returns::<v1::Perp::positionsCall>(&v1::Position {
                delta: pack_balance_delta(i128::MIN, 1),
                margin: 1,
                lastCumlFundingX96: I256::ZERO,
            })),
            mock::ok_row(mock::returns::<v1::Perp::positionOwnerCall>(
                &client.address(),
            )),
        ]);
        let err = client
            .close_taker_v1(
                pos_id,
                None,
                MaxTotalFee::Exact(UsdcAtoms::new(1)),
                Urgency::Normal,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            PerpCityError::Validation(ValidationError::Overflow { .. })
        ));
        assert!(rpc.is_drained());
    }

    /// The flip refusal: a reduce-only adjustment that would take a long
    /// through zero to a short is refused after the position read, before
    /// the fee read and the send.
    #[tokio::test]
    async fn a_reduce_only_flip_is_refused_before_the_send() {
        let (client, rpc) = mock::client();
        rpc.immutables(Era::V1);
        // `get_position` on v1: the row and `positionOwner` in one batch.
        rpc.aggregate3(vec![
            mock::ok_row(mock::returns::<v1::Perp::positionsCall>(&v1::Position {
                delta: pack_balance_delta(400_000, -20_140_658),
                margin: 9_970_290,
                lastCumlFundingX96: I256::ZERO,
            })),
            mock::ok_row(mock::returns::<v1::Perp::positionOwnerCall>(
                &client.address(),
            )),
        ]);
        let err = client
            .adjust_taker_v1(
                &adjust(-800_000),
                MaxTotalFee::ForVolume(UsdcAtoms::new(40_000_000)),
                true,
                Urgency::Normal,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Validation(ValidationError::ReduceOnlyViolated {
                    position: 400_000,
                    perp_delta: -800_000,
                })
            ),
            "{err}"
        );
        assert!(!err.is_transient());
        assert!(rpc.is_drained(), "no fee read, no estimate, no send");
    }
}
