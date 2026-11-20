//! `v1.0.0` maker writes: open, adjust and close with a deadline, each
//! returning the receipt's parsed event. A removal the close depth cannot
//! take is refused before the send, typed as the contract's revert.

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::rpc::types::TransactionReceipt;
use alloy::sol_types::SolCall;

use crate::contracts::v1::{AdjustMakerParams, OpenMakerParams, Perp};
use crate::errors::{ContractError, PerpCityError, Result, ValidationError};
use crate::events::receipt::{MakerExit, V1TradeResult, parse_maker_exit, parse_v1_receipt_for};
use crate::hft::gas::Urgency;
use crate::math::capacity::{MarketCapacity, band_capacity_x64};
use crate::math::liquidity::band_amounts;
use crate::math::range::MakerBand;
use crate::units::{LUnits, PerSide, PerpAtoms, PerpDelta, Side, SqrtPrice, UsdcAtoms, UsdcDelta};

use super::super::trades::{ExactAdjustMakerParams, ExactOpenMakerParams, check_opening_margin};
use super::super::{PerpClient, i32_to_i24, now_ms};
use super::{StuckCall, deadline_after, renewed_deadline};

/// What a `v1.0.0` maker adjustment mined as: the band still holding
/// liquidity, or the exit that removing all of it makes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum V1MakerAdjustment {
    /// The band still holds liquidity.
    Adjusted(V1TradeResult<Perp::MakerAdjusted>),
    /// All liquidity removed: closed, or converted to a taker.
    Exit(MakerExit),
}

/// The mined outcome of a maker trade submitted for replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum V1MakerReplacement {
    /// A new maker.
    Opened(V1TradeResult<Perp::MakerOpened>),
    /// An adjustment, close, or conversion to a taker.
    Adjustment(V1MakerAdjustment),
}

fn replacement_calldata(input: &[u8], now_secs: u64) -> Option<(StuckCall, Option<Bytes>)> {
    match input.get(..4)?.try_into().ok()? {
        Perp::openMakerCall::SELECTOR => {
            let mut call = Perp::openMakerCall::abi_decode(input).ok()?;
            let kind = StuckCall::Open {
                holder: call.params.holder,
            };
            let calldata = renewed_deadline(call.params.deadline, now_secs).map(|deadline| {
                call.params.deadline = deadline;
                call.abi_encode().into()
            });
            Some((kind, calldata))
        }
        Perp::adjustMakerCall::SELECTOR => {
            let mut call = Perp::adjustMakerCall::abi_decode(input).ok()?;
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

/// `openMaker` for `holder`.
fn open_call(holder: Address, params: &ExactOpenMakerParams, deadline: u64) -> Perp::openMakerCall {
    Perp::openMakerCall {
        params: OpenMakerParams {
            holder,
            margin: params.margin.atoms(),
            tickLower: i32_to_i24(params.range.lower()),
            tickUpper: i32_to_i24(params.range.upper()),
            liquidity: params.liquidity.units(),
            maxAmt0In: U256::from(params.max_amt0_in.atoms()),
            maxAmt1In: U256::from(params.max_amt1_in.atoms()),
            deadline,
        },
    }
}

/// `adjustMaker`.
fn adjust_call(params: &ExactAdjustMakerParams, deadline: u64) -> Perp::adjustMakerCall {
    Perp::adjustMakerCall {
        params: AdjustMakerParams {
            posId: params.pos_id,
            marginDelta: params.margin_delta.atoms(),
            liquidityDelta: params.liquidity_delta.units(),
            amt0Limit: U256::from(params.amt0_limit.atoms()),
            amt1Limit: U256::from(params.amt1_limit.atoms()),
            deadline,
        },
    }
}

/// The chain state a maker removal is checked against, read at one block.
#[derive(Debug, Clone, Copy)]
struct RemovalState {
    band: MakerBand,
    perp: PerpDelta,
    open_interest: PerSide<PerpAtoms>,
    depth_x64: PerSide<U256>,
    book: U256,
    sqrt_price: SqrtPrice,
}

/// The contract's close-depth refusal of removing `removed` from the band:
/// its X64 contribution leaves the opposite depths, and removing all of it
/// converts the position's perp left (its stored perp plus the removed
/// principal, rounded down as the pool rounds a removal) to a taker.
fn check_removal(
    state: &RemovalState,
    removed: LUnits,
) -> std::result::Result<(), ValidationError> {
    if removed > state.band.liquidity {
        return Err(ValidationError::ExcessMakerRemoval {
            requested: removed.units(),
            available: state.band.liquidity.units(),
        });
    }
    let removed_band = MakerBand::new(state.band.range, removed);
    let band_x64 = band_capacity_x64(state.sqrt_price, &removed_band)?;
    let residual = if removed == state.band.liquidity {
        let (principal, _) = band_amounts(state.sqrt_price, &removed_band)?;
        let overflow = || ValidationError::Overflow {
            context: "maker removal perp delta exceeds int128".into(),
        };
        let principal = i128::try_from(principal.atoms()).map_err(|_| overflow())?;
        PerpDelta::new(
            state
                .perp
                .atoms()
                .checked_add(principal)
                .ok_or_else(overflow)?,
        )
    } else {
        PerpDelta::ZERO
    };
    let last_maker = state.book == U256::from(removed.units());
    let refusal = MarketCapacity::maker_removal_refusal(
        state.open_interest,
        residual,
        state.depth_x64,
        band_x64,
        last_maker,
    )?;
    let Some((side, interest)) = refusal else {
        return Ok(());
    };
    let interest = interest.atoms();
    Err(match side {
        Side::Long => ValidationError::LongCloseCapacityExceeded {
            interest,
            last_maker,
        },
        Side::Short => ValidationError::ShortCloseCapacityExceeded {
            interest,
            last_maker,
        },
    })
}

impl PerpClient {
    /// Open a maker on a `v1.0.0` market, with a deadline
    /// [`V1_TRADE_DEADLINE`](super::V1_TRADE_DEADLINE) after now. The
    /// market's era is read first; nothing is sent to another build.
    ///
    /// Returns the receipt's `MakerOpened` for this client and the minted
    /// id; the event reports no margin.
    ///
    /// # Errors
    ///
    /// [`ContractError::UnsupportedEra`] for an earlier build and
    /// [`ContractError::UnknownBuild`] for an unknown one, before anything
    /// else; [`ValidationError::InvalidMargin`] below the opening minimum;
    /// the send's errors, with the contract's revert decoded at the
    /// preflight (`TvlCapExceeded`, `MaxAmtExceeded`, ...);
    /// [`ContractError::MinedEventMissing`] or
    /// [`ContractError::MinedEventInvalid`] for a mined receipt without a
    /// readable event: the trade mined, so do not send it again.
    pub async fn open_maker_v1(
        &self,
        params: &ExactOpenMakerParams,
        urgency: Urgency,
    ) -> Result<V1TradeResult<Perp::MakerOpened>> {
        self.require_v1("open_maker_v1").await?;
        check_opening_margin(params.margin)?;
        let call = open_call(self.address, params, deadline_after(now_ms() / 1_000));
        tracing::debug!(
            margin_atoms = params.margin.atoms(),
            tick_lower = params.range.lower(),
            tick_upper = params.range.upper(),
            liquidity = params.liquidity.units(),
            deadline = call.params.deadline,
            ?urgency,
            "opening v1 maker position"
        );
        let receipt = self
            .tx(self.market.perp(), call.abi_encode().into())
            .with_urgency(urgency)
            .send()
            .await?;
        parse_v1_receipt_for(&receipt, self.market.perp(), None, Some(self.address))
    }

    /// Adjust a maker on a `v1.0.0` market, with a deadline
    /// [`V1_TRADE_DEADLINE`](super::V1_TRADE_DEADLINE) after now.
    ///
    /// A removal is checked first against the close depth at the head
    /// block, as [`Self::close_maker_v1`] checks a close; the contract
    /// checks it again at the block it mines in. Removing all of the
    /// band's liquidity exits the position: [`V1MakerAdjustment::Exit`].
    ///
    /// # Errors
    ///
    /// As [`Self::open_maker_v1`], without the margin minimum;
    /// [`ValidationError::LongCloseCapacityExceeded`] or
    /// [`ValidationError::ShortCloseCapacityExceeded`] for a removal the
    /// close depth cannot take, before anything is sent;
    /// [`ContractError::PositionNotFound`] for an id that is not an open
    /// maker when a removal reads it.
    pub async fn adjust_maker_v1(
        &self,
        params: &ExactAdjustMakerParams,
        urgency: Urgency,
    ) -> Result<V1MakerAdjustment> {
        self.require_v1("adjust_maker_v1").await?;
        if params.liquidity_delta.units() < 0 {
            let state = self.removal_state(params.pos_id).await?;
            check_removal(&state, params.liquidity_delta.magnitude())?;
        }
        let receipt = self.send_adjust_maker_v1(params, urgency).await?;
        maker_adjustment(&receipt, self.market.perp(), params.pos_id)
    }

    /// Close a maker on a `v1.0.0` market: all of its liquidity, read now,
    /// removed with no amount minimums (a market close; for minimums, call
    /// [`Self::adjust_maker_v1`]).
    ///
    /// The removal is checked first against the close depth, with the perp
    /// it leaves converted to a taker, as the contract checks it
    /// (`PerpLogic.checkPhysicalCapacity`). The last maker of a market
    /// clears both depths, so it closes only when no open interest is left
    /// and its own perp left is zero: a one-maker market's maker with a
    /// residual is refused here before send. The send preflight checks
    /// the conversion's margin and health.
    ///
    /// # Errors
    ///
    /// As [`Self::adjust_maker_v1`].
    pub async fn close_maker_v1(&self, pos_id: U256, urgency: Urgency) -> Result<MakerExit> {
        self.require_v1("close_maker_v1").await?;
        let state = self.removal_state(pos_id).await?;
        check_removal(&state, state.band.liquidity)?;
        let params = ExactAdjustMakerParams {
            pos_id,
            margin_delta: UsdcDelta::ZERO,
            liquidity_delta: state.band.liquidity.negated()?,
            amt0_limit: PerpAtoms::ZERO,
            amt1_limit: UsdcAtoms::ZERO,
        };
        let receipt = self.send_adjust_maker_v1(&params, urgency).await?;
        parse_maker_exit(&receipt, self.market.perp(), pos_id, Some(self.address))
    }

    /// Replace a pending v1 maker write at the same nonce with a new
    /// deadline and higher fees. If the original mines first, returns
    /// its parsed outcome. See [`Self::replace_v1_taker`] for the send
    /// and receipt rules.
    ///
    /// # Errors
    ///
    /// As [`Self::replace_v1_taker`], for v1 maker calls.
    pub async fn replace_v1_maker(
        &self,
        stuck: B256,
        urgency: Urgency,
    ) -> Result<V1MakerReplacement> {
        let (call, receipt) = self
            .replace_v1_trade("replace_v1_maker", stuck, urgency, replacement_calldata)
            .await?;
        let perp = self.market.perp();
        match call {
            StuckCall::Open { holder } => parse_v1_receipt_for(&receipt, perp, None, Some(holder))
                .map(V1MakerReplacement::Opened),
            StuckCall::Adjust { pos_id } => {
                maker_adjustment(&receipt, perp, pos_id).map(V1MakerReplacement::Adjustment)
            }
        }
    }

    /// The band, the position's perp, the open interest, the X64 close
    /// depth, the market's maker liquidity and the pool price, at the head.
    async fn removal_state(&self, pos_id: U256) -> Result<RemovalState> {
        let state = self.market.state_at_head().await?;
        let (band, position, open_interest, depth_x64, book, sqrt_price) = tokio::try_join!(
            state.maker_band(pos_id),
            state.position(pos_id),
            state.v1_open_interest(),
            state.physical_close_capacity_x64(),
            state.maker_liquidity(),
            state.pool_sqrt_price(),
        )?;
        let not_found = || -> PerpCityError { ContractError::PositionNotFound { pos_id }.into() };
        Ok(RemovalState {
            band: band.ok_or_else(not_found)?,
            perp: position.ok_or_else(not_found)?.perp,
            open_interest,
            depth_x64,
            book,
            sqrt_price,
        })
    }

    /// Sign, send and wait for one `adjustMaker`.
    async fn send_adjust_maker_v1(
        &self,
        params: &ExactAdjustMakerParams,
        urgency: Urgency,
    ) -> Result<TransactionReceipt> {
        let call = adjust_call(params, deadline_after(now_ms() / 1_000));
        tracing::debug!(
            pos_id = %params.pos_id,
            margin_delta_atoms = params.margin_delta.atoms(),
            liquidity_delta = params.liquidity_delta.units(),
            deadline = call.params.deadline,
            ?urgency,
            "adjusting v1 maker position"
        );
        self.tx(self.market.perp(), call.abi_encode().into())
            .with_urgency(urgency)
            .send()
            .await
    }
}

/// The adjustment `receipt` mined for `pos_id` on `perp`.
fn maker_adjustment(
    receipt: &TransactionReceipt,
    perp: Address,
    pos_id: U256,
) -> Result<V1MakerAdjustment> {
    match parse_v1_receipt_for::<Perp::MakerAdjusted>(receipt, perp, Some(pos_id), None) {
        Ok(adjusted) => Ok(V1MakerAdjustment::Adjusted(adjusted)),
        Err(PerpCityError::Contract(ContractError::MinedEventMissing { tx_hash, .. })) => {
            match parse_maker_exit(receipt, perp, pos_id, None) {
                Ok(exit) => Ok(V1MakerAdjustment::Exit(exit)),
                Err(PerpCityError::Contract(ContractError::MinedEventMissing { .. })) => {
                    Err(ContractError::MinedEventMissing {
                        tx_hash,
                        event: "MakerAdjusted, MakerClosed or MakerConverted",
                    }
                    .into())
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ARBITRUM_CHAIN_ID, ARBITRUM_SEPOLIA_CHAIN_ID};
    use crate::client::{ChainDeployments, Era, mock};
    use crate::contracts::Perp as EarlierPerp;
    use crate::errors::TransactionError;
    use crate::math::range::TickRange;
    use crate::units::LDelta;

    fn open() -> ExactOpenMakerParams {
        ExactOpenMakerParams {
            margin: UsdcAtoms::new(100_000_000),
            range: TickRange::new(38_700, 39_540).unwrap(),
            liquidity: LUnits::new(335_000_000),
            max_amt0_in: PerpAtoms::new(979_407),
            max_amt1_in: UsdcAtoms::new(49_477_350),
        }
    }

    fn adjust() -> ExactAdjustMakerParams {
        ExactAdjustMakerParams {
            pos_id: U256::ONE,
            margin_delta: UsdcDelta::new(-5_000_000),
            liquidity_delta: LDelta::new(-50_000_000),
            amt0_limit: PerpAtoms::new(100),
            amt1_limit: UsdcAtoms::new(1_000),
        }
    }

    #[test]
    fn maker_calls_encode_limits_and_deadline() {
        let holder = Address::repeat_byte(1);
        let deadline = deadline_after(1_795_141_000);
        let open = open_call(holder, &open(), deadline).abi_encode();
        assert_eq!(&open[..4], &[0xdb, 0xf6, 0x6c, 0xd6]);
        let open = Perp::openMakerCall::abi_decode(&open).unwrap().params;
        assert_eq!(open.holder, holder);
        assert_eq!(open.margin, 100_000_000);
        assert_eq!(open.tickLower.as_i32(), 38_700);
        assert_eq!(open.tickUpper.as_i32(), 39_540);
        assert_eq!(open.liquidity, 335_000_000);
        assert_eq!(open.maxAmt0In, U256::from(979_407u64));
        assert_eq!(open.maxAmt1In, U256::from(49_477_350u64));
        assert_eq!(open.deadline, 1_795_141_600);

        let adjust = adjust_call(&adjust(), deadline).abi_encode();
        assert_eq!(&adjust[..4], &[0x7b, 0xd0, 0x8a, 0x1a]);
        let adjust = Perp::adjustMakerCall::abi_decode(&adjust).unwrap().params;
        assert_eq!(adjust.posId, U256::ONE);
        assert_eq!(adjust.marginDelta, -5_000_000);
        assert_eq!(adjust.liquidityDelta, -50_000_000);
        assert_eq!(adjust.amt0Limit, U256::from(100u64));
        assert_eq!(adjust.amt1Limit, U256::from(1_000u64));
        assert_eq!(adjust.deadline, 1_795_141_600);
    }

    #[test]
    fn replacing_a_maker_write_changes_only_the_deadline() {
        let holder = Address::repeat_byte(1);
        let open = open_call(holder, &open(), 1_000);
        let (kind, bytes) = replacement_calldata(&open.abi_encode(), 1_000).unwrap();
        assert_eq!(kind, StuckCall::Open { holder });
        let mut expected = open;
        expected.params.deadline = 1_600;
        assert_eq!(bytes.unwrap().as_ref(), expected.abi_encode());

        let adjust = adjust_call(&adjust(), 1_600);
        let (kind, bytes) = replacement_calldata(&adjust.abi_encode(), 1_000).unwrap();
        assert_eq!(kind, StuckCall::Adjust { pos_id: U256::ONE });
        let mut expected = adjust;
        expected.params.deadline = 1_601;
        assert_eq!(bytes.unwrap().as_ref(), expected.abi_encode());
        expected.params.deadline = u64::MAX;
        assert_eq!(
            replacement_calldata(&expected.abi_encode(), 1_000),
            Some((kind, None))
        );
    }

    const STUCK: B256 = B256::repeat_byte(0x5a);

    /// The cause of a replacement refused before the look-up, which keeps
    /// the given hash, unresolved.
    fn kept_cause(err: PerpCityError) -> PerpCityError {
        assert!(err.is_unconfirmed() && !err.is_transient(), "{err}");
        assert_eq!(err.hash_to_replace(), Some(STUCK));
        match err {
            PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
                original,
                nonce: None,
                replacement: None,
                refused: false,
                source: Some(source),
            }) if original == STUCK => *source,
            other => panic!("expected the hash kept: {other}"),
        }
    }

    #[tokio::test]
    async fn maker_writes_refuse_other_eras_on_both_chains() {
        for (chain, id) in [
            (ChainDeployments::ARBITRUM, ARBITRUM_CHAIN_ID),
            (
                ChainDeployments::ARBITRUM_SEPOLIA,
                ARBITRUM_SEPOLIA_CHAIN_ID,
            ),
        ] {
            for era in [Era::Legacy, Era::Upgradeable] {
                let (client, rpc) = mock::client_on(chain, id);
                rpc.immutables_with_hooks(
                    era,
                    chain.v022_perp_guard_hook,
                    chain.v1_perp_guard_hook,
                );
                let open_error = client
                    .open_maker_v1(&open(), Urgency::Normal)
                    .await
                    .unwrap_err();
                let adjust_error = client
                    .adjust_maker_v1(&adjust(), Urgency::Normal)
                    .await
                    .unwrap_err();
                let close_error = client
                    .close_maker_v1(U256::ONE, Urgency::Normal)
                    .await
                    .unwrap_err();
                let replace_error = kept_cause(
                    client
                        .replace_v1_maker(STUCK, Urgency::Normal)
                        .await
                        .unwrap_err(),
                );
                for (error, expected_operation) in [
                    (replace_error, "replace_v1_maker"),
                    (open_error, "open_maker_v1"),
                    (adjust_error, "adjust_maker_v1"),
                    (close_error, "close_maker_v1"),
                ] {
                    assert!(
                        matches!(error, PerpCityError::Contract(ContractError::UnsupportedEra {
                        era: found, operation,
                    }) if found == era && operation == expected_operation)
                    );
                }
                assert_eq!(
                    rpc.selectors(),
                    [
                        EarlierPerp::poolKeyCall::SELECTOR,
                        EarlierPerp::POOL_IDCall::SELECTOR
                    ]
                );
            }

            let (client, rpc) = mock::client_on(chain, id);
            for _ in 0..4 {
                rpc.call::<EarlierPerp::poolKeyCall>(&mock::pool_key_with_hook(
                    30,
                    Address::repeat_byte(0xf1),
                ));
            }
            let open_error = client
                .open_maker_v1(&open(), Urgency::Normal)
                .await
                .unwrap_err();
            let adjust_error = client
                .adjust_maker_v1(&adjust(), Urgency::Normal)
                .await
                .unwrap_err();
            let close_error = client
                .close_maker_v1(U256::ONE, Urgency::Normal)
                .await
                .unwrap_err();
            let replace_error = kept_cause(
                client
                    .replace_v1_maker(STUCK, Urgency::Normal)
                    .await
                    .unwrap_err(),
            );
            for error in [open_error, adjust_error, close_error, replace_error] {
                assert!(matches!(
                    error,
                    PerpCityError::Contract(ContractError::UnknownBuild { .. })
                ));
            }
            assert_eq!(rpc.selectors(), [EarlierPerp::poolKeyCall::SELECTOR; 4]);
        }
    }

    #[tokio::test]
    async fn position_transfer_checks_the_era_before_erc721_ownership() {
        for (chain, id) in [
            (ChainDeployments::ARBITRUM, ARBITRUM_CHAIN_ID),
            (
                ChainDeployments::ARBITRUM_SEPOLIA,
                ARBITRUM_SEPOLIA_CHAIN_ID,
            ),
        ] {
            for era in [Era::Legacy, Era::Upgradeable, Era::V1] {
                let (client, rpc) = mock::client_on(chain, id);
                rpc.immutables_with_hooks(
                    era,
                    chain.v022_perp_guard_hook,
                    chain.v1_perp_guard_hook,
                );
                if era != Era::V1 {
                    rpc.call::<EarlierPerp::ownerOfCall>(&Address::repeat_byte(0x11));
                }
                let result = client
                    .transfer_position(Address::repeat_byte(0x12), U256::ONE, Urgency::Normal)
                    .await;
                if era == Era::V1 {
                    assert!(matches!(
                        result,
                        Err(PerpCityError::Contract(ContractError::UnsupportedEra {
                            era: Era::V1,
                            operation: "transfer_position",
                        }))
                    ));
                    assert_eq!(
                        rpc.selectors(),
                        [
                            EarlierPerp::poolKeyCall::SELECTOR,
                            Perp::poolIdCall::SELECTOR
                        ]
                    );
                } else {
                    assert!(matches!(
                        result,
                        Err(PerpCityError::Contract(
                            ContractError::PositionNotOwned { .. }
                        ))
                    ));
                    assert_eq!(
                        rpc.selectors(),
                        [
                            EarlierPerp::poolKeyCall::SELECTOR,
                            EarlierPerp::POOL_IDCall::SELECTOR,
                            EarlierPerp::ownerOfCall::SELECTOR
                        ]
                    );
                }
                assert!(rpc.is_drained());
            }
        }
    }
}
