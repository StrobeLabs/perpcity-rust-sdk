//! One market's reads over a [`ChainReader`], [`MarketReader`].

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;
use serde::{Deserialize, Serialize};

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

    /// The build this market runs: its pool key's hook against the chain's
    /// deployments ([`ChainDeployments::era_of`](super::ChainDeployments::era_of)).
    /// Read once per market and cached with the pool key.
    ///
    /// # Errors
    ///
    /// [`ContractError::UnknownBuild`] for a hook the chain's deployments
    /// do not name; the transport error from either read.
    pub async fn era(&self) -> Result<Era> {
        Ok(self.immutables().await?.era)
    }

    /// Whether this market runs `v0.2.2-upgradeable`, the build the SDK's
    /// health and equity math ports: [`Self::era`] is
    /// [`Era::Upgradeable`]. False for build `58b42b7` and for `v1.0.0`.
    pub async fn is_upgradeable(&self) -> Result<bool> {
        Ok(self.era().await? == Era::Upgradeable)
    }

    /// The calldata that liquidates `pos_id` whole on this market's era, for
    /// a sender not bound to this market (one signer liquidating across
    /// several); send it to this market's perp at [`GasLimits::LIQUIDATE`].
    ///
    /// Build `58b42b7` takes the id and the recipient. `v0.2.2` takes an
    /// amount too, so the position's whole size is read now, at the head:
    /// the perp amount from `positions` for a taker, the liquidity from
    /// `makerDetails` for a maker. A size the chain moves between this read
    /// and the send reverts `MaxAmtExceeded`, one block wide at most; a zero
    /// size is a position that is gone or not of this role.
    pub async fn liquidation_calldata(
        &self,
        role: PositionRole,
        pos_id: U256,
        fee_recipient: Address,
    ) -> Result<Bytes> {
        validate_fee_recipient(fee_recipient)?;
        match self.immutables().await?.era {
            Era::Legacy => Ok(role.whole_calldata(pos_id, fee_recipient)),
            Era::Upgradeable => {
                let amount = self.liquidation_size(role, pos_id).await?;
                Ok(role.amount_calldata(pos_id, fee_recipient, amount))
            }
            era @ Era::V1 => Err(ContractError::UnsupportedEra {
                era,
                operation: "liquidation_calldata",
            }
            .into()),
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

/// The role a position plays, maker or taker. The two liquidation entry
/// points are twins keyed on it; only the encoded call differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PositionRole {
    /// A band of liquidity in the pool.
    Maker,
    /// A directional position against the pool.
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
    use crate::client::{ChainDeployments, mock};
    use crate::contracts::{Maker, Position, v1};
    use crate::convert::pack_balance_delta;
    use crate::errors::PerpCityError;

    const POOL_ID: B256 = B256::repeat_byte(0x99);
    const RECIPIENT: Address = Address::repeat_byte(0x44);

    /// The build is the pool key's hook, read once per market.
    #[tokio::test]
    async fn the_build_is_read_from_the_pool_hook_once() {
        let (chain, rpc) = mock::chain();
        let (legacy, upgradeable) = (Address::repeat_byte(0x01), Address::repeat_byte(0x02));
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key(30));
        rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
        assert!(!chain.market(legacy).is_upgradeable().await.unwrap());
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key_hooked(30));
        rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
        assert!(chain.market(upgradeable).is_upgradeable().await.unwrap());
        assert!(chain.market(upgradeable).is_upgradeable().await.unwrap());
        assert!(rpc.is_drained(), "two reads per market, then the cache");
    }

    /// Each build by its hook, against the mocked chain's deployments: the
    /// pool key first, then the era's pool id view, `poolId()` on `v1.0.0`
    /// (it has no `POOL_ID()`). Only `v0.2.2` is upgradeable.
    #[tokio::test]
    async fn each_build_is_its_chain_s_hook() {
        let (chain, rpc) = mock::chain();
        let cases = [
            (Address::ZERO, Era::Legacy),
            (mock::V022_HOOK, Era::Upgradeable),
            (mock::V1_HOOK, Era::V1),
        ];
        for (i, (hook, era)) in cases.into_iter().enumerate() {
            let market = chain.market(Address::repeat_byte(0x01 + i as u8));
            rpc.call::<Perp::poolKeyCall>(&mock::pool_key_with_hook(30, hook));
            if era == Era::V1 {
                rpc.call::<v1::Perp::poolIdCall>(&POOL_ID);
            } else {
                rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
            }
            assert_eq!(market.era().await.unwrap(), era);
            assert_eq!(market.immutables().await.unwrap().pool_id, POOL_ID);
            assert_eq!(
                market.is_upgradeable().await.unwrap(),
                era == Era::Upgradeable
            );
            assert!(rpc.is_drained(), "two reads, then the cache");
        }
    }

    /// A hook the chain's deployments do not name is an unknown build:
    /// refused after the pool key, with no pool id read and nothing
    /// cached, so the next call asks again. The mocked chain's hooks are
    /// another chain's here, and a real chain's are unknown on the mock.
    #[tokio::test]
    async fn an_unknown_hook_is_refused_after_the_pool_key() {
        let (chain, rpc) = mock::chain();
        let market = chain.market(mock::PERP);
        for hook in [
            Address::repeat_byte(0x0e),
            ChainDeployments::ARBITRUM.v022_perp_guard_hook,
            ChainDeployments::ARBITRUM.v1_perp_guard_hook,
        ] {
            rpc.call::<Perp::poolKeyCall>(&mock::pool_key_with_hook(30, hook));
            let err = market.era().await.unwrap_err();
            assert!(
                matches!(
                    err,
                    PerpCityError::Contract(ContractError::UnknownBuild { perp, hook: named })
                        if perp == mock::PERP && named == hook
                ),
                "{err}"
            );
            assert!(!err.is_transient());
            assert!(rpc.is_drained(), "one read, nothing cached");
        }
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key_with_hook(30, mock::V022_HOOK));
        rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
        assert!(market.is_upgradeable().await.unwrap());
        assert!(rpc.is_drained());
    }

    /// The tape addresses resolve the build before the modules, so an
    /// unknown build is refused after the pool key alone and no `modules()`
    /// request can fail first and hide it.
    #[tokio::test]
    async fn tape_addresses_refuse_an_unknown_build_before_the_modules() {
        let (chain, rpc) = mock::chain();
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key_with_hook(30, Address::repeat_byte(0x0e)));
        let err = chain.market(mock::PERP).tape_addresses().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::UnknownBuild { .. })
            ),
            "{err}"
        );
        assert!(rpc.is_drained(), "poolKey only, no modules");
    }

    /// A `v1.0.0` market's tape is refused after the immutables, before the
    /// modules and before any log request: the decoder has no `v1.0.0`
    /// position events yet, and a tape without them would report no gap.
    #[tokio::test]
    async fn a_v1_market_refuses_its_tape_addresses() {
        let (chain, rpc) = mock::chain();
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key_with_hook(30, mock::V1_HOOK));
        rpc.call::<v1::Perp::poolIdCall>(&POOL_ID);
        let err = chain.market(mock::PERP).tape_addresses().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::UnsupportedEra {
                    era: Era::V1,
                    operation: "tape_addresses",
                })
            ),
            "{err}"
        );
        assert!(!err.is_transient());
        assert!(
            rpc.is_drained(),
            "poolKey, poolId; no modules, no eth_getLogs"
        );
    }

    /// No liquidation call is sent to a `v1.0.0` market on another era's
    /// shape: it is refused after the immutables, with no size read.
    #[tokio::test]
    async fn a_v1_market_refuses_the_liquidation_calldata() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key_with_hook(30, mock::V1_HOOK));
        rpc.call::<v1::Perp::poolIdCall>(&POOL_ID);
        let err = client
            .market()
            .liquidation_calldata(PositionRole::Taker, U256::from(7u8), RECIPIENT)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::UnsupportedEra {
                    era: Era::V1,
                    operation: "liquidation_calldata",
                })
            ),
            "{err}"
        );
        assert!(!err.is_transient());
        assert!(rpc.is_drained(), "poolKey, poolId");
    }

    /// A build `58b42b7` market: the whole-position call, and no read
    /// beyond the immutables.
    #[tokio::test]
    async fn legacy_market_liquidates_whole_with_the_2_arg_call() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key(30));
        rpc.call::<Perp::POOL_IDCall>(&POOL_ID);

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
        assert!(rpc.is_drained(), "poolKey, POOL_ID");
    }

    /// A `v0.2.2` market: the position's whole size read first, then the
    /// 3-arg call. A short taker's amount is its perp delta's magnitude.
    #[tokio::test]
    async fn upgradeable_market_reads_the_size_then_liquidates_by_amount() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key_hooked(30));
        rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
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
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key_hooked(30));
        rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
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
