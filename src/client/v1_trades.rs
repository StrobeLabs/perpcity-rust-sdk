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
    let closing = -position.atoms();
    let (lowest, highest) = if closing < 0 {
        (closing, 0)
    } else {
        (0, closing)
    };
    if (lowest..=highest).contains(&perp_delta.atoms()) {
        Ok(())
    } else {
        Err(ValidationError::ReduceOnlyViolated {
            position: position.atoms(),
            perp_delta: perp_delta.atoms(),
        })
    }
}

/// The calldata that replaces a stuck `v1.0.0` taker call: the same call
/// with a new deadline, `now_secs` plus [`V1_TRADE_DEADLINE`], and always
/// later than the stuck one's, so the signed bytes always differ. `None`
/// for any other call.
fn replacement_calldata(input: &[u8], now_secs: u64) -> Option<(Bytes, StuckCall)> {
    let renewed = |old: u64| deadline_after(now_secs).max(old.saturating_add(1));
    match input.get(..4)?.try_into().ok()? {
        Perp::openTakerCall::SELECTOR => {
            let mut call = Perp::openTakerCall::abi_decode(input).ok()?;
            call.params.deadline = renewed(call.params.deadline);
            let holder = call.params.holder;
            Some((call.abi_encode().into(), StuckCall::Open { holder }))
        }
        Perp::adjustTakerCall::SELECTOR => {
            let mut call = Perp::adjustTakerCall::abi_decode(input).ok()?;
            call.params.deadline = renewed(call.params.deadline);
            let pos_id = call.params.posId;
            Some((call.abi_encode().into(), StuckCall::Adjust { pos_id }))
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
    /// As [`Self::adjust_taker_v1`];
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
        let perp_delta = PerpDelta::new(-position.perp.atoms());
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
    /// now, fees at least an eighth higher, and a new signature. The stuck
    /// call's own bytes are never sent again, so a replacement cannot mine
    /// past its deadline as `DeadlineExpired`.
    ///
    /// Call it with the hash of a send that returned
    /// [`TransactionError::ReceiptTimeout`] or
    /// [`TransactionError::BroadcastFailed`]. When the stuck transaction has
    /// mined already, nothing is signed and its outcome is returned. Else
    /// the replacement is preflighted at the stuck one's gas limit, sent,
    /// and the first receipt of either hash is parsed.
    ///
    /// # Errors
    ///
    /// [`TransactionError::NotReplaceable`] when the node does not know the
    /// hash, when it is not this client's `openTaker` or `adjustTaker` on
    /// this market, when its send still waits for its receipt, or when it
    /// is not an EIP-1559 transaction; the preflight's decoded revert; the
    /// signing, broadcast and receipt errors of a send; the parse errors of
    /// [`Self::open_taker_v1`] and [`Self::adjust_taker_v1`].
    pub async fn replace_v1_taker(
        &self,
        stuck: B256,
        urgency: Urgency,
    ) -> Result<V1TakerReplacement> {
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
        let (calldata, call) = replacement_calldata(tx.input(), now_ms() / 1_000)
            .ok_or_else(|| refuse("not a v1.0.0 openTaker or adjustTaker"))?;
        if tx.block_number.is_some() {
            let receipt = self.poll_receipt(stuck).await?;
            return self.replacement_outcome(call, &receipt);
        }
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
        self.chain()
            .preflight_call(self.address, perp, &calldata, 0, Some(gas_limit))
            .await?;

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
        if let Err(source) = provider.send_tx_envelope(envelope).await {
            return Err(TransactionError::BroadcastFailed {
                tx_hash: replacement,
                source,
            }
            .into());
        }
        tracing::debug!(
            %stuck,
            %replacement,
            nonce = tx.nonce(),
            max_fee,
            priority,
            ?urgency,
            "v1 taker trade replaced"
        );
        let receipt = self.first_receipt(replacement, stuck).await?;
        self.replacement_outcome(call, &receipt)
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
    async fn first_receipt(&self, replacement: B256, stuck: B256) -> Result<TransactionReceipt> {
        let provider = self.chain().provider();
        let deadline = tokio::time::Instant::now() + RECEIPT_TIMEOUT;
        loop {
            for hash in [replacement, stuck] {
                if let Ok(Some(receipt)) = provider.get_transaction_receipt(hash).await {
                    return Ok(receipt);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(TransactionError::ReceiptTimeout {
                    tx_hash: replacement,
                    reason: format!(
                        "no receipt for the replacement or {stuck} after {}s",
                        RECEIPT_TIMEOUT.as_secs()
                    ),
                }
                .into());
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
    use alloy::primitives::{Address, I256, U256};
    use alloy::sol_types::SolCall;

    use super::{
        MaxTotalFee, StuckCall, V1_TRADE_DEADLINE, adjust_call, check_reduce_only, open_call,
        replacement_calldata, replacement_fees,
    };
    use crate::client::mock;
    use crate::client::queries::Era;
    use crate::client::trades::{ExactAdjustTakerParams, ExactOpenTakerParams};
    use crate::contracts::{Perp as EarlierPerp, v1};
    use crate::convert::pack_balance_delta;
    use crate::errors::{ContractError, PerpCityError, ValidationError};
    use crate::hft::gas::{GasFees, Urgency};
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
    }

    /// A replacement is the same call with a new deadline, now plus 600 s,
    /// and never earlier than one second after the stuck one's; any other
    /// call is not replaced.
    #[test]
    fn a_replacement_renews_only_the_deadline() {
        let stuck = open_call(HOLDER, &open(), UsdcAtoms::new(177_076), NOW + 600).abi_encode();
        let (bytes, call) = replacement_calldata(&stuck, NOW + 30).unwrap();
        assert_eq!(call, StuckCall::Open { holder: HOLDER });
        assert_ne!(bytes.as_ref(), stuck.as_slice());
        let mut expected = v1::Perp::openTakerCall::abi_decode(&stuck).unwrap();
        expected.params.deadline = NOW + 630;
        assert_eq!(bytes.as_ref(), expected.abi_encode().as_slice());

        // Signed again in the same second: the deadline still moves.
        let (same_second, _) = replacement_calldata(&stuck, NOW).unwrap();
        let renewed = v1::Perp::openTakerCall::abi_decode(&same_second).unwrap();
        assert_eq!(renewed.params.deadline, NOW + 601);

        let stuck = adjust_call(&adjust(-400_000), UsdcAtoms::new(1_000), true, NOW + 600);
        let (bytes, call) = replacement_calldata(&stuck.abi_encode(), NOW + 60).unwrap();
        assert_eq!(
            call,
            StuckCall::Adjust {
                pos_id: U256::from(2u8)
            }
        );
        let renewed = v1::Perp::adjustTakerCall::abi_decode(&bytes)
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

    /// No `v1.0.0` write goes to another build: each is refused after the
    /// era read, before any other read or send.
    #[tokio::test]
    async fn v1_writes_refuse_an_earlier_build_before_any_send() {
        for era in [Era::Legacy, Era::Upgradeable] {
            let (client, rpc) = mock::client();
            rpc.immutables(era);
            let err = client
                .open_taker_v1(
                    &open(),
                    MaxTotalFee::Exact(UsdcAtoms::new(1)),
                    Urgency::Normal,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(
                    err,
                    PerpCityError::Contract(ContractError::UnsupportedEra {
                        operation: "open_taker_v1",
                        ..
                    })
                ),
                "{err}"
            );
            assert!(rpc.is_drained(), "poolKey, pool id; nothing else");
        }
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
