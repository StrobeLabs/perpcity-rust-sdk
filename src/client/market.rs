//! One market's reads over a [`ChainReader`], [`MarketReader`].

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;

use crate::contracts::{Perp, PerpV022};
use crate::convert::unpack_balance_delta;
use crate::errors::{ContractError, Result, ValidationError};
use crate::hft::gas::GasLimits;

use super::chain::ChainReader;
use super::queries::Era;

/// One market's reads: the `Perp` contract and the chain reader it lives
/// on.
///
/// Every read addressed to a market — its pool state, positions, capacity,
/// the contract's mark, maker equities, the liquidation probes, and its
/// storage at one block ([`Self::state`]) — is here.
/// Owned and cheap to clone (the chain reader is an
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

/// So a helper bounded on `impl AsRef<MarketReader>` takes a market reader
/// or a client alike, and one bounded on `impl AsRef<ChainReader>` takes a
/// market reader too.
impl AsRef<MarketReader> for MarketReader {
    fn as_ref(&self) -> &MarketReader {
        self
    }
}

impl AsRef<ChainReader> for MarketReader {
    fn as_ref(&self) -> &ChainReader {
        &self.chain
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
    /// `eth_call` from `from` — the batch/scanner probe for makers.
    ///
    /// `Ok(())` means the liquidation would succeed. A typed revert says
    /// why not — triage it with
    /// [`TransactionError::is_revert`](crate::errors::TransactionError::is_revert):
    /// `Perp::NotLiquidatable` is a healthy position, `Perp::NonMakerPosition`
    /// the wrong role (route it to the taker twin). An empty revert or
    /// out-of-gas inside the pinned limit
    /// ([`SimulationFailed`](crate::errors::TransactionError::SimulationFailed))
    /// is the node's answer and not transient; a transport failure
    /// ([`GasUnavailable`](crate::errors::TransactionError::GasUnavailable))
    /// is.
    ///
    /// `from` is what the contract sees as `msg.sender`, and it matters: on
    /// build `58b42b7` a liquidation whose margin was consumed by bad debt
    /// charges the sender the shortfall, so a probe from an unfunded
    /// address reverts `TransferFromFailed` where the funded, approved
    /// sender's transaction would succeed. Probe from the address that will
    /// send. The simulation is capped at [`GasLimits::LIQUIDATE`], exactly
    /// like the send.
    ///
    /// The call is the market's era's: the 2-arg whole-position form on
    /// `58b42b7`, the 3-arg form with the position's whole size on `v0.2.2`,
    /// which costs one `makerDetails` read first. A position whose
    /// liquidity is already zero is [`ContractError::PositionNotFound`].
    pub async fn simulate_liquidate_maker(
        &self,
        from: Address,
        pos_id: U256,
        fee_recipient: Address,
    ) -> Result<()> {
        self.simulate_liquidation(from, PositionRole::Maker, pos_id, fee_recipient)
            .await
    }

    /// Check whether a taker `pos_id` is liquidatable right now, via
    /// `eth_call` from `from` — the batch/scanner probe for takers.
    ///
    /// Identical semantics to [`Self::simulate_liquidate_maker`], including
    /// what `from` means and the era's call shape (the size read here is
    /// `positions`), except the "wrong role" revert is
    /// `Perp::NonTakerPosition`.
    pub async fn simulate_liquidate_taker(
        &self,
        from: Address,
        pos_id: U256,
        fee_recipient: Address,
    ) -> Result<()> {
        self.simulate_liquidation(from, PositionRole::Taker, pos_id, fee_recipient)
            .await
    }

    /// Shared `eth_call` health probe behind the two public liquidation
    /// probes: validates the fee recipient, encodes the role's call, and
    /// preflights from `from` at the pinned [`GasLimits::LIQUIDATE`] cap.
    async fn simulate_liquidation(
        &self,
        from: Address,
        role: PositionRole,
        pos_id: U256,
        fee_recipient: Address,
    ) -> Result<()> {
        validate_fee_recipient(fee_recipient)?;
        let calldata = self
            .liquidation_calldata(role, pos_id, fee_recipient)
            .await?;
        self.chain
            .preflight_call(from, self.perp, &calldata, 0, Some(GasLimits::LIQUIDATE))
            .await?;
        Ok(())
    }

    /// The calldata that liquidates `pos_id` whole on this market's era.
    ///
    /// Build `58b42b7` takes the id and the recipient. `v0.2.2` takes an
    /// amount too, so the position's whole size is read now, at the head:
    /// the perp amount from `positions` for a taker, the liquidity from
    /// `makerDetails` for a maker. A size the chain moves between this read
    /// and the send reverts `MaxAmtExceeded`, one block wide at most; a zero
    /// size is a position that is gone or not of this role.
    pub(super) async fn liquidation_calldata(
        &self,
        role: PositionRole,
        pos_id: U256,
        fee_recipient: Address,
    ) -> Result<Bytes> {
        match self.immutables().await?.era {
            Era::Legacy => Ok(role.whole_calldata(pos_id, fee_recipient)),
            Era::Upgradeable => {
                let amount = self.liquidation_size(role, pos_id).await?;
                Ok(role.amount_calldata(pos_id, fee_recipient, amount))
            }
        }
    }

    /// The whole size of `pos_id` in the units the era's liquidation takes.
    async fn liquidation_size(&self, role: PositionRole, pos_id: U256) -> Result<u128> {
        let perp = Perp::new(self.perp, self.chain.provider());
        let size = match role {
            PositionRole::Taker => {
                let position = perp.positions(pos_id).call().await?;
                let (perp_atoms, _usd) = unpack_balance_delta(position.delta);
                perp_atoms.unsigned_abs()
            }
            PositionRole::Maker => perp.makerDetails(pos_id).call().await?.liquidity,
        };
        if size == 0 {
            return Err(ContractError::PositionNotFound { pos_id }.into());
        }
        Ok(size)
    }
}

/// The role, maker or taker, of the position a liquidation targets. The
/// two contract entry points are twins; only the encoded call differs.
#[derive(Debug, Clone, Copy)]
pub(super) enum PositionRole {
    Maker,
    Taker,
}

impl PositionRole {
    /// Build `58b42b7`'s call: the whole position, no amount.
    fn whole_calldata(self, pos_id: U256, fee_recipient: Address) -> Bytes {
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

    /// `v0.2.2`'s call: `amount` of the position, liquidity for a maker
    /// and perp atoms for a taker.
    fn amount_calldata(self, pos_id: U256, fee_recipient: Address, amount: u128) -> Bytes {
        match self {
            Self::Maker => PerpV022::liquidateMakerCall {
                posId: pos_id,
                liquidationFeeRecipient: fee_recipient,
                liquidityAmount: amount,
            }
            .abi_encode()
            .into(),
            Self::Taker => PerpV022::liquidateTakerCall {
                posId: pos_id,
                liquidationFeeRecipient: fee_recipient,
                perpAmount: amount,
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

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, B256, U256};
    use alloy::sol_types::SolCall;

    use super::*;
    use crate::client::mock;
    use crate::contracts::{Maker, Position};
    use crate::convert::pack_balance_delta;
    use crate::errors::PerpCityError;

    const POOL_ID: B256 = B256::repeat_byte(0x99);
    const RECIPIENT: Address = Address::repeat_byte(0x44);

    /// A build `58b42b7` market: the whole-position call, and no read
    /// beyond the immutables.
    #[tokio::test]
    async fn legacy_market_liquidates_whole_with_the_2_arg_call() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key(30));

        let calldata = client
            .market()
            .liquidation_calldata(PositionRole::Taker, U256::from(7u8), RECIPIENT)
            .await
            .unwrap();
        assert_eq!(
            calldata,
            Bytes::from(
                Perp::liquidateTakerCall {
                    posId: U256::from(7u8),
                    liquidationFeeRecipient: RECIPIENT,
                }
                .abi_encode()
            )
        );
        assert!(rpc.is_drained(), "POOL_ID, poolKey");
    }

    /// A `v0.2.2` market: the position's whole size read first, then the
    /// 3-arg call. A short taker's amount is its perp delta's magnitude.
    #[tokio::test]
    async fn upgradeable_market_reads_the_size_then_liquidates_by_amount() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key_hooked(30));
        rpc.call::<Perp::positionsCall>(&Position {
            delta: pack_balance_delta(-2_500_000, 3_000_000),
            ..mock::position(1_000_000)
        });

        let calldata = client
            .market()
            .liquidation_calldata(PositionRole::Taker, U256::from(7u8), RECIPIENT)
            .await
            .unwrap();
        assert_eq!(
            calldata,
            Bytes::from(
                PerpV022::liquidateTakerCall {
                    posId: U256::from(7u8),
                    liquidationFeeRecipient: RECIPIENT,
                    perpAmount: 2_500_000,
                }
                .abi_encode()
            )
        );
        assert!(rpc.is_drained(), "POOL_ID, poolKey, positions");

        // The era is cached; a maker's size is its liquidity.
        rpc.call::<Perp::makerDetailsCall>(&Maker {
            liquidity: 9_000_000_000,
            ..mock::maker(-60, 60, 0)
        });
        let calldata = client
            .market()
            .liquidation_calldata(PositionRole::Maker, U256::from(8u8), RECIPIENT)
            .await
            .unwrap();
        assert_eq!(
            calldata,
            Bytes::from(
                PerpV022::liquidateMakerCall {
                    posId: U256::from(8u8),
                    liquidationFeeRecipient: RECIPIENT,
                    liquidityAmount: 9_000_000_000,
                }
                .abi_encode()
            )
        );
        assert!(rpc.is_drained(), "makerDetails only");
    }

    /// A zero size on a `v0.2.2` market is a position that is gone or not
    /// of this role: named, not sent to revert `ZeroLiquidity`.
    #[tokio::test]
    async fn upgradeable_market_names_a_vanished_position() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key_hooked(30));
        rpc.call::<Perp::makerDetailsCall>(&mock::maker(-60, 60, 0));

        let err = client
            .market()
            .liquidation_calldata(PositionRole::Maker, U256::from(8u8), RECIPIENT)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::PositionNotFound { pos_id })
                    if pos_id == U256::from(8u8)
            ),
            "{err}"
        );
        assert!(rpc.is_drained());
    }
}
