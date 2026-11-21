//! Transaction lifecycle errors.

use std::fmt;

use alloy::primitives::{FixedBytes, U256};
use alloy::sol_types::SolError;
use alloy::transports::TransportError;
use thiserror::Error;

use super::decode::{RevertClass, revert_class};
use super::{ContractError, PerpCityError};

/// Errors arising from the transaction lifecycle: simulation, signing,
/// broadcasting, receipt polling, and gas resolution.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum TransactionError {
    /// Pre-flight simulation (`eth_estimateGas` or `eth_call`) detected a
    /// contract revert. The transaction was **not** broadcast — no gas was
    /// burned. [`Self::revert_class`] says what the revert means for a
    /// retry ([`RevertClass`] has each case): transient only for the
    /// `v1.0.0` `StaleBeaconIndex` (it can pass after a beacon update) and
    /// `DeadlineExpired` (the call must be built again with a new deadline;
    /// the same calldata fails again); every other revert is the contract's
    /// answer to this call.
    #[error("simulation reverted: {error_name} ({selector})")]
    SimulationReverted {
        /// Human-readable error name decoded from the 4-byte selector
        /// (e.g. `"InvalidMarginRatio"`). Unknown selectors decode to
        /// `"UnknownContractError(0x…)"` with the selector preserved.
        error_name: String,
        /// The raw 4-byte selector (displays as `0x`-prefixed hex, e.g.
        /// `0xbcffc83f`); match it typed via [`Self::is_revert`].
        selector: FixedBytes<4>,
        /// Full revert data hex, if available.
        revert_data: Option<String>,
    },

    /// Transaction was broadcast and mined but reverted on-chain.
    /// Gas was burned. Not transient.
    #[error("transaction reverted: {reason}")]
    Reverted {
        /// Hash of the mined transaction.
        tx_hash: FixedBytes<32>,
        /// Human-readable description.
        reason: String,
    },

    /// Transaction was broadcast and mined but ran out of gas: execution
    /// consumed the limit and returned no revert data. Gas was burned.
    ///
    /// Distinct from [`Self::Reverted`] because the call was never disproved
    /// — only the limit it carried was too small. Arbitrum charges execution
    /// costs that `eth_estimateGas` does not model, so an estimate can be
    /// below what the same call consumes in a block, and `eth_call` does not
    /// reproduce the shortfall either: pre-flight passes and the limit is
    /// only disproved here.
    ///
    /// Not transient, so a backoff loop does not spin on it. The send path
    /// evicts the cached estimate before returning, so a caller-level retry
    /// re-estimates rather than repeating the same limit.
    #[error("transaction out of gas: {tx_hash} used {gas_used} of {gas_limit}")]
    OutOfGas {
        /// Hash of the mined transaction.
        tx_hash: FixedBytes<32>,
        /// Gas the transaction consumed.
        gas_used: u64,
        /// Limit it was broadcast with.
        gas_limit: u64,
    },

    /// A taker close mined, but the contract adjusted the position instead
    /// of closing it: the swap left a nonzero perp delta, so the position
    /// is still open and still holds its margin. Gas was burned.
    ///
    /// The contract closes a taker only when its remaining perp delta is
    /// exactly zero. [`PerpClient::close_taker`](crate::PerpClient::close_taker)
    /// reverses the delta it reads on-chain, so this means the position
    /// changed between that read and the block the close landed in.
    /// Transient: a retry reads the delta again.
    #[error("taker close left position {pos_id} open: {tx_hash}")]
    TakerNotClosed {
        /// Hash of the mined transaction.
        tx_hash: FixedBytes<32>,
        /// The position that is still open.
        pos_id: U256,
    },

    /// A `v1.0.0` maker close mined, but the contract adjusted the band
    /// instead of removing it: the band held more liquidity than the close
    /// read, so the position is still open. Gas was burned.
    /// [`PerpClient::close_maker_v1`](crate::PerpClient::close_maker_v1)
    /// removes the liquidity it reads, so the band changed between that
    /// read and the block the close landed in. Transient: a retry reads
    /// the liquidity again.
    #[error("maker close left position {pos_id} open: {tx_hash}")]
    MakerNotClosed {
        /// Hash of the mined transaction.
        tx_hash: FixedBytes<32>,
        /// The position that is still open.
        pos_id: U256,
    },

    /// Receipt polling timed out before the transaction was confirmed.
    ///
    /// The transaction was broadcast and may still mine, so the send path
    /// never reuses its nonce; the next send resyncs it from chain. Look up
    /// `tx_hash` later (for example with
    /// [`PerpClient::poll_receipt`](crate::PerpClient::poll_receipt)) to
    /// learn the outcome. Transient: the transaction may still land.
    #[error("receipt timeout for {tx_hash}: {reason}")]
    ReceiptTimeout {
        /// Hash of the broadcast transaction.
        tx_hash: FixedBytes<32>,
        /// Why polling stopped: no receipt by the deadline, or the last
        /// poll's RPC error.
        reason: String,
    },

    /// The broadcast request failed after the transaction was signed.
    ///
    /// The node may still have accepted it: the request can fail after the
    /// transaction reached the mempool, or after it mined. An error response
    /// is no proof either: the transport resends the same signed bytes after
    /// a timeout, so a rejection such as `nonce too low` or `already known`
    /// can answer a copy that already landed. `source` keeps the node's
    /// answer; look up `tx_hash` (for example with
    /// [`PerpClient::poll_receipt`](crate::PerpClient::poll_receipt)) to
    /// learn the outcome. The send path neither reuses nor releases the
    /// nonce; the next send resyncs it from chain.
    ///
    /// Transient, like the transport error it wraps.
    #[error("broadcast failed for {tx_hash}: {source}")]
    BroadcastFailed {
        /// Hash of the signed transaction.
        tx_hash: FixedBytes<32>,
        /// The transport error the broadcast returned.
        #[source]
        source: TransportError,
    },

    /// A replacement of a stuck `v1.0.0` trade did not establish which
    /// transaction at the nonce mined, so `original` can still land.
    /// Every failure of
    /// [`PerpClient::replace_v1_taker`](crate::PerpClient::replace_v1_taker)
    /// or [`PerpClient::replace_v1_maker`](crate::PerpClient::replace_v1_maker)
    /// after the call to it, other than a refusal, a parsed receipt or a
    /// used nonce, is this variant.
    ///
    /// A market of another build, refused before `original` is looked
    /// up, is this variant too, with the era error as `source`: nothing
    /// about `original` was read, and the method does not replace on that
    /// market.
    ///
    /// The transience, [`Self::revert_class`] and [`Self::is_revert`]
    /// follow `source`: a preflight revert such as `MaxAmtExceeded` keeps
    /// its class, an RPC failure or a receipt timeout is transient, and an
    /// era refusal is not. With no `source`, no receipt came before the
    /// timeout: transient. [`Self::hash_to_replace`] gives the hash to
    /// replace again.
    #[error(
        "v1 trade replacement unconfirmed for original {original} and replacement {replacement:?}"
    )]
    #[non_exhaustive]
    ReplacementUnconfirmed {
        /// Hash of the stuck transaction.
        original: FixedBytes<32>,
        /// The stuck transaction's nonce, when it was read. The outcome is
        /// known when the account nonce at the latest block is greater
        /// than this nonce: one transaction at it mined, possibly one
        /// signed before `original`.
        nonce: Option<u64>,
        /// Hash of the signed replacement, if broadcast was attempted.
        replacement: Option<FixedBytes<32>>,
        /// Whether the node refused `replacement` with an explicit
        /// rejection (an underpriced fee, a fee above a cap, missing
        /// funds) on every attempt the transport made, so `original` is
        /// still the transaction the node holds. A `nonce too low`, an
        /// `already known`, a wording the SDK does not know, or no answer
        /// on any attempt is not a refusal: a node can hold `replacement`.
        refused: bool,
        /// The failure that stopped the replacement, when one occurred.
        #[source]
        source: Option<Box<PerpCityError>>,
    },

    /// The account nonce passed `nonce`, and no receipt was found for
    /// either `original` or `replacement` in several looks, the last one
    /// [`RECEIPT_POLL_INTERVAL`](crate::constants::RECEIPT_POLL_INTERVAL)
    /// or more after the nonce was first seen used. This is evidence that
    /// the nonce advanced, not of which transaction used it: it can be an
    /// earlier signing that `original` replaced, or one of the two hashes
    /// whose receipt an endpoint did not serve. Neither hash can land now.
    /// The effect is unknown, so this is [unconfirmed](Self::is_unconfirmed):
    /// read the position before a new send. Not transient.
    #[error("nonce {nonce} of {original} advanced, but no receipt was found for either hash")]
    #[non_exhaustive]
    ReplacementNonceUsed {
        /// Hash of the stuck transaction.
        original: FixedBytes<32>,
        /// The nonce the two hashes share.
        nonce: u64,
        /// Hash of the signed replacement, if broadcast was attempted.
        replacement: Option<FixedBytes<32>>,
        /// Whether the node refused `replacement` on every attempt, as
        /// in [`Self::ReplacementUnconfirmed`]: then it cannot have used
        /// the nonce.
        refused: bool,
    },

    /// Transaction signing failed. Not transient; nothing left the process.
    #[error("signing failed: {reason}")]
    SigningFailed {
        /// The underlying signing error.
        reason: String,
    },

    /// Pre-flight simulation (`eth_estimateGas` or `eth_call`) failed with
    /// the node's definitive execution answer but no decodable contract
    /// revert: an empty revert (a selector the deployed contract does not
    /// have, a bare `revert()`), or execution running out of gas inside
    /// the pinned limit. The transaction was **not** broadcast.
    ///
    /// Deterministic for the same calldata and chain state, so — unlike
    /// [`Self::GasUnavailable`] — **not** transient: retrying reproduces
    /// it. (`PerpCityError::is_transient` says `false`.)
    #[error("simulation failed: {reason}")]
    SimulationFailed {
        /// The node's error response.
        reason: String,
    },

    /// A call sent with a fixed gas limit (the caller's, or the stuck
    /// transaction's in a replacement) failed in its preflight at that
    /// limit, as `error_name`, and succeeds without a cap in the same
    /// block: the limit is too low, and the failure is the gas running out
    /// (inside a token transfer, say). The transaction was **not**
    /// broadcast. Not transient: the same limit fails again.
    #[error("the call fails as {error_name} at gas limit {gas_limit} and succeeds without a cap")]
    #[non_exhaustive]
    GasLimitTooLow {
        /// The fixed gas limit.
        gas_limit: u64,
        /// The decoded revert at that limit, or `out of gas`.
        error_name: String,
    },

    /// Gas price or base fee is not available (cache stale, RPC down), or a
    /// pre-flight simulation could not reach the node (transport failure),
    /// or a `v1.0.0` trade's simulation in the next block was refused or
    /// found no head twice, or the call without the cap after a capped
    /// failure got no execution answer or ran out of gas too.
    /// Transient: the transaction was neither disproved nor broadcast.
    #[error("gas unavailable: {reason}")]
    GasUnavailable {
        /// Description of why gas data is unavailable.
        reason: String,
    },

    /// Too many unconfirmed transactions in the pipeline. Transient: the
    /// condition clears as in-flight transactions resolve, so a caller
    /// should back off until a receipt arrives rather than give up. Nothing
    /// was signed or sent.
    #[error("too many in-flight: {count} (max {max})")]
    TooManyInFlight {
        /// Current number of in-flight transactions.
        count: usize,
        /// Maximum allowed.
        max: usize,
    },

    /// The local nonce sequence no longer provably matches the chain — a
    /// broadcast failed or a receipt timed out, leaving a nonce's fate
    /// unknowable. Sends fail fast until in-flight work drains, then the
    /// next send resyncs from the chain automatically.
    ///
    /// Transient by design (see `PerpCityError::is_transient`): the
    /// condition clears itself, so callers should back off briefly and
    /// retry rather than treat this as a dead client.
    #[error("nonce desynced from chain ({in_flight} in flight); resyncs when drained")]
    NonceDesynced {
        /// Transactions still awaiting receipts, which block the resync.
        in_flight: usize,
    },

    /// A stuck transaction that
    /// [`PerpClient::replace_v1_taker`](crate::PerpClient::replace_v1_taker)
    /// or [`PerpClient::replace_v1_maker`](crate::PerpClient::replace_v1_maker)
    /// does not sign again, for `reason`. Nothing was signed or sent. Not
    /// transient. When [`NotReplaceableReason::original_can_land`] holds,
    /// `tx_hash` can still land and the error is
    /// [unconfirmed](Self::is_unconfirmed).
    #[error("cannot replace {tx_hash}: {reason}")]
    #[non_exhaustive]
    NotReplaceable {
        /// Hash of the stuck transaction.
        tx_hash: FixedBytes<32>,
        /// Why it is not replaced.
        reason: NotReplaceableReason,
    },
}

/// Why a stuck transaction is not signed again
/// ([`TransactionError::NotReplaceable`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NotReplaceableReason {
    /// The node does not know the hash, or answers with a transaction
    /// whose signed fields hash to another hash.
    UnknownToNode,
    /// Its signature does not recover to this client's address; the
    /// node's `from` field is not trusted.
    WrongSender,
    /// It is signed for another chain than this client's, or, as a legacy
    /// transaction, for no chain. This holds for a mined transaction too:
    /// a mined legacy call with no chain id is refused, and its mined
    /// outcome is not reported; read it with
    /// [`PerpClient::poll_receipt`](crate::PerpClient::poll_receipt).
    OtherChain {
        /// The chain id it is signed for.
        chain_id: Option<u64>,
    },
    /// It is from this client, to another address than this client's
    /// market: a call that another client or method sends.
    OtherMarket,
    /// It is from this client to this market, but its call is not one
    /// that the method replaces, such as a maker trade, a liquidation, or
    /// a taker call that carries value. This holds for a mined transaction
    /// too: a mined call with value is refused, and its mined outcome is
    /// not reported; read it with
    /// [`PerpClient::poll_receipt`](crate::PerpClient::poll_receipt).
    UnsupportedCall,
    /// Its own send still waits for its receipt.
    InFlight,
    /// It is pending with an envelope type other than EIP-1559 (`2`), such
    /// as an EIP-7702 transaction (`4`).
    NotEip1559 {
        /// The envelope type.
        tx_type: u8,
    },
    /// It is pending and its deadline is `u64::MAX`.
    DeadlineCannotAdvance,
}

impl NotReplaceableReason {
    /// Whether the transaction can be this client's, pending or mined with
    /// an effect the method did not read: every reason but
    /// [`Self::WrongSender`]. A call the method does not replace, or one to
    /// another market, still lands when it is pending.
    pub fn original_can_land(self) -> bool {
        !matches!(self, Self::WrongSender)
    }
}

impl fmt::Display for NotReplaceableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownToNode => f.write_str("the node does not know it"),
            Self::WrongSender => f.write_str("not signed by this client"),
            Self::OtherChain {
                chain_id: Some(chain_id),
            } => write!(f, "signed for chain {chain_id}"),
            Self::OtherChain { chain_id: None } => f.write_str("signed for no chain"),
            Self::OtherMarket => f.write_str("not to this client's market"),
            Self::UnsupportedCall => f.write_str("not a call this method replaces"),
            Self::InFlight => f.write_str("its send still waits for the receipt"),
            Self::NotEip1559 { tx_type } => write!(f, "type {tx_type} is not EIP-1559"),
            Self::DeadlineCannotAdvance => f.write_str("its deadline cannot advance"),
        }
    }
}

impl TransactionError {
    /// Whether a trade's effect is unknown: a send broadcast and not
    /// confirmed either way (`BroadcastFailed`, `ReceiptTimeout`,
    /// `ReplacementUnconfirmed`), a replacement whose nonce advanced with
    /// no receipt found (`ReplacementNonceUsed`), or a refused replacement
    /// whose original can be this client's (`NotReplaceable`, every
    /// [`NotReplaceableReason`] but `WrongSender`).
    ///
    /// Sending the same change again could make it twice. Read the state
    /// the send would have changed before acting.
    pub fn is_unconfirmed(&self) -> bool {
        match self {
            Self::BroadcastFailed { .. }
            | Self::ReceiptTimeout { .. }
            | Self::ReplacementUnconfirmed { .. }
            | Self::ReplacementNonceUsed { .. } => true,
            Self::NotReplaceable { reason, .. } => reason.original_can_land(),
            _ => false,
        }
    }

    /// Hash of the signed transaction, for every failure from the broadcast
    /// onward: `BroadcastFailed`, `ReceiptTimeout`, `Reverted`, `OutOfGas`,
    /// `TakerNotClosed` and `MakerNotClosed`. A replacement's error gives
    /// the replacement's hash, else the stuck one's: the stuck one's too
    /// when the node refused the replacement, which cannot land.
    /// [`Self::hash_to_replace`]
    /// gives the hash to replace again. `None` means nothing was sent.
    ///
    /// A `Some` hash may have landed on chain even when the error says the
    /// send failed, so look up its receipt before treating the effect as
    /// absent.
    pub fn tx_hash(&self) -> Option<FixedBytes<32>> {
        match self {
            Self::ReplacementUnconfirmed {
                original,
                replacement,
                refused,
                ..
            }
            | Self::ReplacementNonceUsed {
                original,
                replacement,
                refused,
                ..
            } => Some(match replacement {
                Some(replacement) if !refused => *replacement,
                _ => *original,
            }),
            Self::BroadcastFailed { tx_hash, .. }
            | Self::ReceiptTimeout { tx_hash, .. }
            | Self::Reverted { tx_hash, .. }
            | Self::OutOfGas { tx_hash, .. }
            | Self::TakerNotClosed { tx_hash, .. }
            | Self::MakerNotClosed { tx_hash, .. }
            | Self::NotReplaceable { tx_hash, .. } => Some(*tx_hash),
            _ => None,
        }
    }

    /// Which hash to give
    /// [`PerpClient::replace_v1_taker`](crate::PerpClient::replace_v1_taker)
    /// to replace the same trade again; not whether to. A caller checks
    /// that a retry can help first: [`PerpCityError::is_transient`], and
    /// [`Self::revert_class`] for a preflight revert. A cause that is not
    /// transient, such as a failed signing or a revert that is the
    /// contract's answer, fails the same way again.
    ///
    /// - `ReceiptTimeout` and `BroadcastFailed`: the send's hash.
    /// - `ReplacementUnconfirmed` with a replacement the node did not
    ///   refuse (accepted, or an answer such as `already known`, `nonce too
    ///   low` or a timeout on any attempt): the replacement, whose fees are
    ///   the highest at the nonce. If the node later says it does not know
    ///   that hash ([`NotReplaceableReason::UnknownToNode`]), give
    ///   `original`.
    /// - `ReplacementUnconfirmed` with no replacement, or one that the node
    ///   refused: `original`.
    ///
    /// `None` means this error gives no next hash: a `ReplacementUnconfirmed`
    /// whose `source` refuses the market's build
    /// ([`ContractError::UnsupportedEra`] or
    /// [`ContractError::UnknownBuild`]), a used nonce, a mined receipt, a
    /// refusal (`NotReplaceable`, which names the hash it was given) and
    /// every other error. After [`NotReplaceableReason::InFlight`], wait
    /// for the result of the send that is still active; after
    /// [`NotReplaceableReason::UnknownToNode`], keep the hashes you know
    /// and check their receipts or the position's state.
    pub fn hash_to_replace(&self) -> Option<FixedBytes<32>> {
        match self {
            Self::ReceiptTimeout { tx_hash, .. } | Self::BroadcastFailed { tx_hash, .. } => {
                Some(*tx_hash)
            }
            Self::ReplacementUnconfirmed {
                source: Some(source),
                ..
            } if matches!(
                source.as_ref(),
                PerpCityError::Contract(
                    ContractError::UnsupportedEra { .. } | ContractError::UnknownBuild { .. }
                )
            ) =>
            {
                None
            }
            Self::ReplacementUnconfirmed {
                original,
                replacement,
                refused,
                ..
            } => Some(match replacement {
                Some(replacement) if !refused => *replacement,
                _ => *original,
            }),
            _ => None,
        }
    }

    /// Whether this error is a [`Self::SimulationReverted`] carrying the
    /// typed contract error `E`, compared by 4-byte selector — no string
    /// matching; for a [`Self::ReplacementUnconfirmed`], whether its
    /// `source` is.
    ///
    /// ```rust,ignore
    /// use perpcity_sdk::Perp;
    ///
    /// if err.is_revert::<Perp::NotLiquidatable>() {
    ///     // healthy right now — retry later
    /// } else if err.is_revert::<Perp::NonMakerPosition>() {
    ///     // never liquidatable on this path — drop the id
    /// }
    /// ```
    pub fn is_revert<E: SolError>(&self) -> bool {
        match self {
            Self::SimulationReverted { selector, .. } => selector.0 == E::SELECTOR,
            Self::ReplacementUnconfirmed {
                source: Some(source),
                ..
            } => matches!(source.as_ref(), PerpCityError::Transaction(e) if e.is_revert::<E>()),
            _ => false,
        }
    }

    /// The [`RevertClass`] of a [`Self::SimulationReverted`], by its
    /// selector, and of a [`Self::ReplacementUnconfirmed`]'s `source`;
    /// `None` for every other variant (a mined revert carries no selector).
    ///
    /// ```rust,ignore
    /// use perpcity_sdk::RevertClass;
    ///
    /// match err.revert_class() {
    ///     Some(RevertClass::Transient) => {}   // build the call again: a new deadline, or after a beacon update
    ///     Some(RevertClass::BeaconFault) => {} // a trade: hold the market until its beacon is fixed;
    ///                                          // setBeacon or createPerp: the named beacon is bad
    ///     Some(RevertClass::Hold) => {}        // this add does not fit under the TVL cap; a smaller one can
    ///     _ => {}
    /// }
    /// ```
    pub fn revert_class(&self) -> Option<RevertClass> {
        match self {
            Self::SimulationReverted { selector, .. } => Some(revert_class(selector.0)),
            Self::ReplacementUnconfirmed {
                source: Some(source),
                ..
            } => source.revert_class(),
            _ => None,
        }
    }
}
