//! One market's reads over a [`ChainReader`], [`MarketReader`].

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;

use crate::contracts::Perp;
use crate::errors::{Result, ValidationError};
use crate::hft::gas::GasLimits;

use super::chain::ChainReader;

/// One market's reads: the `Perp` contract and the chain reader it lives
/// on.
///
/// Every read addressed to a market — its pool state, positions, capacity,
/// the contract's mark, the taker book, maker equities, the liquidation
/// probes — is here. Owned and cheap to clone (the chain reader is an
/// Arc), so it can be stored in a struct or moved into a task, and every
/// reader of one market over one chain reader shares that reader's caches.
/// It needs no signer.
#[derive(Clone, Debug)]
pub struct MarketReader {
    pub(super) chain: ChainReader,
    pub(super) perp: Address,
}

impl ChainReader {
    /// A reader for the market at `perp`.
    pub fn market(&self, perp: Address) -> MarketReader {
        MarketReader {
            chain: self.clone(),
            perp,
        }
    }
}

impl MarketReader {
    /// The chain reader this market is read through.
    pub fn chain(&self) -> &ChainReader {
        &self.chain
    }

    /// The market's `Perp` contract.
    pub fn perp(&self) -> Address {
        self.perp
    }

    // ── Liquidation probes ───────────────────────────────────────────

    /// Check whether a maker `pos_id` is liquidatable right now, via
    /// `eth_call` from `from` — the batch/scanner probe for the maker book.
    ///
    /// `Ok(())` means the liquidation would succeed. A typed revert says
    /// why not — triage it with
    /// [`TransactionError::is_revert`](crate::errors::TransactionError::is_revert):
    /// `Perp::NotLiquidatable` is a healthy position, `Perp::NonMakerPosition`
    /// the wrong book (route it to the taker twin). An empty revert or
    /// out-of-gas inside the pinned limit
    /// ([`SimulationFailed`](crate::errors::TransactionError::SimulationFailed))
    /// is the node's answer and not transient; a transport failure
    /// ([`GasUnavailable`](crate::errors::TransactionError::GasUnavailable))
    /// is.
    ///
    /// `from` is what the contract sees as `msg.sender`, and it matters: on
    /// the deployed contracts a liquidation whose margin was consumed by
    /// bad debt charges the sender the shortfall, so a probe from an
    /// unfunded address reverts `TransferFromFailed` where the funded,
    /// approved sender's transaction would succeed. Probe from the address
    /// that will send. The simulation is capped at
    /// [`GasLimits::LIQUIDATE`], exactly like the send.
    pub async fn simulate_liquidate_maker(
        &self,
        from: Address,
        pos_id: U256,
        fee_recipient: Address,
    ) -> Result<()> {
        self.simulate_liquidation(from, Book::Maker, pos_id, fee_recipient)
            .await
    }

    /// Check whether a taker `pos_id` is liquidatable right now, via
    /// `eth_call` from `from` — the batch/scanner probe for the taker book.
    ///
    /// Identical semantics to [`Self::simulate_liquidate_maker`], including
    /// what `from` means, except the "wrong book" revert here is
    /// `Perp::NonTakerPosition`.
    pub async fn simulate_liquidate_taker(
        &self,
        from: Address,
        pos_id: U256,
        fee_recipient: Address,
    ) -> Result<()> {
        self.simulate_liquidation(from, Book::Taker, pos_id, fee_recipient)
            .await
    }

    /// Shared `eth_call` health probe behind the two public liquidation
    /// probes: validates the fee recipient, encodes the book's call, and
    /// preflights from `from` at the pinned [`GasLimits::LIQUIDATE`] cap.
    async fn simulate_liquidation(
        &self,
        from: Address,
        book: Book,
        pos_id: U256,
        fee_recipient: Address,
    ) -> Result<()> {
        validate_fee_recipient(fee_recipient)?;
        let calldata = book.liquidation_calldata(pos_id, fee_recipient);
        self.chain
            .preflight_call(from, self.perp, &calldata, 0, Some(GasLimits::LIQUIDATE))
            .await?;
        Ok(())
    }
}

/// Which book, maker or taker, a liquidation targets. The two contract
/// entry points are twins; only the encoded call differs.
#[derive(Debug, Clone, Copy)]
pub(super) enum Book {
    Maker,
    Taker,
}

impl Book {
    pub(super) fn liquidation_calldata(self, pos_id: U256, fee_recipient: Address) -> Bytes {
        match self {
            Self::Maker => Perp::liquidateMakerCall {
                posId: pos_id,
                liquidationFeeRecipient: fee_recipient,
            }
            .abi_encode()
            .into(),
            Self::Taker => Perp::liquidateTakerCall {
                posId: pos_id,
                liquidationFeeRecipient: fee_recipient,
            }
            .abi_encode()
            .into(),
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Maker => "maker",
            Self::Taker => "taker",
        }
    }
}

/// Reject `Address::ZERO` as a liquidation fee recipient: the contract
/// transfers the fee wherever it is told, so the zero address silently
/// burns the caller's liquidation reward. Always a caller bug.
pub(super) fn validate_fee_recipient(
    fee_recipient: Address,
) -> std::result::Result<(), ValidationError> {
    if fee_recipient == Address::ZERO {
        return Err(ValidationError::InvalidConfig {
            reason: "liquidation fee_recipient must not be the zero address \
                     (the fee would be burned)"
                .into(),
        });
    }
    Ok(())
}
