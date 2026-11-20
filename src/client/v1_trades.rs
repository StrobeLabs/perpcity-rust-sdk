//! `v1.0.0` writes with deadlines and parsed receipts, and replacement of
//! pending writes. Taker writes also carry a fee bound.

mod maker;

pub use maker::{V1MakerAdjustment, V1MakerReplacement};

use std::time::Duration;

use alloy::consensus::transaction::SignerRecoverable;
use alloy::consensus::{Transaction as ConsensusTransaction, TxType};
use alloy::contract::Error as CallError;
use alloy::eips::eip2718::Encodable2718;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, Bytes, I256, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol_types::SolCall;
use alloy::transports::TransportError;
use serde::{Deserialize, Serialize};

use crate::constants::{RECEIPT_POLL_INTERVAL, RECEIPT_TIMEOUT};
use crate::contracts::v1::{AdjustTakerParams, OpenTakerParams, Perp};
use crate::errors::{
    ContractError, NotReplaceableReason, PerpCityError, Result, TransactionError, ValidationError,
};
use crate::events::receipt::{V1TradeResult, parse_v1_receipt, parse_v1_receipt_for};
use crate::hft::gas::{GasFees, Urgency, is_out_of_gas};
use crate::math::swap::MAX_TOTAL_FEE_SLACK;
use crate::transport::fault::{Fault, fault, is_execution_error, is_rejection};
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
#[non_exhaustive]
pub enum MaxTotalFee {
    /// The bound for a swap that moves at most this much USDC, from the
    /// market's rates read at the send
    /// ([`TakerFeeRates::max_total_fee`](crate::TakerFeeRates::max_total_fee)
    /// with [`MAX_TOTAL_FEE_SLACK`]). A buy's `amt1_limit` is such a
    /// volume. A sell's `amt1_limit` is a floor, not a ceiling, so size its
    /// volume from the quote's proceeds with a margin for the price moving
    /// up before the block (for example 1%).
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

/// The fields from a stuck call needed to parse its receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StuckCall {
    Open { holder: Address },
    Adjust { pos_id: U256 },
}

/// The call a stuck input carries, and its calldata with a renewed
/// deadline when that can advance.
type ReplacementCalldata = fn(&[u8], u64) -> Option<(StuckCall, Option<Bytes>)>;

/// What the receipts and the account nonce say about a nonce's
/// transactions.
#[derive(Debug)]
enum Settled {
    /// One of the hashes mined.
    Mined(Box<TransactionReceipt>),
    /// The account nonce passed the nonce, and every receipt read answered
    /// with no receipt.
    NonceUsed,
    /// Nothing at the nonce mined yet, or a read gave no answer.
    Pending,
}

/// The looks at the receipts after the account nonce is first seen past a
/// trade's, before a polling wait reports the nonce used: an endpoint can
/// serve the nonce before the receipt that advanced it. A single check
/// takes one.
const NONCE_USED_LOOKS: u32 = 3;

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

/// The reduce-only `adjustTaker` that closes `position` of `pos_id`: its
/// perp amount reversed to the atom, and `amt1_limit` or, for `None`, the
/// market limit of the closing swap (no proceeds for a sell, any cost for
/// a buy).
fn close_call(
    pos_id: U256,
    position: PerpDelta,
    amt1_limit: Option<UsdcAtoms>,
    max_total_fee: UsdcAtoms,
    deadline: u64,
) -> std::result::Result<Perp::adjustTakerCall, ValidationError> {
    let perp_delta = PerpDelta::new(i128::try_from(closing_delta(position)).map_err(|_| {
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
    Ok(adjust_call(&params, max_total_fee, true, deadline))
}

/// The later of the new deadline and one second after the original one.
fn renewed_deadline(old: u64, now_secs: u64) -> Option<u64> {
    old.checked_add(1)
        .map(|next| deadline_after(now_secs).max(next))
}

/// Identify the v1 taker call and renew its deadline when it can advance.
/// A mined call can still be parsed when its deadline cannot be renewed.
fn replacement_calldata(input: &[u8], now_secs: u64) -> Option<(StuckCall, Option<Bytes>)> {
    match input.get(..4)?.try_into().ok()? {
        Perp::openTakerCall::SELECTOR => {
            let mut call = Perp::openTakerCall::abi_decode(input).ok()?;
            let kind = StuckCall::Open {
                holder: call.params.holder,
            };
            let calldata = renewed_deadline(call.params.deadline, now_secs).map(|deadline| {
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
            let calldata = renewed_deadline(call.params.deadline, now_secs).map(|deadline| {
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
/// the urgency asks of the current base fee.
fn replacement_fees(max_fee: u128, priority: u128, current: GasFees) -> (u128, u128) {
    let bump = |fee: u128| fee.saturating_add(fee / 8).saturating_add(1);
    let max_fee = bump(max_fee).max(u128::from(current.max_fee_per_gas));
    let priority = bump(priority).max(u128::from(current.max_priority_fee_per_gas));
    (max_fee.max(priority), priority)
}

/// Whether a broadcast error is the node's explicit rejection of the
/// replacement on every attempt ([`is_rejection`]; the transport passes a
/// rejection on as the node's answer only then). `already known`, `nonce
/// too low`, an unknown wording and no answer are not: a node can hold
/// the replacement, or it mined.
fn is_clear_refusal(error: &TransportError) -> bool {
    error.as_error_resp().is_some_and(is_rejection)
}

/// An era read's failure as every `v1.0.0` write reports it. No answer
/// from the node ([`is_no_answer`]) is the node's and transient, as
/// [`PerpCityError::Rpc`]. An answer (an execution revert of `poolKey()`
/// or `poolId()`, or a refusal of the request such as `-32601`) stays as
/// it is, not transient.
fn era_read_error(error: PerpCityError) -> PerpCityError {
    match error {
        PerpCityError::Abi(CallError::TransportError(error)) if is_no_answer(&error) => {
            error.into()
        }
        error => error,
    }
}

/// Parts of an error answer's message that report no answer from the
/// node behind the endpoint: a timeout or a cancelled request, a gateway
/// with no node to ask, a node behind the block asked for, or a quota.
const NO_ANSWER_WORDINGS: [&str; 15] = [
    "timeout",
    "timed out",
    "deadline exceeded",
    "context canceled",
    "unexpected eof",
    "no backend",
    "unavailable",
    "bad gateway",
    "upstream",
    "overloaded",
    "connection",
    "block not found",
    "failed to get block",
    "quota",
    "limit exceeded",
];

/// Whether a failed read got no answer from the node.
///
/// An error answer is the node's answer first when it reports an
/// execution error ([`is_execution_error`]: the code `3`, an execution
/// wording, revert data) or a method the endpoint does not serve
/// (`-32601`), whatever else its message says. Else it is no
/// answer when it reports one: a wording in [`NO_ANSWER_WORDINGS`], a
/// bare `EOF`, a rate limit ([`Fault::Endpoint`]) or a bare internal
/// error (`-32603`), with or without empty revert data (`0x`). Any other
/// failure is no answer when it is the endpoint's ([`Fault::Endpoint`]).
fn is_no_answer(error: &TransportError) -> bool {
    let Some(resp) = error.as_error_resp() else {
        return fault(error) == Fault::Endpoint;
    };
    let message = resp.message.to_ascii_lowercase();
    if resp.code == -32_601 || is_execution_error(resp) {
        return false;
    }
    message.trim() == "eof"
        || NO_ANSWER_WORDINGS.iter().any(|w| message.contains(w))
        || fault(error) == Fault::Endpoint
        || resp.code == -32_603
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
    /// else; a failed era read as [`PerpCityError::Rpc`], transient, when
    /// the node gave no answer, and as its read error, not transient, when
    /// the node answered with a revert or a refusal;
    /// [`ValidationError::InvalidMargin`] below the opening minimum;
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
    /// in (`ReduceOnlyViolated`). Without it, the contract permits a flip.
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
        let max_total_fee = self.resolve_max_total_fee(max_total_fee).await?;
        let call = adjust_call(
            params,
            max_total_fee,
            reduce_only,
            deadline_after(now_ms() / 1_000),
        );
        let receipt = self.send_adjust_taker_v1(call, urgency).await?;
        taker_adjustment(&receipt, self.market.perp(), params.pos_id)
    }

    /// Close a taker on a `v1.0.0` market: its perp amount read now and
    /// reversed to the atom, with `reduce_only` set.
    ///
    /// The contract checks the delta against the position at the block the
    /// close mines in, and does not clamp it. A position that shrank after
    /// the read reverts `ReduceOnlyViolated`. A position that grew is only
    /// reduced, which gives [`TransactionError::TakerNotClosed`].
    ///
    /// `amt1_limit` is the directional USDC limit of the closing swap; `None`
    /// is a market close (any proceeds for a sell, any cost for a buy).
    ///
    /// # Errors
    ///
    /// As [`Self::adjust_taker_v1`]; [`ValidationError::Overflow`] when the
    /// closing delta exceeds the signed 128-bit amount range;
    /// [`TransactionError::SimulationReverted`] with `ReduceOnlyViolated`
    /// at the preflight when the position shrank after the read (a mined
    /// revert is [`TransactionError::Reverted`]);
    /// [`TransactionError::TakerNotClosed`], transient, when the close mined
    /// as an adjustment because the position grew after the read.
    pub async fn close_taker_v1(
        &self,
        pos_id: U256,
        amt1_limit: Option<UsdcAtoms>,
        max_total_fee: MaxTotalFee,
        urgency: Urgency,
    ) -> Result<V1TradeResult<Perp::TakerClosed>> {
        self.require_v1("close_taker_v1").await?;
        let position = self.market.get_position(pos_id).await?;
        let max_total_fee = self.resolve_max_total_fee(max_total_fee).await?;
        let call = close_call(
            pos_id,
            position.perp,
            amt1_limit,
            max_total_fee,
            deadline_after(now_ms() / 1_000),
        )?;
        let receipt = self.send_adjust_taker_v1(call, urgency).await?;
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
    /// checks both hashes and the account nonce, and parses the first
    /// receipt it finds; after the node's explicit rejection on every
    /// attempt of the transport it checks once. A nonce seen used with no
    /// receipt is looked at again before it is reported, because an
    /// endpoint can serve the nonce before the receipt.
    ///
    /// Like a send, it needs fresh gas data: a receipt timeout outlives the
    /// fee cache, so call
    /// [`ChainReader::refresh_gas`](crate::ChainReader::refresh_gas) first.
    ///
    /// The replacement is not recorded in the client's `TxPipeline`: it
    /// reuses a nonce the pipeline has already counted, and
    /// `TxPipeline::is_in_flight` does not see it.
    ///
    /// # To replace again
    ///
    /// First check that a retry can help:
    /// [`is_transient`](PerpCityError::is_transient), and
    /// [`revert_class`](PerpCityError::revert_class) for a preflight
    /// revert. Then give the hash that [`PerpCityError::hash_to_replace`]
    /// returns for the last error. For a stuck send `S` replaced by `R1`:
    /// after the node rejected `R1` on every attempt, it is `S`; after the
    /// node took `R1`, or may have, and no receipt came, it is `R1`, whose
    /// fees are the highest at the nonce. When the node says it does not
    /// know that hash ([`NotReplaceableReason::UnknownToNode`]), give the
    /// one before it. `None` means this error gives no next hash; see
    /// [`TransactionError::hash_to_replace`].
    ///
    /// # Errors
    ///
    /// The outcome is final when a transaction at the nonce mined and was
    /// read: `Ok`, and for its receipt [`TransactionError::OutOfGas`],
    /// [`TransactionError::Reverted`] and the parse errors of
    /// [`Self::open_taker_v1`] and [`Self::adjust_taker_v1`]. Gas was burned
    /// and nothing else at the nonce can land. Every other error leaves the
    /// outcome open and is
    /// [unconfirmed](crate::PerpCityError::is_unconfirmed), except
    /// [`NotReplaceableReason::WrongSender`]: read the position before a
    /// new send.
    ///
    /// [`TransactionError::ReplacementUnconfirmed`], carrying the stuck
    /// hash, its nonce, any replacement hash, whether the node rejected it
    /// and the cause, for a market of another or an unknown build (before
    /// the look-up, as [`ContractError::UnsupportedEra`] or
    /// [`ContractError::UnknownBuild`] in `source`), an RPC failure, a
    /// stale fee cache, a failed preflight or signing, or no receipt before
    /// the timeout. [`TransactionError::NotReplaceable`] with a
    /// [`NotReplaceableReason`] when the stuck transaction is not signed
    /// again. [`TransactionError::ReplacementNonceUsed`] when the account
    /// nonce advanced and no receipt was found for either hash.
    pub async fn replace_v1_taker(
        &self,
        stuck: B256,
        urgency: Urgency,
    ) -> Result<V1TakerReplacement> {
        let (call, receipt) = self
            .replace_v1_trade("replace_v1_taker", stuck, urgency, replacement_calldata)
            .await?;
        let perp = self.market.perp();
        match call {
            StuckCall::Open { holder } => parse_v1_receipt_for(&receipt, perp, None, Some(holder))
                .map(V1TakerReplacement::Opened),
            StuckCall::Adjust { pos_id } => {
                taker_adjustment(&receipt, perp, pos_id).map(V1TakerReplacement::Adjustment)
            }
        }
    }

    /// Replace the stuck `v1.0.0` trade `stuck` whose call `decode` reads,
    /// and return the call with the successful receipt of whichever
    /// transaction at its nonce mined.
    async fn replace_v1_trade(
        &self,
        operation: &'static str,
        stuck: B256,
        urgency: Urgency,
        decode: ReplacementCalldata,
    ) -> Result<(StuckCall, TransactionReceipt)> {
        let refuse = |reason| -> PerpCityError {
            TransactionError::NotReplaceable {
                tx_hash: stuck,
                reason,
            }
            .into()
        };
        let unconfirmed = |nonce, source: PerpCityError| -> PerpCityError {
            TransactionError::ReplacementUnconfirmed {
                original: stuck,
                nonce,
                replacement: None,
                refused: false,
                source: Some(Box::new(source)),
            }
            .into()
        };
        match self.market.era().await {
            Ok(Era::V1) => {}
            Ok(era) => {
                return Err(unconfirmed(
                    None,
                    ContractError::UnsupportedEra { era, operation }.into(),
                ));
            }
            Err(error) => return Err(unconfirmed(None, era_read_error(error))),
        }
        let perp = self.market.perp();
        let provider = self.chain().provider();
        let tx = match provider.get_transaction_by_hash(stuck).await {
            Ok(Some(tx)) => tx,
            Ok(None) => return Err(refuse(NotReplaceableReason::UnknownToNode)),
            Err(error) => return Err(unconfirmed(None, error.into())),
        };
        // The node's answer is checked, not trusted: its signed fields must
        // hash to the hash asked for, its signature must recover to this
        // client, whose key is about to sign its call again, and it must be
        // signed for this client's chain.
        let signed = tx.inner.inner();
        if keccak256(signed.encoded_2718()) != stuck {
            return Err(refuse(NotReplaceableReason::UnknownToNode));
        }
        if signed.recover_signer().ok() != Some(self.address) {
            return Err(refuse(NotReplaceableReason::WrongSender));
        }
        let chain_id = ConsensusTransaction::chain_id(signed);
        if chain_id != Some(self.chain().chain_id()) {
            return Err(refuse(NotReplaceableReason::OtherChain { chain_id }));
        }
        if tx.to() != Some(perp) {
            return Err(refuse(NotReplaceableReason::OtherMarket));
        }
        let (call, calldata) = decode(tx.input(), now_ms() / 1_000)
            .filter(|_| tx.value().is_zero())
            .ok_or_else(|| refuse(NotReplaceableReason::UnsupportedCall))?;
        let nonce = tx.nonce();
        let gas_limit = tx.gas_limit();
        let selector: [u8; 4] = tx.input()[..4]
            .try_into()
            .expect("a decoded call has a selector");
        if tx.block_number.is_some() {
            let receipt = self
                .poll_receipt(stuck)
                .await
                .map_err(|error| unconfirmed(Some(nonce), error))?;
            return self.mined(call, receipt, gas_limit, selector);
        }
        if self.pipeline.lock().unwrap().is_in_flight(&stuck.0) {
            return Err(refuse(NotReplaceableReason::InFlight));
        }
        let tx_type = tx.inner.tx_type();
        let old_priority = match (tx_type, tx.max_priority_fee_per_gas()) {
            (TxType::Eip1559, Some(priority)) => priority,
            _ => {
                return Err(refuse(NotReplaceableReason::NotEip1559 {
                    tx_type: tx_type as u8,
                }));
            }
        };
        let calldata =
            calldata.ok_or_else(|| refuse(NotReplaceableReason::DeadlineCannotAdvance))?;
        let current = self
            .chain()
            .fee_cache()
            .lock()
            .unwrap()
            .fees_for(urgency, now_ms())
            .ok_or_else(|| {
                unconfirmed(
                    Some(nonce),
                    TransactionError::GasUnavailable {
                        reason: "gas cache stale or empty".into(),
                    }
                    .into(),
                )
            })?;
        let (max_fee, priority) = replacement_fees(
            ConsensusTransaction::max_fee_per_gas(&tx),
            old_priority,
            current,
        );
        if let Err(source) = self
            .chain()
            .preflight_call(self.address, perp, &calldata, 0, Some(gas_limit))
            .await
        {
            return match self.first_receipt(&[stuck], nonce, false).await {
                Settled::Mined(receipt) => self.mined(call, *receipt, gas_limit, selector),
                Settled::NonceUsed => Err(TransactionError::ReplacementNonceUsed {
                    original: stuck,
                    nonce,
                    replacement: None,
                    refused: false,
                }
                .into()),
                Settled::Pending => Err(unconfirmed(Some(nonce), source.into())),
            };
        }

        let request = TransactionRequest::default()
            .with_to(perp)
            .with_input(calldata)
            .with_value(U256::ZERO)
            .with_nonce(nonce)
            .with_gas_limit(gas_limit)
            .with_max_fee_per_gas(max_fee)
            .with_max_priority_fee_per_gas(priority)
            .with_chain_id(self.chain().chain_id());
        let envelope = request.build(&self.wallet).await.map_err(|e| {
            unconfirmed(
                Some(nonce),
                TransactionError::SigningFailed {
                    reason: format!("{e}"),
                }
                .into(),
            )
        })?;
        let replacement = *envelope.tx_hash();
        let broadcast_error = provider.send_tx_envelope(envelope).await.err();
        let refused = broadcast_error.as_ref().is_some_and(is_clear_refusal);
        tracing::debug!(
            %stuck,
            %replacement,
            nonce,
            max_fee,
            priority,
            ?urgency,
            broadcast_acknowledged = broadcast_error.is_none(),
            refused,
            "sent v1 trade replacement"
        );
        match self
            .first_receipt(&[replacement, stuck], nonce, !refused)
            .await
        {
            Settled::Mined(receipt) => self.mined(call, *receipt, gas_limit, selector),
            Settled::NonceUsed => Err(TransactionError::ReplacementNonceUsed {
                original: stuck,
                nonce,
                replacement: Some(replacement),
                refused,
            }
            .into()),
            Settled::Pending => Err(TransactionError::ReplacementUnconfirmed {
                original: stuck,
                nonce: Some(nonce),
                replacement: Some(replacement),
                refused,
                source: broadcast_error.map(|source| {
                    Box::new(
                        TransactionError::BroadcastFailed {
                            tx_hash: replacement,
                            source,
                        }
                        .into(),
                    )
                }),
            }
            .into()),
        }
    }

    /// Refuse anything but a `v1.0.0` market, after its era read.
    async fn require_v1(&self, operation: &'static str) -> Result<()> {
        match self.market.era().await.map_err(era_read_error)? {
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
        call: Perp::adjustTakerCall,
        urgency: Urgency,
    ) -> Result<TransactionReceipt> {
        let params = &call.params;
        tracing::debug!(
            pos_id = %params.posId,
            margin_delta_atoms = params.marginDelta,
            perp_delta = %params.perpDelta,
            amt1_limit = %params.amt1Limit,
            max_total_fee = %params.maxTotalFee,
            reduce_only = params.reduceOnly,
            deadline = params.deadline,
            ?urgency,
            "adjusting v1 taker position"
        );
        self.tx(self.market.perp(), call.abi_encode().into())
            .with_urgency(urgency)
            .send()
            .await
    }

    /// The first receipt of any of `hashes`, polled every
    /// [`RECEIPT_POLL_INTERVAL`] until [`RECEIPT_TIMEOUT`] when `poll`, else
    /// checked once. Once the nonce is seen used, the receipts get
    /// [`NONCE_USED_LOOKS`] more looks (one for a single check), whatever
    /// the timeout, and the last look's answer is the outcome.
    async fn first_receipt(&self, hashes: &[B256], nonce: u64, poll: bool) -> Settled {
        let deadline = tokio::time::Instant::now() + RECEIPT_TIMEOUT;
        let mut looks_left = None;
        loop {
            let settled = self.settled(hashes, nonce).await;
            if matches!(settled, Settled::Mined(_)) {
                return settled;
            }
            let left = match (looks_left, &settled) {
                (Some(left), _) => left,
                (None, Settled::NonceUsed) => {
                    if poll {
                        NONCE_USED_LOOKS
                    } else {
                        1
                    }
                }
                _ => {
                    if !poll || tokio::time::Instant::now() >= deadline {
                        return settled;
                    }
                    tokio::time::sleep(RECEIPT_POLL_INTERVAL).await;
                    continue;
                }
            };
            if left == 0 {
                return settled;
            }
            looks_left = Some(left - 1);
            tokio::time::sleep(RECEIPT_POLL_INTERVAL).await;
        }
    }

    /// One look at the account nonce, then at the receipts of `hashes`. The
    /// nonce is read first, so a nonce used before receipt reads that all
    /// answered "none" means none of `hashes` mined by that endpoint's
    /// view. A failed read, or a receipt of another hash than the one
    /// asked for, is no answer: the look is pending.
    async fn settled(&self, hashes: &[B256], nonce: u64) -> Settled {
        let provider = self.chain().provider();
        let used = provider
            .get_transaction_count(self.address)
            .latest()
            .await
            .is_ok_and(|count| count > nonce);
        let mut answered = true;
        for hash in hashes {
            match provider.get_transaction_receipt(*hash).await {
                Ok(Some(receipt)) if receipt.transaction_hash == *hash => {
                    return Settled::Mined(Box::new(receipt));
                }
                Ok(None) => {}
                Ok(Some(_)) | Err(_) => answered = false,
            }
        }
        if used && answered {
            Settled::NonceUsed
        } else {
            Settled::Pending
        }
    }

    /// A replaced trade's mined `receipt`, classified as a send's: out of
    /// gas at `gas_limit` (the cached estimate for `selector` evicted),
    /// reverted, or successful.
    fn mined(
        &self,
        call: StuckCall,
        receipt: TransactionReceipt,
        gas_limit: u64,
        selector: [u8; 4],
    ) -> Result<(StuckCall, TransactionReceipt)> {
        let tx_hash = receipt.transaction_hash;
        if receipt.status() {
            return Ok((call, receipt));
        }
        let gas_used = receipt.gas_used;
        if is_out_of_gas(gas_used, gas_limit) {
            self.gas_limit_cache.lock().unwrap().invalidate(&selector);
            tracing::warn!(%tx_hash, gas_used, gas_limit, "v1 trade replacement out of gas; cached estimate evicted");
            return Err(TransactionError::OutOfGas {
                tx_hash,
                gas_used,
                gas_limit,
            }
            .into());
        }
        Err(TransactionError::Reverted {
            tx_hash,
            reason: format!("transaction {tx_hash} reverted"),
        }
        .into())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use alloy::consensus::Transaction as ConsensusTransaction;
    use alloy::consensus::transaction::Recovered;
    use alloy::consensus::{Receipt, ReceiptEnvelope};
    use alloy::eips::BlockId;
    use alloy::hex;
    use alloy::network::{EthereumWallet, TransactionBuilder};
    use alloy::primitives::{Address, B256, Bytes, I256, U256, keccak256};
    use alloy::providers::RootProvider;
    use alloy::providers::bindings::IMulticall3 as Multicall;
    use alloy::rpc::client::RpcClient;
    use alloy::rpc::json_rpc::ErrorPayload;
    use alloy::rpc::types::{Log, Transaction, TransactionReceipt, TransactionRequest};
    use alloy::signers::Signer;
    use alloy::signers::local::PrivateKeySigner;
    use alloy::sol_types::{SolCall, SolError, SolEvent};
    use alloy::transports::{BoxTransport, TransportError, TransportErrorKind};
    use serde_json::value::RawValue;

    use super::{
        MaxTotalFee, NONCE_USED_LOOKS, StuckCall, V1_TRADE_DEADLINE, V1TakerAdjustment,
        V1TakerReplacement, adjust_call, check_reduce_only, close_call, is_clear_refusal,
        is_no_answer, open_call, replacement_calldata, replacement_fees,
    };
    use crate::client::queries::Era;
    use crate::client::trades::{ExactAdjustTakerParams, ExactOpenTakerParams};
    use crate::client::{
        ARBITRUM_CHAIN_ID, ARBITRUM_SEPOLIA_CHAIN_ID, ChainDeployments, PerpClient, mock, now_ms,
    };
    use crate::constants::{RECEIPT_POLL_INTERVAL, RECEIPT_TIMEOUT};
    use crate::contracts::{OpenTakerParams as EarlierOpenTakerParams, Perp as EarlierPerp, v1};
    use crate::convert::pack_balance_delta;
    use crate::errors::{
        ContractError, NotReplaceableReason, PerpCityError, RevertClass, TransactionError,
        ValidationError,
    };
    use crate::hft::gas::{GasFees, Urgency};
    use crate::hft::pipeline::TxRequest;
    use crate::history::test_support::mined_event_log;
    use crate::transport::config::WriteRetryConfig;
    use crate::transport::scripted::{self, Answer};
    use crate::units::{PerpDelta, UsdcAtoms, UsdcDelta};
    use crate::{HftTransport, TransportConfig};

    const HOLDER: Address = Address::repeat_byte(0x77);
    pub(super) const NOW: u64 = 1_763_600_000;

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

        let shown = |position, perp_delta| {
            ValidationError::ReduceOnlyViolated {
                position,
                perp_delta,
            }
            .to_string()
        };
        assert_eq!(
            shown(400_000, 1),
            "reduce-only change 1 is outside [-400000, 0] for position 400000"
        );
        assert_eq!(
            shown(-250_000, -1),
            "reduce-only change -1 is outside [0, 250000] for position -250000"
        );
        assert!(shown(i128::MIN, -1).contains("[0, 170141183460469231731687303715884105728]"));
        assert!(shown(0, 1).contains("[0, 0]"));
    }

    /// The close of either side: the perp amount reversed to the atom,
    /// reduce-only, and the market limit of the swap's direction for
    /// `None` (no proceeds for a long's sell, any cost for a short's buy).
    #[test]
    fn a_close_reverses_either_side_with_its_market_limit() {
        let pos_id = U256::from(2u8);
        let fee = UsdcAtoms::new(1_000);
        for (position, delta, market_limit) in [
            (400_000, -400_000, U256::ZERO),
            (-250_000, 250_000, U256::from(u128::MAX)),
        ] {
            let call = close_call(pos_id, PerpDelta::new(position), None, fee, NOW).unwrap();
            let params = v1::Perp::adjustTakerCall::abi_decode(&call.abi_encode())
                .unwrap()
                .params;
            assert_eq!(params.posId, pos_id);
            assert_eq!(
                params.perpDelta,
                I256::try_from(delta).unwrap(),
                "{position}"
            );
            assert_eq!(params.amt1Limit, market_limit, "{position}");
            assert!(params.reduceOnly, "{position}");
            assert_eq!(params.marginDelta, 0);
            assert_eq!(params.maxTotalFee, U256::from(1_000u64));
            assert_eq!(params.deadline, NOW);

            let limited = close_call(
                pos_id,
                PerpDelta::new(position),
                Some(UsdcAtoms::new(7)),
                fee,
                NOW,
            )
            .unwrap();
            assert_eq!(limited.params.amt1Limit, U256::from(7u8));
            assert!(limited.params.reduceOnly);
        }
        assert!(matches!(
            close_call(pos_id, PerpDelta::new(i128::MIN), None, fee, NOW),
            Err(ValidationError::Overflow { .. })
        ));
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
            params: EarlierOpenTakerParams {
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
        let low = GasFees {
            base_fee: 1,
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 2,
            updated_at_ms: 0,
        };
        assert_eq!(replacement_fees(800, 80, low), (901, 91));
        let current = GasFees {
            base_fee: 1_000,
            max_priority_fee_per_gas: 200,
            max_fee_per_gas: 2_200,
            updated_at_ms: 0,
        };
        assert_eq!(replacement_fees(800, 80, current), (2_200, 200));
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
                        // The given hash is kept, unresolved, with the
                        // refusal as the cause.
                        _ => {
                            let stuck = B256::repeat_byte(0x5a);
                            let err = client
                                .replace_v1_taker(stuck, Urgency::Normal)
                                .await
                                .unwrap_err();
                            assert!(err.is_unconfirmed() && !err.is_transient(), "{err}");
                            assert_eq!(err.tx_hash(), Some(stuck));
                            assert_eq!(err.hash_to_replace(), None, "no replacement here");
                            match err {
                                PerpCityError::Transaction(
                                    TransactionError::ReplacementUnconfirmed {
                                        original,
                                        nonce: None,
                                        replacement: None,
                                        refused: false,
                                        source: Some(source),
                                    },
                                ) if original == stuck => *source,
                                other => panic!("expected the hash kept: {other}"),
                            }
                        }
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
        let calls = rpc.calls();
        assert_eq!(calls[2][..4], EarlierPerp::modulesCall::SELECTOR);
        let batch = Multicall::aggregateCall::abi_decode(&calls[3])
            .unwrap()
            .calls;
        let targets: Vec<_> = batch
            .iter()
            .map(|call| (call.target, call.callData[..4].to_vec()))
            .collect();
        assert_eq!(
            targets,
            [
                (mock::FEES, v1::IFees::maxSwapFeeCall::SELECTOR.to_vec()),
                (
                    client.market().perp(),
                    v1::Perp::protocolFeeCall::SELECTOR.to_vec()
                ),
            ],
            "the fee module's maxSwapFee and the market's own protocolFee"
        );
        assert!(rpc.is_drained());
    }

    pub(super) const STUCK_NONCE: u64 = 12;
    pub(super) const STUCK_GAS: u64 = 200_000;

    /// A signed EIP-1559 trade of `client` on its market, as the request
    /// for [`as_seen`].
    pub(super) fn trade_request(client: &PerpClient, input: Vec<u8>) -> TransactionRequest {
        TransactionRequest::default()
            .with_to(client.market().perp())
            .with_input(Bytes::from(input))
            .with_nonce(STUCK_NONCE)
            .with_gas_limit(STUCK_GAS)
            .with_max_fee_per_gas(800)
            .with_max_priority_fee_per_gas(80)
            .with_chain_id(client.chain().chain_id())
    }

    /// Another account's key.
    fn other_signer() -> PrivateKeySigner {
        PrivateKeySigner::from_bytes(&B256::repeat_byte(0x44)).unwrap()
    }

    /// `request` signed by `from`, the client or [`other_signer`], as the
    /// node returns it, pending or mined in `block`.
    pub(super) async fn as_seen(
        client: &PerpClient,
        request: TransactionRequest,
        from: Address,
        block: Option<u64>,
    ) -> (B256, Transaction) {
        let envelope = if from == client.address() {
            request.build(&client.wallet).await.unwrap()
        } else {
            assert_eq!(from, other_signer().address());
            request
                .build(&EthereumWallet::from(other_signer()))
                .await
                .unwrap()
        };
        let hash = *envelope.tx_hash();
        (
            hash,
            Transaction {
                inner: Recovered::new_unchecked(envelope, from),
                block_hash: block.map(|_| B256::with_last_byte(1)),
                block_number: block,
                transaction_index: block.map(|_| 0),
                effective_gas_price: None,
            },
        )
    }

    pub(super) async fn pending_trade(client: &PerpClient, input: Vec<u8>) -> (B256, Transaction) {
        as_seen(client, trade_request(client, input), client.address(), None).await
    }

    fn stuck_open(client: &PerpClient, deadline: u64) -> v1::Perp::openTakerCall {
        open_call(client.address(), &open(), UsdcAtoms::new(177_076), deadline)
    }

    fn opened_event(client: &PerpClient) -> v1::Perp::TakerOpened {
        v1::Perp::TakerOpened {
            posId: U256::from(2),
            owner: client.address(),
            margin: 10_000_000,
            sr: zero_swap(),
            lastCumlFundingX96: I256::ZERO,
        }
    }

    pub(super) fn mined_trade_receipt(
        client: &PerpClient,
        hash: B256,
        event: &impl SolEvent,
    ) -> TransactionReceipt {
        let mut log = mined_event_log(event, client.market().perp(), 92, 0, Some(NOW));
        log.transaction_hash = Some(hash);
        receipt(client, hash, true, 123_000, vec![log])
    }

    pub(super) fn failed_receipt(
        client: &PerpClient,
        hash: B256,
        gas_used: u64,
    ) -> TransactionReceipt {
        receipt(client, hash, false, gas_used, Vec::new())
    }

    fn receipt(
        client: &PerpClient,
        hash: B256,
        status: bool,
        gas_used: u64,
        logs: Vec<Log>,
    ) -> TransactionReceipt {
        let receipt = Receipt {
            status: status.into(),
            cumulative_gas_used: gas_used,
            logs,
        };
        TransactionReceipt {
            inner: ReceiptEnvelope::Eip1559(receipt.with_bloom()),
            transaction_hash: hash,
            transaction_index: Some(0),
            block_hash: Some(B256::with_last_byte(1)),
            block_number: Some(92),
            gas_used,
            effective_gas_price: 80,
            blob_gas_used: None,
            blob_gas_price: None,
            from: client.address(),
            to: Some(client.market().perp()),
            contract_address: None,
        }
    }

    /// One look at the nonce and the receipts: the account nonce, then no
    /// receipt for each of `hashes`.
    pub(super) fn nothing_settled(rpc: &mock::Rpc, account_nonce: u64, hashes: usize) {
        rpc.quantity(account_nonce);
        for _ in 0..hashes {
            rpc.response(&None::<TransactionReceipt>);
        }
    }

    pub(super) fn refusal(err: &PerpCityError) -> NotReplaceableReason {
        match err {
            PerpCityError::Transaction(TransactionError::NotReplaceable { reason, .. }) => *reason,
            other => panic!("expected a refusal: {other}"),
        }
    }

    #[tokio::test]
    async fn an_original_open_that_mines_before_broadcast_is_returned() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (hash, tx) = pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
        let event = opened_event(&client);
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.raw(&[]);
        // The nonce is used: the first look finds the original.
        rpc.fails("nonce too low");
        rpc.quantity(STUCK_NONCE + 1);
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
        client.chain().set_base_fee(100);
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
        rpc.quantity(STUCK_NONCE + 1);
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
    async fn uncertain_broadcast_with_no_receipts_reports_both_hashes_and_the_nonce() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (hash, tx) = pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.raw(&[]);
        // Not a refusal: a resent copy can be what the node already has.
        rpc.fails("already known");
        for _ in 0..=RECEIPT_TIMEOUT.as_secs() / RECEIPT_POLL_INTERVAL.as_secs() {
            nothing_settled(&rpc, STUCK_NONCE, 2);
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
            original, nonce: Some(STUCK_NONCE), replacement: Some(replacement), refused: false, source: Some(_),
        }) if original == hash && replacement == replacement_hash)
        );
        assert_eq!(err.hash_to_replace(), Some(replacement_hash));
        assert!(rpc.is_drained());
    }

    /// A clear refusal of the replacement is checked once, not polled to
    /// the timeout.
    #[tokio::test]
    async fn a_refused_replacement_is_checked_once() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (hash, tx) = pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.raw(&[]);
        rpc.fails("replacement transaction underpriced");
        nothing_settled(&rpc, STUCK_NONCE, 2);
        let started = Instant::now();
        let err = client
            .replace_v1_taker(hash, Urgency::Normal)
            .await
            .unwrap_err();
        assert!(started.elapsed() < RECEIPT_POLL_INTERVAL);
        assert!(err.is_unconfirmed() && err.is_transient(), "{err}");
        assert!(matches!(
            err,
            PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
                original,
                replacement: Some(_),
                refused: true,
                source: Some(_),
                ..
            }) if original == hash
        ));
        assert_eq!(
            err.hash_to_replace(),
            Some(hash),
            "replace the original again"
        );
        assert_eq!(
            err.tx_hash(),
            Some(hash),
            "the refused replacement cannot land"
        );
        assert!(rpc.is_drained());
    }

    /// S, then R1 replacing it, then R2 replacing R1, and S mines: the
    /// look-ups of R2 and R1 find nothing, but the account nonce passed
    /// theirs, so after the first look and three more the outcome is the
    /// used nonce, not a timeout.
    #[tokio::test(start_paused = true)]
    async fn a_second_replacement_reports_the_nonce_an_earlier_signing_used() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (r1, tx) = pending_trade(&client, stuck_open(&client, NOW + 601).abi_encode()).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.raw(&[]);
        rpc.fails("already known");
        for _ in 0..=NONCE_USED_LOOKS {
            nothing_settled(&rpc, STUCK_NONCE + 1, 2);
        }
        let started = tokio::time::Instant::now();
        let err = client
            .replace_v1_taker(r1, Urgency::Normal)
            .await
            .unwrap_err();
        let r2 = err.tx_hash().unwrap();
        assert_ne!(r2, r1);
        assert!(
            matches!(err, PerpCityError::Transaction(TransactionError::ReplacementNonceUsed {
                original, nonce: STUCK_NONCE, replacement: Some(replacement), refused: false,
            }) if original == r1 && replacement == r2),
            "{err}"
        );
        assert_eq!(
            started.elapsed(),
            RECEIPT_POLL_INTERVAL * NONCE_USED_LOOKS,
            "three more looks, one poll interval apart"
        );
        assert!(err.is_unconfirmed(), "the trade's effect is unknown");
        assert!(!err.is_transient(), "neither hash can land");
        assert_eq!(err.hash_to_replace(), None);
        assert!(
            err.to_string()
                .ends_with("advanced, but no receipt was found for either hash"),
            "{err}"
        );
        assert!(rpc.is_drained());
    }

    /// An RPC failure of the era read or of the stuck transaction's look-up
    /// keeps the given hash and is transient.
    #[tokio::test]
    async fn rpc_failures_before_the_lookup_keep_the_original() {
        for failing_era in [true, false] {
            let (client, rpc) = mock::client();
            let hash = B256::repeat_byte(0x5a);
            if failing_era {
                rpc.fails("upstream timeout");
            } else {
                rpc.immutables(Era::V1);
                rpc.fails("upstream timeout");
            }
            let err = client
                .replace_v1_taker(hash, Urgency::Normal)
                .await
                .unwrap_err();
            assert!(
                matches!(err, PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
                    original, nonce: None, replacement: None, refused: false, source: Some(_),
                }) if original == hash),
                "{err}"
            );
            assert!(err.is_transient() && err.is_unconfirmed(), "{err}");
            assert_eq!(err.tx_hash(), Some(hash));
            assert!(rpc.is_drained());
        }
    }

    /// A preflight revert with no original receipt keeps the original's
    /// hash and nonce, and the revert's class, selector and transience.
    #[tokio::test(start_paused = true)]
    async fn a_preflight_revert_keeps_its_class_and_the_original() {
        for (selector, class) in [
            (v1::Perp::MaxAmtExceeded::SELECTOR, RevertClass::Answer),
            (v1::Perp::TvlCapExceeded::SELECTOR, RevertClass::Hold),
            (v1::Perp::DeadlineExpired::SELECTOR, RevertClass::Transient),
        ] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            let (hash, tx) =
                pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
            rpc.immutables(Era::V1);
            rpc.response(&tx);
            rpc.reverts(&selector);
            nothing_settled(&rpc, STUCK_NONCE, 1);
            let err = client
                .replace_v1_taker(hash, Urgency::Normal)
                .await
                .unwrap_err();
            assert!(
                matches!(&err, PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
                    original, nonce: Some(STUCK_NONCE), replacement: None, refused: false, source: Some(source),
                }) if *original == hash && source.is_simulation_revert()),
                "{err}"
            );
            assert_eq!(err.revert_class(), Some(class), "{err}");
            assert_eq!(err.is_transient(), class.is_transient(), "{err}");
            assert!(err.is_unconfirmed());
            let PerpCityError::Transaction(inner) = &err else {
                unreachable!()
            };
            assert_eq!(
                inner.is_revert::<v1::Perp::MaxAmtExceeded>(),
                selector == v1::Perp::MaxAmtExceeded::SELECTOR
            );
            assert!(rpc.is_drained());
        }

        // The same revert after the nonce was used by another transaction:
        // the original's receipt gets one more look.
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (hash, tx) = pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.reverts(&v1::Perp::MaxAmtExceeded::SELECTOR);
        nothing_settled(&rpc, STUCK_NONCE + 1, 1);
        nothing_settled(&rpc, STUCK_NONCE + 1, 1);
        let err = client
            .replace_v1_taker(hash, Urgency::Normal)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            PerpCityError::Transaction(TransactionError::ReplacementNonceUsed {
                original,
                nonce: STUCK_NONCE,
                replacement: None,
                refused: false,
            }) if original == hash
        ));
        assert!(rpc.is_drained());
    }

    /// Every refusal sends nothing: the node's answer to the look-up is
    /// the last request.
    #[tokio::test]
    async fn refusals_sign_nothing_and_say_whether_the_original_can_land() {
        let (client, _) = mock::client();
        let input = stuck_open(&client, NOW + 600).abi_encode();
        let other = other_signer().address();
        let earlier = EarlierPerp::openTakerCall {
            params: EarlierOpenTakerParams {
                holder: HOLDER,
                margin: 10_000_000,
                perpDelta: I256::ONE,
                amt1Limit: U256::ZERO,
            },
        }
        .abi_encode();
        let mut type_4 = trade_request(&client, input.clone());
        type_4.authorization_list = Some(Vec::new());
        let legacy = TransactionRequest::default()
            .with_to(client.market().perp())
            .with_input(Bytes::from(input.clone()))
            .with_nonce(STUCK_NONCE)
            .with_gas_limit(STUCK_GAS)
            .with_gas_price(800)
            .with_chain_id(client.chain().chain_id());
        let cases = [
            (
                trade_request(&client, input.clone()),
                other,
                NotReplaceableReason::WrongSender,
            ),
            (
                trade_request(&client, input.clone()).with_to(other),
                other,
                NotReplaceableReason::WrongSender,
            ),
            (
                trade_request(&client, input.clone()).with_to(other),
                client.address(),
                NotReplaceableReason::OtherMarket,
            ),
            (
                trade_request(&client, earlier),
                client.address(),
                NotReplaceableReason::UnsupportedCall,
            ),
            (
                type_4,
                client.address(),
                NotReplaceableReason::NotEip1559 { tx_type: 4 },
            ),
            (
                legacy,
                client.address(),
                NotReplaceableReason::NotEip1559 { tx_type: 0 },
            ),
            (
                trade_request(&client, stuck_open(&client, u64::MAX).abi_encode()),
                client.address(),
                NotReplaceableReason::DeadlineCannotAdvance,
            ),
        ];
        for (request, from, expected) in cases {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            let (hash, tx) = as_seen(&client, request, from, None).await;
            rpc.immutables(Era::V1);
            rpc.response(&tx);
            let err = client
                .replace_v1_taker(hash, Urgency::Normal)
                .await
                .unwrap_err();
            assert_eq!(refusal(&err), expected, "{err}");
            assert_eq!(err.tx_hash(), Some(hash));
            assert_eq!(err.hash_to_replace(), None);
            assert_eq!(
                err.is_unconfirmed(),
                expected != NotReplaceableReason::WrongSender,
                "only another sender's transaction cannot be this client's: {err}"
            );
            assert_eq!(err.is_unconfirmed(), expected.original_can_land(), "{err}");
            assert!(!err.is_transient());
            assert!(rpc.is_drained(), "{expected}: nothing after the look-up");
        }

        let (client, rpc) = mock::client();
        rpc.immutables(Era::V1);
        rpc.response(&None::<Transaction>);
        let err = client
            .replace_v1_taker(B256::repeat_byte(0x5a), Urgency::Normal)
            .await
            .unwrap_err();
        assert_eq!(refusal(&err), NotReplaceableReason::UnknownToNode);
        assert!(err.is_unconfirmed(), "another node can still mine it");
        assert!(rpc.is_drained());
    }

    /// The node answers the look-up with a transaction the client did not
    /// sign: another key's `openTaker` for that key's holder, with a large
    /// margin, and only the JSON `from` set to the client. Or it answers
    /// with a transaction of the client whose signed fields hash to another
    /// hash than the one asked for. Either is refused, and nothing is
    /// preflighted, signed or sent after the look-up.
    #[tokio::test]
    async fn a_lookup_answer_is_checked_against_the_signature_and_the_hash() {
        let (client, _) = mock::client();
        let attacker = other_signer();
        let forged = v1::Perp::openTakerCall {
            params: v1::OpenTakerParams {
                holder: attacker.address(),
                margin: 1_000_000_000_000,
                perpDelta: I256::ONE,
                amt1Limit: U256::MAX,
                maxTotalFee: U256::MAX,
                deadline: NOW + 600,
            },
        };
        let envelope = trade_request(&client, forged.abi_encode())
            .build(&EthereumWallet::from(attacker))
            .await
            .unwrap();
        let forged_hash = *envelope.tx_hash();
        let forged = Transaction {
            inner: Recovered::new_unchecked(envelope, client.address()),
            block_hash: None,
            block_number: None,
            transaction_index: None,
            effective_gas_price: None,
        };
        let (_, own) = pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
        let asked_other = B256::repeat_byte(0x5a);
        for (asked, answer, expected) in [
            (forged_hash, forged, NotReplaceableReason::WrongSender),
            (asked_other, own, NotReplaceableReason::UnknownToNode),
        ] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            rpc.immutables(Era::V1);
            rpc.response(&answer);
            let err = client
                .replace_v1_taker(asked, Urgency::Normal)
                .await
                .unwrap_err();
            assert_eq!(refusal(&err), expected, "{err}");
            assert_eq!(err.tx_hash(), Some(asked));
            assert_eq!(err.hash_to_replace(), None);
            assert_eq!(
                rpc.calls().len(),
                2,
                "{expected}: only the era read's calls"
            );
            assert!(rpc.sent().is_empty(), "{expected}: nothing signed or sent");
            assert!(rpc.is_drained(), "{expected}: nothing after the look-up");
        }
    }

    #[tokio::test]
    async fn a_trade_whose_send_still_waits_is_refused() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let input = stuck_open(&client, NOW + 600).abi_encode();
        let (hash, tx) = pending_trade(&client, input.clone()).await;
        let prepared = {
            let pipeline = client.pipeline.lock().unwrap();
            let fees = client.chain().fee_cache().lock().unwrap();
            pipeline
                .prepare(
                    TxRequest {
                        to: client.market().perp().into_array(),
                        calldata: input,
                        value: 0,
                        gas_limit: STUCK_GAS,
                        urgency: Urgency::Normal,
                    },
                    &fees,
                    now_ms(),
                )
                .unwrap()
        };
        client
            .pipeline
            .lock()
            .unwrap()
            .record_submission(hash.0, prepared, now_ms());
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        let err = client
            .replace_v1_taker(hash, Urgency::Normal)
            .await
            .unwrap_err();
        assert_eq!(refusal(&err), NotReplaceableReason::InFlight);
        assert!(err.is_unconfirmed());
        assert!(rpc.is_drained());
    }

    /// The fee cache is stale: refused before the preflight, as a send
    /// would be, with the original kept.
    #[tokio::test]
    async fn a_stale_fee_cache_refuses_before_the_preflight() {
        let (client, rpc) = mock::client();
        let (hash, tx) = pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        let err = client
            .replace_v1_taker(hash, Urgency::Normal)
            .await
            .unwrap_err();
        let PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
            original,
            nonce: Some(STUCK_NONCE),
            replacement: None,
            refused: false,
            source: Some(source),
        }) = &err
        else {
            panic!("expected the original kept: {err}");
        };
        assert_eq!(*original, hash);
        assert!(matches!(
            source.as_ref(),
            PerpCityError::Transaction(TransactionError::GasUnavailable { .. })
        ));
        assert!(err.is_transient());
        assert!(rpc.is_drained(), "no preflight, nothing sent");
    }

    /// A trade that mined before the look-up is parsed, nothing signed,
    /// even at the deadline that could not advance.
    #[tokio::test]
    async fn a_trade_mined_at_the_lookup_is_returned_even_at_the_max_deadline() {
        let (client, rpc) = mock::client();
        let input = stuck_open(&client, u64::MAX).abi_encode();
        let (hash, tx) = as_seen(
            &client,
            trade_request(&client, input),
            client.address(),
            Some(92),
        )
        .await;
        let event = opened_event(&client);
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.response(&mined_trade_receipt(&client, hash, &event));
        let result = client
            .replace_v1_taker(hash, Urgency::Normal)
            .await
            .unwrap();
        let V1TakerReplacement::Opened(opened) = result else {
            panic!("expected the mined open: {result:?}");
        };
        assert_eq!(opened.tx_hash, hash);
        assert!(rpc.is_drained());
    }

    /// A mined failure is a send's: reverted, or out of gas at the stuck
    /// limit with the cached estimate for the selector evicted.
    #[tokio::test]
    async fn a_mined_failure_is_reverted_or_out_of_gas() {
        let selector = v1::Perp::openTakerCall::SELECTOR;
        for (gas_used, out_of_gas) in [(50_000, false), (STUCK_GAS, true)] {
            let (client, rpc) = mock::client();
            client
                .gas_limit_cache
                .lock()
                .unwrap()
                .put(selector, 150_000, now_ms());
            let input = stuck_open(&client, NOW + 600).abi_encode();
            let (hash, tx) = as_seen(
                &client,
                trade_request(&client, input),
                client.address(),
                Some(92),
            )
            .await;
            rpc.immutables(Era::V1);
            rpc.response(&tx);
            rpc.response(&failed_receipt(&client, hash, gas_used));
            let err = client
                .replace_v1_taker(hash, Urgency::Normal)
                .await
                .unwrap_err();
            let cached = client
                .gas_limit_cache
                .lock()
                .unwrap()
                .get(&selector, now_ms());
            if out_of_gas {
                assert!(
                    matches!(err, PerpCityError::Transaction(TransactionError::OutOfGas {
                        tx_hash, gas_used: STUCK_GAS, gas_limit: STUCK_GAS,
                    }) if tx_hash == hash),
                    "{err}"
                );
                assert_eq!(cached, None, "the estimate is evicted");
            } else {
                assert!(
                    matches!(err, PerpCityError::Transaction(TransactionError::Reverted { tx_hash, .. }) if tx_hash == hash),
                    "{err}"
                );
                assert!(cached.is_some(), "a revert keeps the estimate");
            }
            assert!(rpc.is_drained());
        }
    }

    /// The replacement's own receipt runs out of gas.
    #[tokio::test]
    async fn a_replacement_out_of_gas_is_out_of_gas() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (hash, tx) = pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.raw(&[]);
        rpc.fails("already known");
        rpc.quantity(STUCK_NONCE + 1);
        rpc.response(&failed_receipt(&client, mock::SENT, STUCK_GAS));
        let err = client
            .replace_v1_taker(hash, Urgency::Normal)
            .await
            .unwrap_err();
        let winner = rpc.sent()[0];
        assert!(
            matches!(err, PerpCityError::Transaction(TransactionError::OutOfGas { tx_hash, .. }) if tx_hash == winner),
            "{err}"
        );
        assert!(rpc.is_drained());
    }

    /// The nonce read is ahead of the receipt reads (another endpoint), or
    /// one receipt read fails. The replacement mined, and the next poll
    /// finds its receipt: no used nonce is reported on the first look.
    #[tokio::test(start_paused = true)]
    async fn a_nonce_ahead_of_the_receipts_waits_for_the_receipt() {
        for failing_receipt_read in [false, true] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            let (hash, tx) =
                pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
            let event = opened_event(&client);
            rpc.immutables(Era::V1);
            rpc.response(&tx);
            rpc.raw(&[]);
            rpc.fails("already known");
            // Poll 1: the nonce is used; the receipt reads are behind or fail.
            rpc.quantity(STUCK_NONCE + 1);
            if failing_receipt_read {
                rpc.fails("upstream timeout");
            } else {
                rpc.response(&None::<TransactionReceipt>);
            }
            rpc.response(&None::<TransactionReceipt>);
            // Poll 2: the replacement's receipt.
            rpc.quantity(STUCK_NONCE + 1);
            rpc.response(&mined_trade_receipt(&client, mock::SENT, &event));
            let result = client.replace_v1_taker(hash, Urgency::Normal).await;
            let Ok(V1TakerReplacement::Opened(opened)) = result else {
                panic!("failing_receipt_read={failing_receipt_read}: {result:?}");
            };
            let r1 = rpc.sent()[0];
            assert_eq!(opened.tx_hash, r1);
            assert_eq!(
                rpc.receipt_hashes(),
                [r1, hash, r1],
                "the replacement first"
            );
            assert!(rpc.is_drained());
        }
    }

    /// A failed receipt read is no answer: with the nonce used and a read
    /// failing on every look, no look counts towards a used nonce, so the
    /// poll runs to the timeout and the outcome stays unconfirmed and
    /// transient.
    #[tokio::test(start_paused = true)]
    async fn failed_receipt_reads_never_report_the_nonce_used() {
        for foreign in [false, true] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            let (hash, tx) =
                pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
            let other = B256::repeat_byte(0x0e);
            rpc.immutables(Era::V1);
            rpc.response(&tx);
            rpc.raw(&[]);
            rpc.fails("already known");
            for _ in 0..=RECEIPT_TIMEOUT.as_secs() / RECEIPT_POLL_INTERVAL.as_secs() {
                rpc.quantity(STUCK_NONCE + 1);
                rpc.response(&None::<TransactionReceipt>);
                if foreign {
                    rpc.foreign_receipt(&mined_trade_receipt(
                        &client,
                        other,
                        &opened_event(&client),
                    ));
                } else {
                    rpc.fails("upstream timeout");
                }
            }
            let started = tokio::time::Instant::now();
            let err = client
                .replace_v1_taker(hash, Urgency::Normal)
                .await
                .unwrap_err();
            assert!(
                matches!(
                    err,
                    PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
                        refused: false,
                        ..
                    })
                ),
                "foreign {foreign}: {err}"
            );
            assert!(err.is_transient() && err.is_unconfirmed(), "{err}");
            assert_eq!(started.elapsed(), RECEIPT_TIMEOUT, "foreign {foreign}");
            assert!(
                rpc.is_drained(),
                "foreign {foreign}: one look per poll interval, then the timeout"
            );
        }
    }

    /// After an explicit rejection, a used nonce gets one more look after
    /// a poll interval: the receipt it finds, or else the used nonce, which
    /// keeps the refusal and names the original, since the refused
    /// replacement cannot have used it. (S mined while R1 was refused.)
    #[tokio::test(start_paused = true)]
    async fn a_rejected_replacement_looks_once_more_at_a_used_nonce() {
        for found in [true, false] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            let (hash, tx) =
                pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
            let event = opened_event(&client);
            rpc.immutables(Era::V1);
            rpc.response(&tx);
            rpc.raw(&[]);
            rpc.fails("replacement transaction underpriced");
            nothing_settled(&rpc, STUCK_NONCE + 1, 2);
            rpc.quantity(STUCK_NONCE + 1);
            rpc.response(&None::<TransactionReceipt>);
            if found {
                rpc.response(&mined_trade_receipt(&client, hash, &event));
            } else {
                rpc.response(&None::<TransactionReceipt>);
            }
            let started = tokio::time::Instant::now();
            let result = client.replace_v1_taker(hash, Urgency::Normal).await;
            assert_eq!(started.elapsed(), RECEIPT_POLL_INTERVAL);
            match result {
                Ok(V1TakerReplacement::Opened(opened)) if found => {
                    assert_eq!(opened.tx_hash, hash);
                }
                Err(err) if !found => {
                    let r1 = rpc.sent()[0];
                    assert!(
                        matches!(err, PerpCityError::Transaction(TransactionError::ReplacementNonceUsed {
                            original,
                            nonce: STUCK_NONCE,
                            replacement: Some(replacement),
                            refused: true,
                        }) if original == hash && replacement == r1),
                        "{err:?}"
                    );
                    assert_eq!(err.tx_hash(), Some(hash));
                    assert_eq!(err.hash_to_replace(), None);
                    assert!(err.is_unconfirmed() && !err.is_transient());
                }
                other => panic!("found={found}: {other:?}"),
            }
            assert!(rpc.is_drained());
        }
    }

    /// Only an explicit rejection stops the wait after one check. An
    /// unknown answer, an upstream timeout and a used nonce can come after
    /// the node took the replacement, so the receipts are polled, and the
    /// replacement is the hash to replace next.
    #[tokio::test(start_paused = true)]
    async fn an_ambiguous_broadcast_answer_is_polled() {
        for answer in [
            "unknown error",
            "upstream timeout",
            "already known",
            "nonce too low",
        ] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            let (hash, tx) =
                pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
            rpc.immutables(Era::V1);
            rpc.response(&tx);
            rpc.raw(&[]);
            rpc.fails(answer);
            nothing_settled(&rpc, STUCK_NONCE, 2);
            let event = opened_event(&client);
            rpc.quantity(STUCK_NONCE + 1);
            rpc.response(&mined_trade_receipt(&client, mock::SENT, &event));
            let result = client.replace_v1_taker(hash, Urgency::Normal).await;
            let r1 = rpc.sent()[0];
            assert!(
                matches!(&result, Ok(V1TakerReplacement::Opened(opened)) if opened.tx_hash == r1),
                "{answer}: {result:?}"
            );
            assert!(rpc.is_drained(), "{answer}");
        }
        for (answer, refused) in [
            ("replacement transaction underpriced", true),
            ("Replacement Transaction Underpriced", true),
            ("transaction underpriced", true),
            ("nonce too low", false),
            ("insufficient funds for gas * price + value", true),
            ("max fee per gas less than block base fee", true),
            ("already known", false),
            ("unknown error", false),
            ("upstream timeout", false),
            ("Known transaction", false),
        ] {
            let error = TransportErrorKind::custom_str(answer);
            assert!(!is_clear_refusal(&error), "no response: {answer}");
            let response = TransportError::ErrorResp(ErrorPayload {
                code: -32000,
                message: answer.to_owned().into(),
                data: None,
            });
            assert_eq!(is_clear_refusal(&response), refused, "{answer}");
        }
    }

    /// One era-read rule for every `v1.0.0` write: no answer (a dropped
    /// connection, an upstream timeout) is the node's and transient; an
    /// answer (an execution revert of the pool key, a method the endpoint
    /// does not serve) is not. A replacement keeps the given hash either
    /// way.
    #[tokio::test]
    async fn the_era_read_tells_an_answer_from_no_answer() {
        let stuck = B256::repeat_byte(0x5a);
        for (failure, transient) in [
            ("connection", true),
            ("upstream timeout", true),
            ("no backends", true),
            ("revert", false),
            ("code 3", false),
            ("internal error with revert data", false),
            ("method not found", false),
        ] {
            for replacing in [false, true] {
                let (client, rpc) = mock::client();
                match failure {
                    // Nothing queued: the transport fails with no response.
                    "connection" => {}
                    "upstream timeout" => rpc.fails("upstream timeout"),
                    "revert" => rpc.fails("execution reverted"),
                    "code 3" => rpc.reverts(&[]),
                    "no backends" => rpc.answers_error(-32_011, "no backends available", None),
                    "internal error with revert data" => {
                        rpc.answers_error(-32_603, "internal error", Some(r#""0xbcffc83f""#));
                    }
                    _ => rpc.method_not_found(),
                }
                let case = format!("{failure}, replacing={replacing}");
                let (err, cause) = if replacing {
                    let err = client
                        .replace_v1_taker(stuck, Urgency::Normal)
                        .await
                        .unwrap_err();
                    let PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
                        original,
                        nonce: None,
                        source: Some(source),
                        ..
                    }) = &err
                    else {
                        panic!("{case}: {err}");
                    };
                    assert_eq!(*original, stuck);
                    assert!(err.is_unconfirmed(), "{case}");
                    assert_eq!(err.hash_to_replace(), Some(stuck), "{case}");
                    let cause = matches!(source.as_ref(), PerpCityError::Rpc(_));
                    (err, cause)
                } else {
                    let err = client
                        .open_taker_v1(
                            &open(),
                            MaxTotalFee::Exact(UsdcAtoms::new(1)),
                            Urgency::Normal,
                        )
                        .await
                        .unwrap_err();
                    let cause = matches!(err, PerpCityError::Rpc(_));
                    (err, cause)
                };
                assert_eq!(err.is_transient(), transient, "{case}: {err}");
                assert_eq!(cause, transient, "{case}: {err:?}");
                assert!(rpc.is_drained(), "{case}");
            }
        }
    }

    /// What counts as no answer from the node for an era read: an
    /// execution error, by code `3` or by its wording, and a method the
    /// endpoint does not serve are answers whatever else the message says;
    /// revert data of at least four bytes, under any code and under `data`
    /// or `originalError` at any depth, is the contract's answer, and a hex
    /// string under another key is not; a bare internal error, a rate
    /// limit, a gateway with no node to ask, a timeout and a quota are no
    /// answer, and empty or shorter revert data does not change that.
    #[test]
    fn no_answer_is_a_failure_of_the_node_never_its_answer() {
        const REVERT: &str = r#""0x08c379a0""#;
        const NESTED: &str = r#"{"message":"execution reverted","data":"0xbcffc83f"}"#;
        const ORIGINAL: &str =
            r#"{"originalError":{"code":3,"data":"0xbcffc83f","message":"execution reverted"}}"#;
        const EMPTY: &str = r#""0x""#;
        for (code, message, data, no_answer) in [
            (-32_000, "execution aborted (timeout = 5s)", None, true),
            (-32_002, "request timed out", None, true),
            (-32_000, "context deadline exceeded", None, true),
            (-32_000, "context canceled", None, true),
            (-32_000, "EOF", None, true),
            (-32_000, "unexpected EOF", None, true),
            (-32_000, "failed to get block", None, true),
            (-32_000, "block not found", None, true),
            (-32_001, "Exceeded the quota usage", None, true),
            (-32_016, "over rate limit", None, true),
            (-32_005, "limit exceeded", None, true),
            (-32_097, "rate limit exceeded", None, true),
            (-32_000, "gateway timeout", Some(EMPTY), true),
            (-32_603, "request timed out", Some(EMPTY), true),
            (-32_000, "service unavailable", Some(EMPTY), true),
            (
                3,
                "VM Exception while processing transaction: revert service unavailable",
                None,
                false,
            ),
            (3, "upstream timeout", None, false),
            (-32_000, "execution reverted: upstream timeout", None, false),
            (-32_603, "Internal JSON-RPC error.", Some(ORIGINAL), false),
            (
                -32_015,
                "VM execution error.",
                Some(r#""Reverted 0x08c379a0""#),
                false,
            ),
            (-32_015, "VM execution error.", Some(r#""revert""#), false),
            (-32_603, "internal error", Some(r#""0xABCDEF01""#), false),
            (-32_603, "internal error", Some(r#""0x08c379a0f""#), false),
            (-32_603, "internal error", Some(r#""0xabc""#), true),
            (-32_603, "internal error", Some(r#""0xABCDEF""#), true),
            (-32_000, "upstream timeout", Some(r#""0x08c379""#), true),
            (-32_000, "upstream timeout", Some(r#""0x08c379a""#), true),
            (
                -32_000,
                "block not found",
                Some(r#"{"blockNumber":"0x10"}"#),
                true,
            ),
            (
                -32_000,
                "block not found",
                Some(r#"{"block":"0x1234abcd"}"#),
                true,
            ),
            (
                -32_000,
                "failed to get block",
                Some(r#"{"blockHash":"0x08c379a008c379a0"}"#),
                true,
            ),
            (
                -32_000,
                "request timed out",
                Some(r#"{"requestId":"0xdeadbeef"}"#),
                true,
            ),
            (
                -32_000,
                "gateway timeout",
                Some(r#"{"errors":[{"data":"0x08c379a0"}]}"#),
                false,
            ),
            (
                -32_000,
                "upstream timeout",
                Some(r#"{"data":"0x","originalError":{"data":"0xbcffc83f"}}"#),
                false,
            ),
            (
                -32_000,
                "upstream timeout",
                Some(r#"["0x","0xbcffc83f"]"#),
                false,
            ),
            (
                -32_000,
                "upstream timeout",
                Some(r#"{"originalError":"0xbcffc83f"}"#),
                false,
            ),
            (429, "Too Many Requests", Some(EMPTY), true),
            (-32_016, "over rate limit", Some(EMPTY), true),
            (-32_000, "header not found", Some(EMPTY), true),
            (-32_603, "internal error", Some(EMPTY), true),
            (-32_603, "out of gas", None, false),
            (-32_603, "invalid opcode: INVALID", None, false),
            (-32_603, "stack limit reached 1024", None, false),
            (-32_603, "invalid jump destination", None, false),
            (-32_603, "VM execution error.", None, false),
            (
                -32_603,
                "VM Exception while processing transaction",
                None,
                false,
            ),
            (-32_000, "out of gas", None, false),
            (-32_000, "invalid opcode: INVALID", None, false),
            (-32_000, "invalid jump destination", None, false),
            (-32_000, "stack limit reached 1024 (1023)", None, false),
            (-32_601, "method eth_call is unavailable", None, false),
            (-32_601, "method unavailable on this plan", None, false),
            (-32_603, "upstream timeout", None, true),
            (-32_000, "request timed out", None, true),
            (-32_000, "upstream timeout", None, true),
            (-32_603, "internal error", None, true),
            (
                -32_603,
                "Internal error",
                Some(r#""not revert data""#),
                true,
            ),
            (-32_603, "internal error", Some("null"), true),
            (-32_603, "internal error", Some(r#""0xnot hex""#), true),
            (429, "Too Many Requests", None, true),
            (-32_000, "header not found", None, true),
            (-32_011, "no backends available for method", None, true),
            (-32_000, "Bad Gateway", None, true),
            (-32_000, "service unavailable", None, true),
            (-32_000, "upstream connect error", None, true),
            (-32_000, "server overloaded", None, true),
            (-32_000, "connection reset by peer", None, true),
            (-32_603, "execution reverted", None, false),
            (3, "execution reverted", None, false),
            (3, "execution reverted", Some(REVERT), false),
            (-32_603, "internal error", Some(REVERT), false),
            (-32_603, "internal error", Some(NESTED), false),
            (-32_000, "upstream timeout", Some(REVERT), false),
            (
                -32_011,
                "no backends available for method",
                Some(NESTED),
                false,
            ),
            (
                -32_601,
                "the method does not exist/is not available",
                None,
                false,
            ),
            (-32_602, "invalid params", None, false),
            (-32_000, "missing trie node", None, false),
        ] {
            let error = TransportError::ErrorResp(ErrorPayload {
                code,
                message: message.to_owned().into(),
                data: data.map(|data| RawValue::from_string(data.to_owned()).unwrap()),
            });
            assert_eq!(is_no_answer(&error), no_answer, "{code} {message} {data:?}");
        }
        assert!(is_no_answer(&TransportError::local_usage_str(
            "request timed out"
        )));
        assert!(is_no_answer(&TransportErrorKind::http_error(
            503,
            String::new()
        )));
        assert!(!is_no_answer(&TransportErrorKind::http_error(
            400,
            String::new()
        )));
    }

    /// A pending transaction of this client to this market that is not a
    /// taker call (a liquidation, a maker open) can still land, so its
    /// refusal is unconfirmed.
    #[tokio::test]
    async fn a_pending_own_call_of_another_kind_is_unconfirmed() {
        let (client, _) = mock::client();
        let liquidation = v1::Perp::liquidateTakerCall {
            posId: U256::from(2u8),
            liquidationFeeRecipient: client.address(),
            perpAmount: 400_000,
        }
        .abi_encode();
        let mut maker_open = v1::Perp::openMakerCall::SELECTOR.to_vec();
        maker_open.extend_from_slice(&[0; 64]);
        for input in [liquidation, maker_open] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            let (hash, tx) = pending_trade(&client, input).await;
            rpc.immutables(Era::V1);
            rpc.response(&tx);
            let err = client
                .replace_v1_taker(hash, Urgency::Normal)
                .await
                .unwrap_err();
            assert_eq!(refusal(&err), NotReplaceableReason::UnsupportedCall);
            assert!(err.is_unconfirmed(), "{err}");
            assert!(!err.is_transient());
            assert_eq!(err.tx_hash(), Some(hash));
            assert!(rpc.is_drained());
        }
    }

    /// A signer that fails: nothing is broadcast, and the original is kept
    /// with its nonce and the signing failure as the cause.
    #[tokio::test]
    async fn a_failing_signer_keeps_the_original() {
        let (mut client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (hash, tx) = pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
        client.wallet = EthereumWallet::from(mock::signer().with_chain_id(Some(1)));
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.raw(&[]);
        let err = client
            .replace_v1_taker(hash, Urgency::Normal)
            .await
            .unwrap_err();
        let PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
            original,
            nonce: Some(STUCK_NONCE),
            replacement: None,
            refused: false,
            source: Some(source),
        }) = &err
        else {
            panic!("expected the original kept: {err}");
        };
        assert_eq!(*original, hash);
        assert!(
            matches!(
                source.as_ref(),
                PerpCityError::Transaction(TransactionError::SigningFailed { .. })
            ),
            "{source:?}"
        );
        assert!(err.is_unconfirmed());
        assert!(!err.is_transient(), "the signer fails again");
        assert_eq!(err.hash_to_replace(), Some(hash));
        assert!(rpc.is_drained(), "nothing broadcast");
    }

    /// The nonce is seen used at the first look, and the replacement's
    /// receipt first appears at the third look after it: it is found,
    /// three poll intervals in, and each look reads the replacement first.
    #[tokio::test(start_paused = true)]
    async fn a_receipt_at_the_third_look_after_a_used_nonce_is_found() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (hash, tx) = pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
        let event = opened_event(&client);
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.raw(&[]);
        rpc.fails("already known");
        for _ in 0..3 {
            nothing_settled(&rpc, STUCK_NONCE + 1, 2);
        }
        rpc.quantity(STUCK_NONCE + 1);
        rpc.response(&mined_trade_receipt(&client, mock::SENT, &event));
        let started = tokio::time::Instant::now();
        let result = client.replace_v1_taker(hash, Urgency::Normal).await;
        let r1 = rpc.sent()[0];
        assert!(
            matches!(&result, Ok(V1TakerReplacement::Opened(opened)) if opened.tx_hash == r1),
            "{result:?}"
        );
        assert_eq!(started.elapsed(), RECEIPT_POLL_INTERVAL * 3);
        assert_eq!(rpc.receipt_hashes(), [r1, hash, r1, hash, r1, hash, r1]);
        assert!(rpc.is_drained());
    }

    /// The nonce is seen used at every look, but the last look's receipt
    /// read fails: that look is no answer, so the outcome is unconfirmed,
    /// not a used nonce.
    #[tokio::test(start_paused = true)]
    async fn a_failed_read_at_the_last_look_is_no_used_nonce() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (hash, tx) = pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.raw(&[]);
        rpc.fails("already known");
        for _ in 0..3 {
            nothing_settled(&rpc, STUCK_NONCE + 1, 2);
        }
        rpc.quantity(STUCK_NONCE + 1);
        rpc.response(&None::<TransactionReceipt>);
        rpc.fails("upstream timeout");
        let started = tokio::time::Instant::now();
        let err = tokio::time::timeout(
            RECEIPT_TIMEOUT * 2,
            client.replace_v1_taker(hash, Urgency::Normal),
        )
        .await
        .expect("the looks after a used nonce end")
        .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
                    refused: false,
                    ..
                })
            ),
            "{err}"
        );
        assert!(err.is_transient() && err.is_unconfirmed(), "{err}");
        assert_eq!(started.elapsed(), RECEIPT_POLL_INTERVAL * 3);
        assert!(rpc.is_drained());
    }

    /// `nonce too low` says the nonce is used, possibly by the replacement:
    /// the receipts are polled like any answer that is not a rejection.
    /// With reads that never see the nonce used, the poll runs to the
    /// timeout and the replacement is the hash to replace next; with the
    /// nonce seen used and no receipt, three more looks give the used
    /// nonce.
    #[tokio::test(start_paused = true)]
    async fn nonce_too_low_polls_the_receipts() {
        for nonce_seen_used in [false, true] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            let (hash, tx) =
                pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
            rpc.immutables(Era::V1);
            rpc.response(&tx);
            rpc.raw(&[]);
            rpc.fails("nonce too low");
            let (looks, account_nonce) = if nonce_seen_used {
                (4, STUCK_NONCE + 1)
            } else {
                (
                    RECEIPT_TIMEOUT.as_secs() / RECEIPT_POLL_INTERVAL.as_secs() + 1,
                    STUCK_NONCE,
                )
            };
            for _ in 0..looks {
                nothing_settled(&rpc, account_nonce, 2);
            }
            let started = tokio::time::Instant::now();
            let err = client
                .replace_v1_taker(hash, Urgency::Normal)
                .await
                .unwrap_err();
            let r1 = rpc.sent()[0];
            assert!(err.is_unconfirmed(), "{err}");
            if nonce_seen_used {
                assert!(
                    matches!(err, PerpCityError::Transaction(TransactionError::ReplacementNonceUsed {
                        original, replacement: Some(replacement), ..
                    }) if original == hash && replacement == r1),
                    "{err}"
                );
                assert_eq!(started.elapsed(), RECEIPT_POLL_INTERVAL * 3);
            } else {
                assert!(
                    matches!(err, PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
                        original, replacement: Some(replacement), refused: false, ..
                    }) if original == hash && replacement == r1),
                    "{err}"
                );
                assert!(err.is_transient());
                assert_eq!(err.tx_hash(), Some(r1));
                assert_eq!(err.hash_to_replace(), Some(r1));
                assert_eq!(started.elapsed(), RECEIPT_TIMEOUT);
            }
            assert!(rpc.is_drained(), "nonce_seen_used={nonce_seen_used}");
        }
    }

    /// Every look is pending until the timeout, and the nonce is first
    /// seen used at the look at the timeout, then read stale twice: the
    /// looks after the nonce was seen used count whatever they read, so
    /// the wait ends three intervals after the timeout.
    #[tokio::test(start_paused = true)]
    async fn the_looks_after_a_used_nonce_are_bounded() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (hash, tx) = pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.raw(&[]);
        rpc.fails("already known");
        for _ in 0..RECEIPT_TIMEOUT.as_secs() / RECEIPT_POLL_INTERVAL.as_secs() {
            nothing_settled(&rpc, STUCK_NONCE, 2);
        }
        for account_nonce in [STUCK_NONCE + 1, STUCK_NONCE, STUCK_NONCE, STUCK_NONCE + 1] {
            nothing_settled(&rpc, account_nonce, 2);
        }
        let started = tokio::time::Instant::now();
        let err = tokio::time::timeout(
            RECEIPT_TIMEOUT * 2,
            client.replace_v1_taker(hash, Urgency::Normal),
        )
        .await
        .expect("the looks after a used nonce end")
        .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Transaction(TransactionError::ReplacementNonceUsed { .. })
            ),
            "{err}"
        );
        assert_eq!(
            started.elapsed(),
            RECEIPT_TIMEOUT + RECEIPT_POLL_INTERVAL * 3
        );
        assert!(rpc.is_drained());
    }

    /// Through the real transport, which sends a write again after an
    /// attempt with no answer or with an answer that is not a rejection: a
    /// last `underpriced` after such an attempt is no refusal, and neither
    /// is `underpriced` as a bare error object with no id or as the result
    /// of a success, so the receipts are polled and the replacement's
    /// receipt is found at the second look. Only `underpriced` as the
    /// error answer under the send's id on every attempt is a refusal,
    /// checked once, with the original to replace next.
    #[tokio::test]
    async fn a_rejection_after_an_ambiguous_attempt_is_no_refusal() {
        const UNDERPRICED: &str = "replacement transaction underpriced";
        for (first, last) in [
            ("no answer", UNDERPRICED),
            ("already known", UNDERPRICED),
            (UNDERPRICED, UNDERPRICED),
            ("no answer", "bare error"),
            ("bare error", "bare error"),
            ("error result", "error result"),
        ] {
            let (setup, _) = mock::client();
            let (stuck, tx) =
                pending_trade(&setup, stuck_open(&setup, NOW + 600).abi_encode()).await;
            let tx = serde_json::to_value(&tx).unwrap();
            let receipt = serde_json::to_string(&mined_trade_receipt(
                &setup,
                mock::SENT,
                &opened_event(&setup),
            ))
            .unwrap();
            let sent = Arc::new(Mutex::new(Vec::<B256>::new()));
            let looks = Arc::new(AtomicUsize::new(0));
            let (sent_by_node, looks_by_node) = (Arc::clone(&sent), Arc::clone(&looks));
            let url = scripted::endpoint(move |method, params| {
                let hex_result =
                    |bytes: Vec<u8>| Answer::Result(hex::encode_prefixed(bytes).into());
                match method {
                    "eth_call" => {
                        let call = &params[0];
                        let input = call["input"].as_str().or(call["data"].as_str()).unwrap();
                        let selector = hex::decode(input).unwrap()[..4].to_vec();
                        if selector == EarlierPerp::poolKeyCall::SELECTOR {
                            let key = mock::pool_key_with_hook(30, mock::V1_HOOK);
                            hex_result(mock::returns::<EarlierPerp::poolKeyCall>(&key))
                        } else if selector == v1::Perp::poolIdCall::SELECTOR {
                            hex_result(mock::returns::<v1::Perp::poolIdCall>(&mock::POOL_ID))
                        } else {
                            hex_result(Vec::new())
                        }
                    }
                    "eth_getTransactionByHash" => Answer::Result(tx.clone()),
                    "eth_sendRawTransaction" => {
                        let raw = hex::decode(params[0].as_str().unwrap()).unwrap();
                        let mut sent = sent_by_node.lock().unwrap();
                        sent.push(keccak256(raw));
                        let underpriced =
                            serde_json::json!({ "code": -32_000, "message": UNDERPRICED });
                        match if sent.len() == 1 { first } else { last } {
                            "no answer" => Answer::Silence,
                            "bare error" => Answer::Body(underpriced),
                            "error result" => Answer::Result(underpriced),
                            answer => Answer::Error(-32_000, answer.into()),
                        }
                    }
                    "eth_getTransactionCount" => {
                        let look = looks_by_node.fetch_add(1, Ordering::SeqCst);
                        let nonce = if look == 0 {
                            STUCK_NONCE
                        } else {
                            STUCK_NONCE + 1
                        };
                        Answer::Result(format!("{nonce:#x}").into())
                    }
                    "eth_getTransactionReceipt" => {
                        let r1 = sent_by_node.lock().unwrap()[0];
                        let asked: B256 = serde_json::from_value(params[0].clone()).unwrap();
                        if looks_by_node.load(Ordering::SeqCst) > 1 && asked == r1 {
                            let receipt = receipt.replace(
                                &hex::encode_prefixed(mock::SENT),
                                &hex::encode_prefixed(r1),
                            );
                            Answer::Result(serde_json::from_str(&receipt).unwrap())
                        } else {
                            Answer::Result(serde_json::Value::Null)
                        }
                    }
                    other => Answer::Error(-32_601, format!("{other} is not served")),
                }
            })
            .await;
            let transport = HftTransport::new(
                TransportConfig::builder()
                    .shared_endpoint(&url)
                    .request_timeout(Duration::from_millis(300))
                    .write_retry(WriteRetryConfig {
                        max_retries: 1,
                        base_delay: Duration::from_millis(1),
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
            let client = mock::client_over(RootProvider::new(RpcClient::new(
                BoxTransport::new(transport),
                false,
            )));
            client.chain().set_base_fee(100);
            let result = client.replace_v1_taker(stuck, Urgency::Normal).await;
            let r1 = sent.lock().unwrap()[0];
            assert_eq!(
                sent.lock().unwrap().as_slice(),
                [r1, r1],
                "{first}, {last}: two attempts"
            );
            if (first, last) == (UNDERPRICED, UNDERPRICED) {
                let err = result.unwrap_err();
                assert!(
                    matches!(err, PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
                        replacement: Some(replacement), refused: true, ..
                    }) if replacement == r1),
                    "{err}"
                );
                assert_eq!(err.hash_to_replace(), Some(stuck));
                assert_eq!(err.tx_hash(), Some(stuck));
                assert_eq!(looks.load(Ordering::SeqCst), 1, "checked once");
            } else {
                assert!(
                    matches!(&result, Ok(V1TakerReplacement::Opened(opened)) if opened.tx_hash == r1),
                    "{first}, {last}: {result:?}"
                );
                assert_eq!(looks.load(Ordering::SeqCst), 2, "{first}, {last}");
            }
        }
    }

    /// The node answers the look-up with a transaction of the client, and
    /// sets its JSON `hash` field to the hash asked for. The hash is the
    /// signed fields' own, so the answer is refused and nothing is
    /// preflighted, signed or sent.
    #[tokio::test]
    async fn a_forged_json_hash_is_not_trusted() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (own_hash, own) =
            pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
        let asked = B256::repeat_byte(0x5a);
        let mut json = serde_json::to_value(&own).unwrap();
        assert_eq!(json["hash"], serde_json::to_value(own_hash).unwrap());
        json["hash"] = serde_json::to_value(asked).unwrap();
        rpc.immutables(Era::V1);
        rpc.response(&json);
        let err = client
            .replace_v1_taker(asked, Urgency::Normal)
            .await
            .unwrap_err();
        assert_eq!(refusal(&err), NotReplaceableReason::UnknownToNode, "{err}");
        assert_eq!(err.tx_hash(), Some(asked));
        assert!(rpc.sent().is_empty(), "nothing signed or sent");
        assert!(rpc.is_drained(), "nothing after the look-up");
    }

    /// The same key signs on two chains, for the same market address. A
    /// trade of this client signed for the other chain, pending or mined,
    /// or a legacy one signed for no chain, is refused before anything is
    /// preflighted, signed or sent: its call is not signed again on this
    /// client's chain.
    #[tokio::test]
    async fn a_trade_signed_for_another_chain_is_refused() {
        let (client, _) = mock::client();
        let chain = client.chain().chain_id();
        let input = || stuck_open(&client, NOW + 600).abi_encode();
        let no_chain = TransactionRequest::default()
            .with_to(client.market().perp())
            .with_input(Bytes::from(input()))
            .with_nonce(STUCK_NONCE)
            .with_gas_limit(STUCK_GAS)
            .with_gas_price(800);
        for (request, block, chain_id) in [
            (
                trade_request(&client, input()).with_chain_id(chain + 1),
                None,
                Some(chain + 1),
            ),
            (
                trade_request(&client, input()).with_chain_id(chain + 1),
                Some(92),
                Some(chain + 1),
            ),
            (
                trade_request(&client, input()).with_chain_id(ARBITRUM_SEPOLIA_CHAIN_ID),
                None,
                Some(ARBITRUM_SEPOLIA_CHAIN_ID),
            ),
            (no_chain, None, None),
        ] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            let (hash, tx) = as_seen(&client, request, client.address(), block).await;
            assert_eq!(ConsensusTransaction::chain_id(&tx), chain_id);
            rpc.immutables(Era::V1);
            rpc.response(&tx);
            let err = client
                .replace_v1_taker(hash, Urgency::Normal)
                .await
                .unwrap_err();
            assert_eq!(
                refusal(&err),
                NotReplaceableReason::OtherChain { chain_id },
                "{err}"
            );
            assert_eq!(err.tx_hash(), Some(hash));
            assert!(err.is_unconfirmed() && !err.is_transient());
            assert!(
                rpc.sent().is_empty(),
                "{chain_id:?}: nothing signed or sent"
            );
            assert!(rpc.is_drained(), "{chain_id:?}: nothing after the look-up");
        }
    }

    /// The replacement carries no value, so a taker call that carried
    /// value is not one it replaces: an open or an adjustment with value,
    /// pending or mined, is refused, and nothing is polled, preflighted,
    /// signed or sent.
    #[tokio::test]
    async fn a_taker_call_with_value_is_refused() {
        let (client, _) = mock::client();
        let calls = [
            stuck_open(&client, NOW + 600).abi_encode(),
            adjust_call(&adjust(-400_000), UsdcAtoms::new(1_000), true, NOW + 600).abi_encode(),
        ];
        for (input, block) in calls
            .into_iter()
            .flat_map(|input| [(input.clone(), None), (input, Some(92))])
        {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            let request = trade_request(&client, input).with_value(U256::from(1));
            let (hash, tx) = as_seen(&client, request, client.address(), block).await;
            rpc.immutables(Era::V1);
            rpc.response(&tx);
            let err = client
                .replace_v1_taker(hash, Urgency::Normal)
                .await
                .unwrap_err();
            assert_eq!(
                refusal(&err),
                NotReplaceableReason::UnsupportedCall,
                "{err}"
            );
            assert!(err.is_unconfirmed());
            assert!(rpc.sent().is_empty());
            assert!(rpc.is_drained());
        }
    }

    /// A node that answers a receipt read with the receipt of another
    /// hash gave no answer for the hash asked for. After a refused
    /// replacement, that look is pending, not the other trade's outcome,
    /// and gets no second look.
    #[tokio::test(start_paused = true)]
    async fn a_receipt_of_another_hash_is_no_answer() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (hash, tx) = pending_trade(&client, stuck_open(&client, NOW + 600).abi_encode()).await;
        let other = B256::repeat_byte(0x0e);
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.raw(&[]);
        rpc.fails("replacement transaction underpriced");
        rpc.quantity(STUCK_NONCE + 1);
        rpc.foreign_receipt(&mined_trade_receipt(&client, other, &opened_event(&client)));
        rpc.response(&None::<TransactionReceipt>);
        let started = tokio::time::Instant::now();
        let err = client
            .replace_v1_taker(hash, Urgency::Normal)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
                    original,
                    refused: true,
                    ..
                }) if original == hash
            ),
            "{err:?}"
        );
        assert_eq!(err.tx_hash(), Some(hash));
        assert_eq!(started.elapsed(), Duration::ZERO, "no second look");
        assert!(rpc.is_drained());
    }

    /// A trade the look-up shows mined: a receipt of another hash is a
    /// failed poll, and the next poll's own receipt is the outcome.
    #[tokio::test(start_paused = true)]
    async fn a_mined_trade_waits_for_its_own_receipt() {
        let (client, rpc) = mock::client();
        let (hash, tx) = as_seen(
            &client,
            trade_request(&client, stuck_open(&client, NOW + 600).abi_encode()),
            client.address(),
            Some(92),
        )
        .await;
        let event = opened_event(&client);
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.foreign_receipt(&mined_trade_receipt(
            &client,
            B256::repeat_byte(0x0e),
            &event,
        ));
        rpc.response(&mined_trade_receipt(&client, hash, &event));
        let started = tokio::time::Instant::now();
        let result = client.replace_v1_taker(hash, Urgency::Normal).await;
        assert!(
            matches!(&result, Ok(V1TakerReplacement::Opened(opened)) if opened.tx_hash == hash),
            "{result:?}"
        );
        assert_eq!(started.elapsed(), RECEIPT_POLL_INTERVAL);
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
