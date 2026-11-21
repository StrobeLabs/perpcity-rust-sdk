//! `v1.0.0` maker writes: open, adjust and close with a deadline, each
//! returning the receipt's parsed event. A removal the close depth cannot
//! take is refused before the send, typed as the contract's revert.

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::rpc::types::TransactionReceipt;
use alloy::sol_types::{SolCall, SolError};

use crate::contracts::v1::{AdjustMakerParams, OpenMakerParams, Perp};
use crate::errors::{ContractError, PerpCityError, Result, TransactionError, ValidationError};
use crate::events::receipt::{MakerExit, V1TradeResult, parse_maker_exit, parse_v1_receipt_for};
use crate::hft::gas::Urgency;
use crate::math::capacity::{MarketCapacity, band_capacity_x64};
use crate::math::liquidity::band_amounts;
use crate::math::range::MakerBand;
use crate::units::{LUnits, PerSide, PerpAtoms, PerpDelta, Side, SqrtPrice, UsdcAtoms, UsdcDelta};

use super::super::state::StateAt;
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

/// The side, the open interest and `last_maker` of a close-depth refusal.
fn refused_side(refusal: &ValidationError) -> Option<(Side, u128, bool)> {
    match *refusal {
        ValidationError::LongCloseCapacityExceeded {
            interest,
            last_maker,
        } => Some((Side::Long, interest, last_maker)),
        ValidationError::ShortCloseCapacityExceeded {
            interest,
            last_maker,
        } => Some((Side::Short, interest, last_maker)),
        _ => None,
    }
}

/// The answer to a removal [`check_removal`] refused, given the contract's
/// answer to the same call at the same block: the refusal when the contract
/// reverts with the same capacity error, the contract's revert when an
/// earlier check fails first (`MarginTooLow` before the converted
/// residual's depth, for one), the call's own failure when the contract
/// gave no answer, and `None`, to send, when it accepts.
fn confirmed_refusal(refusal: ValidationError, simulated: Result<()>) -> Option<PerpCityError> {
    let expected = match refusal {
        ValidationError::LongCloseCapacityExceeded { .. } => {
            Perp::LongCloseCapacityExceeded::SELECTOR
        }
        ValidationError::ShortCloseCapacityExceeded { .. } => {
            Perp::ShortCloseCapacityExceeded::SELECTOR
        }
        _ => return Some(refusal.into()),
    };
    match simulated {
        Ok(()) => None,
        Err(PerpCityError::Transaction(TransactionError::SimulationReverted {
            selector, ..
        })) if selector.0 == expected => Some(refusal.into()),
        Err(error) => Some(error),
    }
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
    /// A removal the check refuses is sent as an `eth_call` at the same
    /// block, with the calldata the send would sign, before the refusal is
    /// returned. The contract checks the amount limits, then the open
    /// interest at the removal against the depth, then the margin, then
    /// the conversion's margin and health, then the converted residual
    /// against the depth: when an earlier check fails, its revert is
    /// returned instead. When the contract accepts, the removal is sent
    /// and a warning is logged.
    ///
    /// # Errors
    ///
    /// As [`Self::open_maker_v1`], without the margin minimum;
    /// [`ValidationError::LongCloseCapacityExceeded`] or
    /// [`ValidationError::ShortCloseCapacityExceeded`] for a removal the
    /// close depth cannot take, before anything is sent: not transient at
    /// the block read, but the same removal can fit after the open
    /// interest falls or the depth grows;
    /// [`ValidationError::ExcessMakerRemoval`] for more liquidity than the
    /// band holds; [`ContractError::PositionNotFound`] for an id that is
    /// not an open maker when a removal reads it.
    pub async fn adjust_maker_v1(
        &self,
        params: &ExactAdjustMakerParams,
        urgency: Urgency,
    ) -> Result<V1MakerAdjustment> {
        self.require_v1("adjust_maker_v1").await?;
        let removal = if params.liquidity_delta.units() < 0 {
            Some(self.removal_state(params.pos_id).await?)
        } else {
            None
        };
        let call = adjust_call(params, deadline_after(now_ms() / 1_000));
        if let Some((at, state)) = removal
            && let Err(refusal) = check_removal(&state, params.liquidity_delta.magnitude())
        {
            self.confirm_refusal(&at, &call, refusal).await?;
        }
        let receipt = self.send_adjust_maker_v1(&call, urgency).await?;
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
    /// residual is refused here before send. A refusal is confirmed at the
    /// same block as [`Self::adjust_maker_v1`] confirms one, so a
    /// conversion whose margin is too low returns the contract's
    /// `MarginTooLow` even when its residual also exceeds the depth. The
    /// send preflight checks the conversion's margin and health.
    ///
    /// # Errors
    ///
    /// As [`Self::adjust_maker_v1`];
    /// [`TransactionError::MakerNotClosed`], transient, when the close
    /// mined as an adjustment because the band gained liquidity after the
    /// read.
    pub async fn close_maker_v1(&self, pos_id: U256, urgency: Urgency) -> Result<MakerExit> {
        self.require_v1("close_maker_v1").await?;
        let (at, state) = self.removal_state(pos_id).await?;
        let params = ExactAdjustMakerParams {
            pos_id,
            margin_delta: UsdcDelta::ZERO,
            liquidity_delta: state.band.liquidity.negated()?,
            amt0_limit: PerpAtoms::ZERO,
            amt1_limit: UsdcAtoms::ZERO,
        };
        let call = adjust_call(&params, deadline_after(now_ms() / 1_000));
        if let Err(refusal) = check_removal(&state, state.band.liquidity) {
            self.confirm_refusal(&at, &call, refusal).await?;
        }
        let receipt = self.send_adjust_maker_v1(&call, urgency).await?;
        match maker_adjustment(&receipt, self.market.perp(), pos_id)? {
            V1MakerAdjustment::Exit(exit) => Ok(exit),
            V1MakerAdjustment::Adjusted(adjusted) => Err(TransactionError::MakerNotClosed {
                tx_hash: adjusted.tx_hash,
                pos_id,
            }
            .into()),
        }
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

    /// Return `refusal`, or the contract's earlier revert, unless the
    /// contract accepts `call`, the removal that will be sent, at the
    /// block `refusal` was found at.
    async fn confirm_refusal(
        &self,
        at: &StateAt,
        call: &Perp::adjustMakerCall,
        refusal: ValidationError,
    ) -> Result<()> {
        let Some((side, interest, last_maker)) = refused_side(&refusal) else {
            return Err(refusal.into());
        };
        let simulated = at
            .simulate_from(self.address, &call.abi_encode().into())
            .await;
        match confirmed_refusal(refusal, simulated) {
            Some(error) => Err(error),
            None => {
                tracing::warn!(
                    pos_id = %call.params.posId,
                    liquidity_delta = call.params.liquidityDelta,
                    block = at.block().number,
                    ?side,
                    interest,
                    last_maker,
                    "the contract accepts a maker removal the close-depth check refused"
                );
                Ok(())
            }
        }
    }

    /// The band, the position's perp, the open interest, the X64 close
    /// depth, the market's maker liquidity and the pool price, at the head,
    /// with the handle they were read at.
    async fn removal_state(&self, pos_id: U256) -> Result<(StateAt, RemovalState)> {
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
        let removal = RemovalState {
            band: band.ok_or_else(not_found)?,
            perp: position.ok_or_else(not_found)?.perp,
            open_interest,
            depth_x64,
            book,
            sqrt_price,
        };
        Ok((state, removal))
    }

    /// Sign, send and wait for one `adjustMaker`.
    async fn send_adjust_maker_v1(
        &self,
        call: &Perp::adjustMakerCall,
        urgency: Urgency,
    ) -> Result<TransactionReceipt> {
        tracing::debug!(
            pos_id = %call.params.posId,
            margin_delta_atoms = call.params.marginDelta,
            liquidity_delta = call.params.liquidityDelta,
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
    use super::super::tests as taker;
    use super::*;
    use crate::client::{ARBITRUM_CHAIN_ID, ARBITRUM_SEPOLIA_CHAIN_ID};
    use crate::client::{ChainDeployments, Era, mock};
    use crate::contracts::Perp as EarlierPerp;
    use crate::contracts::v1::{FundingState, MakerFunding, OpenInterest};
    use crate::errors::NotReplaceableReason;
    use crate::errors::decode::revert_class;
    use crate::math::range::TickRange;
    use crate::units::LDelta;
    use std::sync::{Arc, Mutex};

    use alloy::consensus::Transaction as _;
    use alloy::consensus::transaction::Recovered;
    use alloy::eips::BlockId;
    use alloy::network::TransactionBuilder as _;
    use alloy::primitives::{I256, U160};
    use tracing::field::{Field, Visit};
    use tracing::instrument::WithSubscriber;
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Level, Metadata, Subscriber};

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

    /// The W12 band at the W12 pool price: in range, so its removal
    /// leaves both depths, by different amounts.
    fn w12_band() -> (SqrtPrice, MakerBand) {
        let price = SqrtPrice::from_x96(U256::from(560_227_709_747_861_399_187_319_382_274u128));
        let band = MakerBand::new(
            TickRange::new(38_700, 39_540).unwrap(),
            LUnits::new(335_000_000),
        );
        (price, band)
    }

    fn removal(
        perp: i128,
        interest: (u128, u128),
        depth: (U256, U256),
        book: u128,
    ) -> RemovalState {
        let (sqrt_price, band) = w12_band();
        RemovalState {
            band,
            perp: PerpDelta::new(perp),
            open_interest: PerSide::new(PerpAtoms::new(interest.0), PerpAtoms::new(interest.1)),
            depth_x64: PerSide::new(depth.0, depth.1),
            book: U256::from(book),
            sqrt_price,
        }
    }

    fn x64(atoms: u128) -> U256 {
        U256::from(atoms) << 64
    }

    type Refusal = Option<(Side, u128, bool)>;

    /// A check's refusal as the side, the interest and `last_maker`.
    fn refused(result: std::result::Result<(), ValidationError>) -> Refusal {
        match result {
            Ok(()) => None,
            Err(ValidationError::LongCloseCapacityExceeded {
                interest,
                last_maker,
            }) => Some((Side::Long, interest, last_maker)),
            Err(ValidationError::ShortCloseCapacityExceeded {
                interest,
                last_maker,
            }) => Some((Side::Short, interest, last_maker)),
            Err(other) => panic!("not a close-depth refusal: {other}"),
        }
    }

    fn long(interest: u128, last_maker: bool) -> Refusal {
        Some((Side::Long, interest, last_maker))
    }

    fn short(interest: u128, last_maker: bool) -> Refusal {
        Some((Side::Short, interest, last_maker))
    }

    /// A partial removal takes only the removed liquidity's contribution,
    /// crosswise (the long depth loses the band's short contribution), and
    /// ignores the perp it does not convert.
    #[test]
    fn a_partial_removal_debits_its_own_contribution_crosswise() {
        let (price, band) = w12_band();
        let removed = LUnits::new(100_000_000);
        let part = band_capacity_x64(price, &MakerBand::new(band.range, removed)).unwrap();
        let whole = band_capacity_x64(price, &band).unwrap();
        assert!(part.long != part.short && part.long < whole.long && part.short < whole.short);
        let book = 2 * band.liquidity.units();
        let (oi_long, oi_short) = (7, 5);
        let depth_long = x64(oi_long) + part.short;
        let depth_short = x64(oi_short) + part.long;
        let check = |long_depth, short_depth| {
            refused(check_removal(
                &removal(
                    -1_000_000_000,
                    (oi_long, oi_short),
                    (long_depth, short_depth),
                    book,
                ),
                removed,
            ))
        };
        assert_eq!(check(depth_long, depth_short), None);
        assert_eq!(
            check(depth_long - U256::ONE, depth_short),
            long(oi_long, false)
        );
        assert_eq!(
            check(depth_long, depth_short - U256::ONE),
            short(oi_short, false)
        );
        // Each side keeps its own crosswise contribution: the sides' depths
        // swapped refuse.
        assert!(check(x64(oi_long) + part.long, x64(oi_short) + part.short).is_some());
    }

    /// A full removal converts the perp left, its stored perp plus the
    /// principal the pool returns, and checks it on its own side after the
    /// interest at the removal.
    #[test]
    fn a_full_removal_checks_the_signed_residual_on_its_side() {
        let (price, band) = w12_band();
        let whole = band_capacity_x64(price, &band).unwrap();
        let (principal, _) = band_amounts(price, &band).unwrap();
        let principal = i128::try_from(principal.atoms()).unwrap();
        let book = 2 * band.liquidity.units();
        let (oi_long, oi_short) = (7, 5);
        let check = |perp, long_depth, short_depth| {
            refused(check_removal(
                &removal(perp, (oi_long, oi_short), (long_depth, short_depth), book),
                band.liquidity,
            ))
        };
        let fits_long = x64(oi_long) + whole.short;
        let fits_short = x64(oi_short) + whole.long;

        // No residual: the interest at the removal alone.
        assert_eq!(check(-principal, fits_long, fits_short), None);
        assert_eq!(
            check(-principal, fits_long - U256::ONE, fits_short),
            long(oi_long, false)
        );

        // A short residual of 4 joins the short interest.
        let with_short = fits_short + x64(4);
        assert_eq!(check(-principal - 4, fits_long, with_short), None);
        assert_eq!(
            check(-principal - 4, fits_long, with_short - U256::ONE),
            short(oi_short + 4, false)
        );

        // A long residual of 3 joins the long interest.
        let with_long = fits_long + x64(3);
        assert_eq!(check(-principal + 3, with_long, fits_short), None);
        assert_eq!(
            check(-principal + 3, with_long - U256::ONE, fits_short),
            long(oi_long + 3, false)
        );

        // The interest at the removal is checked first and named alone.
        assert_eq!(
            check(-principal + 3, fits_long - U256::ONE, fits_short),
            long(oi_long, false)
        );
    }

    /// The last maker's removal clears both depths: it fits only with no
    /// interest and no residual, whatever the depths held.
    #[test]
    fn the_last_maker_closes_only_with_nothing_left_to_cover() {
        let (price, band) = w12_band();
        let (principal, _) = band_amounts(price, &band).unwrap();
        let principal = i128::try_from(principal.atoms()).unwrap();
        let book = band.liquidity.units();
        let deep = (x64(u64::MAX.into()), x64(u64::MAX.into()));
        let check = |perp, interest| {
            refused(check_removal(
                &removal(perp, interest, deep, book),
                band.liquidity,
            ))
        };
        assert_eq!(check(-principal, (0, 0)), None);
        assert_eq!(check(-principal - 1, (0, 0)), short(1, true));
        assert_eq!(check(-principal + 2, (0, 0)), long(2, true));
        assert_eq!(check(-principal, (1, 0)), long(1, true));
        // One more unit of liquidity elsewhere: not the last maker.
        assert_eq!(
            refused(check_removal(
                &removal(-principal - 1, (0, 0), deep, book + 1),
                band.liquidity
            )),
            None
        );
    }

    /// The only maker's partial removal leaves liquidity in its band, so it
    /// is not the last maker: the depths decide, not the interest alone.
    #[test]
    fn a_partial_removal_by_the_only_maker_is_not_the_last() {
        let (price, band) = w12_band();
        let removed = LUnits::new(100_000_000);
        let part = band_capacity_x64(price, &MakerBand::new(band.range, removed)).unwrap();
        let (oi_long, oi_short) = (7, 5);
        let check = |long_depth, short_depth| {
            refused(check_removal(
                &removal(
                    -1_000_000_000,
                    (oi_long, oi_short),
                    (long_depth, short_depth),
                    band.liquidity.units(),
                ),
                removed,
            ))
        };
        let fits = (x64(oi_long) + part.short, x64(oi_short) + part.long);
        assert_eq!(check(fits.0, fits.1), None);
        assert_eq!(check(fits.0 - U256::ONE, fits.1), long(oi_long, false));
    }

    #[test]
    fn a_removal_beyond_the_band_is_refused_before_the_depth() {
        let (_, band) = w12_band();
        let state = removal(0, (0, 0), (U256::ZERO, U256::ZERO), band.liquidity.units());
        let requested = band.liquidity.units() + 1;
        assert!(matches!(
            check_removal(&state, LUnits::new(requested)),
            Err(ValidationError::ExcessMakerRemoval { requested: r, available })
                if r == requested && available == band.liquidity.units()
        ));
    }

    /// The head block the removal reads pin, by number then header; its
    /// hash.
    fn answer_head(rpc: &mock::Rpc) -> B256 {
        rpc.quantity(100);
        rpc.block(100, 1_795_141_000)
    }

    /// Depths the W12 band's removals with interest `(7, 5)` exceed on
    /// the long side.
    fn shallow() -> (U256, U256) {
        (x64(11), x64(13))
    }

    /// Depths every removal fits.
    fn deep() -> (U256, U256) {
        (x64(u64::MAX.into()), x64(u64::MAX.into()))
    }

    /// Depths the W12 band's removal of `removed` with interest `(7, 5)`
    /// fits exactly, and any larger removal exceeds on the long side.
    fn fitting(removed: u128) -> (U256, U256) {
        let (price, band) = w12_band();
        let part =
            band_capacity_x64(price, &MakerBand::new(band.range, LUnits::new(removed))).unwrap();
        (x64(7) + part.short, x64(u64::MAX.into()))
    }

    /// The removal reads after the head, answered in order: the band, the
    /// position row and owner, the open interest `(7, 5)`, both depth
    /// words, the market's maker liquidity `book` and the pool. `owner`
    /// zero is a position that is not open.
    fn answer_removal_rows(rpc: &mock::Rpc, owner: Address, book: u64, depth: (U256, U256)) {
        rpc.raw(&mock::v1_maker_words(38_700, 39_540, 335_000_000));
        rpc.aggregate3(vec![
            mock::ok_row(mock::v1_position_words(
                -979_408,
                -49_477_350,
                100_000_000,
                0,
            )),
            mock::ok_row(mock::returns::<Perp::positionOwnerCall>(&owner)),
        ]);
        rpc.call::<Perp::openInterestCall>(&OpenInterest { long: 7, short: 5 });
        rpc.storage(depth.0);
        rpc.storage(depth.1);
        rpc.call::<Perp::fundingStateCall>(&FundingState {
            baseX96: I256::ZERO,
            inventoryX96: I256::ZERO,
            makerLiquidity: U256::from(book),
        });
        let mut pool = mock::pool_state_at_tick(39_000);
        pool.sqrtPrice = U160::from(560_227_709_747_861_399_187_319_382_274u128);
        rpc.call::<EarlierPerp::poolStateCall>(&pool);
    }

    /// A v1 write's build check, then the removal reads of position 1,
    /// owned by the client, with a second maker's liquidity in the book;
    /// the pinned head's hash.
    fn answer_removal(rpc: &mock::Rpc, client: &PerpClient, depth: (U256, U256)) -> B256 {
        rpc.immutables(Era::V1);
        let hash = answer_head(rpc);
        answer_removal_rows(rpc, client.address(), 405_000_000, depth);
        hash
    }

    /// Each read answers in order: the head, the build, then the rows.
    fn answer_removal_reads(rpc: &mock::Rpc, owner: Address) -> B256 {
        let hash = answer_head(rpc);
        rpc.immutables(Era::V1);
        answer_removal_rows(rpc, owner, 405_000_000, shallow());
        hash
    }

    /// Every input of the removal check comes from one block, each read
    /// into its own field.
    #[tokio::test]
    async fn the_removal_state_reads_each_input_at_one_block() {
        let (client, rpc) = mock::client();
        let hash = answer_removal_reads(&rpc, Address::repeat_byte(1));
        let (at, state) = client.removal_state(U256::ONE).await.unwrap();
        assert_eq!(at.block().hash, hash);
        let (price, band) = w12_band();
        assert_eq!(state.band, band);
        assert_eq!(state.perp, PerpDelta::new(-979_408));
        assert_eq!(
            state.open_interest,
            PerSide::new(PerpAtoms::new(7), PerpAtoms::new(5))
        );
        assert_eq!(
            state.depth_x64,
            PerSide::new(U256::from(11u8) << 64, U256::from(13u8) << 64)
        );
        assert_eq!(state.book, U256::from(405_000_000u64));
        assert_eq!(state.sqrt_price, price);
        let pinned = BlockId::hash(hash);
        let blocks = rpc.call_blocks();
        assert_eq!(blocks[2..], vec![pinned; blocks.len() - 2]);
        assert_eq!(rpc.storage_blocks(), [pinned, pinned]);
        assert!(rpc.is_drained());

        let (client, rpc) = mock::client();
        answer_removal_reads(&rpc, Address::ZERO);
        assert!(matches!(
            client.removal_state(U256::ONE).await,
            Err(PerpCityError::Contract(ContractError::PositionNotFound { pos_id })) if pos_id == U256::ONE
        ));
    }

    fn reverted(selector: [u8; 4]) -> Result<()> {
        Err(TransactionError::SimulationReverted {
            error_name: String::new(),
            selector: selector.into(),
            revert_data: None,
        }
        .into())
    }

    #[test]
    fn a_refusal_stands_only_when_the_contract_reverts_the_same_way() {
        let refusal = || ValidationError::ShortCloseCapacityExceeded {
            interest: 9,
            last_maker: false,
        };
        let same = confirmed_refusal(
            refusal(),
            reverted(Perp::ShortCloseCapacityExceeded::SELECTOR),
        );
        assert!(matches!(
            same,
            Some(PerpCityError::Validation(
                ValidationError::ShortCloseCapacityExceeded {
                    interest: 9,
                    last_maker: false
                }
            ))
        ));
        let margin = confirmed_refusal(refusal(), reverted(Perp::MarginTooLow::SELECTOR));
        assert!(
            matches!(&margin, Some(PerpCityError::Transaction(error)) if error.is_revert::<Perp::MarginTooLow>()),
            "{margin:?}"
        );
        let other_side = confirmed_refusal(
            refusal(),
            reverted(Perp::LongCloseCapacityExceeded::SELECTOR),
        );
        assert!(matches!(
            other_side,
            Some(PerpCityError::Transaction(
                TransactionError::SimulationReverted { .. }
            ))
        ));
        let unanswered = confirmed_refusal(
            ValidationError::LongCloseCapacityExceeded {
                interest: 1,
                last_maker: false,
            },
            Err(TransactionError::GasUnavailable {
                reason: "eth_call failed".into(),
            }
            .into()),
        );
        assert!(unanswered.is_some_and(|error| error.is_transient()));
        let missing = confirmed_refusal(
            refusal(),
            Err(ContractError::BlockUnavailable { number: 100 }.into()),
        );
        assert!(matches!(
            missing,
            Some(PerpCityError::Contract(ContractError::BlockUnavailable {
                number: 100
            }))
        ));
        assert!(confirmed_refusal(refusal(), Ok(())).is_none());
        let excess = ValidationError::ExcessMakerRemoval {
            requested: 2,
            available: 1,
        };
        assert!(matches!(
            confirmed_refusal(excess, Ok(())),
            Some(PerpCityError::Validation(
                ValidationError::ExcessMakerRemoval { .. }
            ))
        ));
    }

    /// The `adjustMaker` in `calldata`.
    fn removal_call(calldata: &Bytes) -> AdjustMakerParams {
        Perp::adjustMakerCall::abi_decode(calldata).unwrap().params
    }

    /// A refused close is confirmed by the contract at the block its reads
    /// were pinned to, from the client to its market with the close's own
    /// calldata, and nothing is sent. The depths fit all of the band but
    /// one unit, so the check is of the whole band.
    #[tokio::test]
    async fn a_refused_close_is_confirmed_at_the_pinned_block() {
        let (client, rpc) = mock::client();
        let all_but_one = fitting(335_000_000 - 1);
        let hash = answer_removal(&rpc, &client, all_but_one);
        rpc.reverts(&Perp::LongCloseCapacityExceeded::SELECTOR);
        let err = client
            .close_maker_v1(U256::ONE, Urgency::Normal)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Validation(ValidationError::LongCloseCapacityExceeded {
                    interest: 7,
                    last_maker: false
                })
            ),
            "{err}"
        );
        let confirmation = rpc.calls().pop().unwrap();
        let call = removal_call(&confirmation);
        assert_eq!(call.posId, U256::ONE);
        assert_eq!(call.liquidityDelta, -335_000_000);
        assert_eq!((call.amt0Limit, call.amt1Limit), (U256::ZERO, U256::ZERO));
        assert_eq!(rpc.call_blocks().pop(), Some(BlockId::hash(hash)));
        assert_eq!(
            rpc.call_routes().pop(),
            Some((Some(client.address()), Some(mock::PERP)))
        );
        assert!(rpc.is_drained());
    }

    /// A refused partial removal is confirmed the same way.
    #[tokio::test]
    async fn a_refused_partial_removal_is_confirmed_at_the_pinned_block() {
        let (client, rpc) = mock::client();
        let hash = answer_removal(&rpc, &client, shallow());
        rpc.reverts(&Perp::LongCloseCapacityExceeded::SELECTOR);
        let err = client
            .adjust_maker_v1(&adjust(), Urgency::Normal)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Validation(ValidationError::LongCloseCapacityExceeded {
                    interest: 7,
                    last_maker: false
                })
            ),
            "{err}"
        );
        let call = removal_call(&rpc.calls().pop().unwrap());
        assert_eq!(call.liquidityDelta, -50_000_000);
        assert_eq!(call.marginDelta, -5_000_000);
        assert_eq!(rpc.call_blocks().pop(), Some(BlockId::hash(hash)));
        assert!(rpc.is_drained());
    }

    /// A confirmation whose node lacks the pinned block is the transient
    /// `BlockUnavailable` a pinned read gives; pruned state is
    /// `StateUnavailable`; an execution error whose message names a
    /// missing block is the call's failure.
    #[tokio::test]
    async fn a_confirmation_without_the_pinned_block_is_typed_for_it() {
        let (client, rpc) = mock::client();
        answer_removal(&rpc, &client, shallow());
        rpc.node_error(-32000, "header not found");
        let err = client
            .close_maker_v1(U256::ONE, Urgency::Normal)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::BlockUnavailable { number: 100 })
            ),
            "{err}"
        );
        assert!(err.is_transient());
        assert!(rpc.is_drained());

        let (client, rpc) = mock::client();
        answer_removal(&rpc, &client, shallow());
        rpc.node_error(-32000, "missing trie node 0xab (path )");
        let err = client
            .close_maker_v1(U256::ONE, Urgency::Normal)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::StateUnavailable { number: 100 })
            ),
            "{err}"
        );
        assert!(!err.is_transient());
        assert!(rpc.is_drained());

        let (client, rpc) = mock::client();
        answer_removal(&rpc, &client, shallow());
        rpc.node_error(3, "execution reverted: unknown block");
        let err = client
            .close_maker_v1(U256::ONE, Urgency::Normal)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Transaction(TransactionError::SimulationFailed { .. })
            ),
            "{err}"
        );
        assert!(!err.is_transient());
        assert!(rpc.is_drained());
    }

    /// `adjustMaker` mined for position 1 as an adjustment that left
    /// `liquidity` in the band.
    fn adjusted_receipt(client: &PerpClient, hash: B256, liquidity: u128) -> TransactionReceipt {
        let event = Perp::MakerAdjusted {
            posId: U256::ONE,
            owner: client.address(),
            marginDelta: 0,
            liquidityDelta: -335_000_000,
            funding: I256::ZERO,
            lpFees: U256::ZERO,
            delta: I256::ZERO,
            margin: 100_000_000,
            liquidity,
            lastCumlFundingX96: I256::ZERO,
            lastCumlFunding: MakerFunding {
                belowX96: I256::ZERO,
                withinInventoryX192: I256::ZERO,
            },
        };
        taker::mined_trade_receipt(client, hash, &event)
    }

    /// A close that mined as an adjustment, the band having gained
    /// liquidity after the read, is the transient `MakerNotClosed` with
    /// its hash. Its cached limit's preflight runs without the buffer.
    #[tokio::test(start_paused = true)]
    async fn a_close_that_mines_as_an_adjustment_is_not_closed() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        client.gas_limit_cache.lock().unwrap().put(
            Perp::adjustMakerCall::SELECTOR,
            750_000,
            now_ms(),
        );
        answer_removal(&rpc, &client, deep());
        let head = rpc.block(101, 1_795_141_002);
        rpc.response(&Bytes::new());
        rpc.response(&mock::SENT);
        rpc.response(&adjusted_receipt(&client, mock::SENT, 20_000_000));
        let err = client
            .close_maker_v1(U256::ONE, Urgency::Normal)
            .await
            .unwrap_err();
        let hash = rpc.sent()[0];
        assert!(
            matches!(err, PerpCityError::Transaction(TransactionError::MakerNotClosed {
                tx_hash, pos_id,
            }) if tx_hash == hash && pos_id == U256::ONE),
            "{err}"
        );
        assert!(err.is_transient());
        assert_eq!(err.tx_hash(), Some(hash));
        assert_eq!(err.hash_to_replace(), None, "it mined");
        let preflight = rpc.calls().pop().unwrap();
        assert_eq!(removal_call(&preflight).liquidityDelta, -335_000_000);
        assert_eq!(rpc.call_blocks().pop(), Some(BlockId::hash(head)));
        assert_eq!(rpc.call_gas().pop(), Some(Some(750_000)));
        let sent = rpc.sent_txs();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].gas_limit(), 900_000, "the cached limit is sent");
        assert_eq!(sent[0].input(), &preflight);
        assert!(rpc.is_drained());
    }

    /// A removal the check refuses and the contract accepts at the pinned
    /// block is sent: the contract is the authority on its own depth. The
    /// signed transaction carries the calldata the contract accepted, its
    /// deadline and nonzero limits included, and one warning names the
    /// overruled refusal.
    #[tokio::test(start_paused = true)]
    async fn a_refusal_the_contract_accepts_is_sent() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let hash = answer_removal(&rpc, &client, shallow());
        rpc.response(&Bytes::new());
        rpc.block(101, 1_795_141_002);
        rpc.quantity(400_000);
        rpc.response(&mock::SENT);
        rpc.response(&adjusted_receipt(&client, mock::SENT, 285_000_000));
        let warnings = Warnings::default();
        let adjusted = client
            .adjust_maker_v1(&adjust(), Urgency::Normal)
            .with_subscriber(warnings.clone())
            .await
            .unwrap();
        assert!(
            matches!(&adjusted, V1MakerAdjustment::Adjusted(result) if result.tx_hash == rpc.sent()[0]),
            "{adjusted:?}"
        );
        let confirmation = rpc.calls().pop().unwrap();
        assert_eq!(rpc.call_blocks().pop(), Some(BlockId::hash(hash)));
        assert_eq!(
            rpc.call_routes().pop(),
            Some((Some(client.address()), Some(mock::PERP)))
        );
        let confirmed = removal_call(&confirmation);
        assert_eq!(
            (confirmed.amt0Limit, confirmed.amt1Limit),
            (U256::from(100u8), U256::from(1_000u16))
        );
        let signed = rpc.sent_txs();
        assert_eq!(signed.len(), 1);
        assert_eq!(signed[0].input(), &confirmation, "the confirmed calldata");
        assert_eq!(signed[0].to(), Some(mock::PERP));
        assert_eq!(
            warnings.events(),
            [vec![
                (
                    "message".to_owned(),
                    "the contract accepts a maker removal the close-depth check refused".to_owned()
                ),
                ("pos_id".to_owned(), "1".to_owned()),
                ("liquidity_delta".to_owned(), "-50000000".to_owned()),
                ("block".to_owned(), "100".to_owned()),
                ("side".to_owned(), "Long".to_owned()),
                ("interest".to_owned(), "7".to_owned()),
                ("last_maker".to_owned(), "false".to_owned()),
            ]]
        );
        assert!(rpc.is_drained());
    }

    /// A partial removal is checked for its own amount: with depths that
    /// fit it and not the whole band, it is sent with no confirmation.
    #[tokio::test(start_paused = true)]
    async fn a_partial_removal_is_checked_for_its_own_amount() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        answer_removal(&rpc, &client, fitting(50_000_000));
        rpc.block(101, 1_795_141_002);
        rpc.quantity(400_000);
        rpc.response(&mock::SENT);
        rpc.response(&adjusted_receipt(&client, mock::SENT, 285_000_000));
        client
            .adjust_maker_v1(&adjust(), Urgency::Normal)
            .await
            .unwrap();
        assert_eq!(
            rpc.methods(),
            [
                "eth_call",
                "eth_call",
                "eth_blockNumber",
                "eth_getBlockByNumber",
                "eth_call",
                "eth_call",
                "eth_call",
                "eth_getStorageAt",
                "eth_getStorageAt",
                "eth_call",
                "eth_call",
                "eth_getBlockByNumber",
                "eth_estimateGas",
                "eth_sendRawTransaction",
                "eth_getTransactionReceipt"
            ]
        );
        assert!(rpc.is_drained());
    }

    /// An event's fields, by name and value, in order.
    type EventFields = Vec<(String, String)>;

    /// The warnings logged under it, each as its fields in order.
    #[derive(Clone, Default)]
    struct Warnings(Arc<Mutex<Vec<EventFields>>>);

    impl Warnings {
        fn events(&self) -> Vec<EventFields> {
            self.0.lock().unwrap().clone()
        }
    }

    struct Fields(EventFields);

    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.push((field.name().to_owned(), format!("{value:?}")));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.push((field.name().to_owned(), value.to_owned()));
        }
    }

    impl Subscriber for Warnings {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() <= Level::WARN
        }

        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _: &Id, _: &Record<'_>) {}

        fn record_follows_from(&self, _: &Id, _: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut fields = Fields(Vec::new());
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }

        fn enter(&self, _: &Id) {}

        fn exit(&self, _: &Id) {}
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

    fn stuck_removal(deadline: u64) -> Vec<u8> {
        adjust_call(&adjust(), deadline).abi_encode()
    }

    /// A maker removal's preflight revert keeps the original's hash and
    /// nonce and the revert's selector and class.
    #[tokio::test]
    async fn a_maker_preflight_revert_keeps_its_class_and_the_original() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (hash, tx) = taker::pending_trade(&client, stuck_removal(taker::NOW + 600)).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.block(taker::HEAD, taker::NOW);
        rpc.reverts(&Perp::ShortCloseCapacityExceeded::SELECTOR);
        rpc.reverts(&Perp::ShortCloseCapacityExceeded::SELECTOR);
        taker::nothing_settled(&rpc, taker::STUCK_NONCE, 1);
        let err = client
            .replace_v1_maker(hash, Urgency::Normal)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
                original, nonce: Some(taker::STUCK_NONCE), replacement: None, refused: false,
                source: Some(source),
            }) if *original == hash && source.is_simulation_revert()),
            "{err}"
        );
        assert_eq!(err.hash_to_replace(), Some(hash));
        let class = revert_class(Perp::ShortCloseCapacityExceeded::SELECTOR);
        assert_eq!(err.revert_class(), Some(class));
        assert_eq!(err.is_transient(), class.is_transient());
        assert!(err.is_unconfirmed());
        let PerpCityError::Transaction(inner) = &err else {
            unreachable!()
        };
        assert!(inner.is_revert::<Perp::ShortCloseCapacityExceeded>());
        assert!(rpc.is_drained());
    }

    /// A maker write's look-up answer is checked as a taker trade's: one
    /// signed by another key with the client's address as `from` is
    /// `WrongSender`, and one of the client's that hashes to another hash
    /// is `UnknownToNode`. Nothing is signed or sent after either.
    #[tokio::test]
    async fn a_maker_lookup_answer_is_checked_against_the_signature_and_the_hash() {
        let (client, _) = mock::client();
        let forged = taker::trade_request(&client, stuck_removal(taker::NOW + 600));
        let (forged_hash, forged) =
            taker::as_seen(&client, forged, taker::other_signer().address(), None).await;
        let mut forged = forged;
        forged.inner = Recovered::new_unchecked(forged.inner.into_inner(), client.address());
        let (_, own) = taker::pending_trade(&client, stuck_removal(taker::NOW + 600)).await;
        for (asked, answer, expected) in [
            (forged_hash, forged, NotReplaceableReason::WrongSender),
            (STUCK, own, NotReplaceableReason::UnknownToNode),
        ] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            rpc.immutables(Era::V1);
            rpc.response(&answer);
            let err = client
                .replace_v1_maker(asked, Urgency::Normal)
                .await
                .unwrap_err();
            assert_eq!(taker::refusal(&err), expected, "{err}");
            assert_eq!(err.tx_hash(), Some(asked));
            assert_eq!(err.hash_to_replace(), None);
            assert!(rpc.sent().is_empty(), "{expected}: nothing signed or sent");
            assert!(rpc.is_drained(), "{expected}: nothing after the look-up");
        }
    }

    /// The node failing the look-up leaves the original pending: its hash,
    /// unconfirmed and transient.
    #[tokio::test]
    async fn a_failed_maker_lookup_keeps_the_original() {
        let (client, rpc) = mock::client();
        let hash = B256::repeat_byte(0x5b);
        rpc.immutables(Era::V1);
        rpc.fails("upstream timeout");
        let err = client
            .replace_v1_maker(hash, Urgency::Normal)
            .await
            .unwrap_err();
        assert!(
            matches!(err, PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
                original, nonce: None, replacement: None, refused: false, source: Some(_),
            }) if original == hash),
            "{err}"
        );
        assert_eq!(err.hash_to_replace(), Some(hash));
        assert!(err.is_transient() && err.is_unconfirmed());
        assert_eq!(err.tx_hash(), Some(hash));
        assert!(rpc.is_drained());
    }

    /// A pending EIP-7702 maker write is not signed again; nothing follows
    /// the look-up.
    #[tokio::test]
    async fn a_pending_type_4_maker_write_is_refused() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let mut request = taker::trade_request(&client, stuck_removal(taker::NOW + 600));
        request.authorization_list = Some(Vec::new());
        let (hash, tx) = taker::as_seen(&client, request, client.address(), None).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        let err = client
            .replace_v1_maker(hash, Urgency::Normal)
            .await
            .unwrap_err();
        assert_eq!(
            taker::refusal(&err),
            NotReplaceableReason::NotEip1559 { tx_type: 4 }
        );
        assert!(err.is_unconfirmed() && !err.is_transient());
        assert!(rpc.is_drained());
    }

    /// A maker write signed for another chain, or one that carried value,
    /// is refused before anything is preflighted, signed or sent, as a
    /// taker trade is: the call is not signed again on this chain, and the
    /// replacement carries no value.
    #[tokio::test]
    async fn a_maker_write_for_another_chain_or_with_value_is_refused() {
        let (client, _) = mock::client();
        let chain = client.chain().chain_id();
        let input = || stuck_removal(taker::NOW + 600);
        for (request, expected) in [
            (
                taker::trade_request(&client, input()).with_chain_id(chain + 1),
                NotReplaceableReason::OtherChain {
                    chain_id: Some(chain + 1),
                },
            ),
            (
                taker::trade_request(&client, input()).with_value(U256::from(1)),
                NotReplaceableReason::UnsupportedCall,
            ),
            (
                taker::trade_request(
                    &client,
                    open_call(client.address(), &open(), taker::NOW + 600).abi_encode(),
                )
                .with_value(U256::from(1)),
                NotReplaceableReason::UnsupportedCall,
            ),
        ] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            let (hash, tx) = taker::as_seen(&client, request, client.address(), None).await;
            rpc.immutables(Era::V1);
            rpc.response(&tx);
            let err = client
                .replace_v1_maker(hash, Urgency::Normal)
                .await
                .unwrap_err();
            assert_eq!(taker::refusal(&err), expected, "{err}");
            assert_eq!(err.tx_hash(), Some(hash));
            assert!(err.is_unconfirmed() && !err.is_transient());
            assert!(rpc.sent().is_empty(), "{expected}: nothing signed or sent");
            assert!(rpc.is_drained(), "{expected}: nothing after the look-up");
        }
    }

    /// A maker replacement is preflighted at the stuck limit in the block
    /// after the head, where it accrues as the mined replacement does. A
    /// revert at that limit that passes without the cap is a limit too
    /// low; that, a failed head read and a refusal of the block overrides
    /// twice (the preflight never runs without them) are checked against
    /// the original's receipt, and nothing is signed.
    #[tokio::test]
    async fn a_maker_replacement_preflights_the_stuck_limit_in_the_next_block() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (hash, tx) = taker::pending_trade(&client, stuck_removal(taker::NOW + 600)).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        let head = rpc.block(taker::HEAD, taker::NOW);
        rpc.raw(&[]);
        rpc.fails("replacement transaction underpriced");
        taker::nothing_settled(&rpc, taker::STUCK_NONCE, 2);
        let err = client
            .replace_v1_maker(hash, Urgency::Normal)
            .await
            .unwrap_err();
        assert!(err.is_unconfirmed(), "{err}");
        let calls = rpc.params("eth_call");
        let preflight = calls.last().unwrap();
        assert_eq!(preflight[1], serde_json::json!({"blockHash": head}));
        assert_eq!(
            preflight[3],
            serde_json::json!({
                "number": format!("{:#x}", taker::HEAD + 1),
                "time": format!("{:#x}", taker::NOW + 1),
            })
        );
        assert_eq!(rpc.call_gas().last(), Some(&Some(taker::STUCK_GAS)));
        assert_eq!(rpc.sent().len(), 1);
        assert!(rpc.is_drained());

        for case in ["head read fails", "too low", "overrides refused"] {
            let (client, rpc) = mock::client();
            client.chain().set_base_fee(100);
            let (hash, tx) = taker::pending_trade(&client, stuck_removal(taker::NOW + 600)).await;
            rpc.immutables(Era::V1);
            rpc.response(&tx);
            match case {
                "head read fails" => rpc.fails("upstream timeout"),
                "too low" => {
                    rpc.block(taker::HEAD, taker::NOW);
                    rpc.reverts(&TRANSFER_FROM_FAILED);
                    rpc.raw(&[]);
                }
                _ => {
                    for _ in 0..2 {
                        rpc.block(taker::HEAD, taker::NOW);
                        rpc.node_error(-32602, "too many arguments, want at most 3");
                    }
                }
            }
            taker::nothing_settled(&rpc, taker::STUCK_NONCE, 1);
            let err = client
                .replace_v1_maker(hash, Urgency::Normal)
                .await
                .unwrap_err();
            let PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
                original,
                replacement: None,
                source: Some(source),
                ..
            }) = &err
            else {
                panic!("{err}");
            };
            assert_eq!(*original, hash);
            if case == "overrides refused" {
                assert!(
                    matches!(
                        source.as_ref(),
                        PerpCityError::Transaction(TransactionError::GasUnavailable { .. })
                    ),
                    "{source}"
                );
                let preflights: Vec<usize> = rpc
                    .params("eth_call")
                    .iter()
                    .filter(|call| call[0].get("gas").is_some())
                    .map(|call| call.as_array().map_or(0, Vec::len))
                    .collect();
                assert_eq!(preflights, [4, 4]);
            }
            if case != "too low" {
                assert!(err.is_transient(), "{err}");
            } else {
                assert!(
                    matches!(
                        source.as_ref(),
                        PerpCityError::Transaction(TransactionError::GasLimitTooLow {
                            gas_limit: taker::STUCK_GAS,
                            ..
                        })
                    ),
                    "{source}"
                );
                assert_eq!(
                    rpc.call_gas()[rpc.call_gas().len() - 2..],
                    [Some(taker::STUCK_GAS), None]
                );
            }
            assert!(rpc.sent().is_empty());
            assert!(rpc.is_drained());
        }
    }

    /// A receipt of another hash is no answer for the hash asked for, in a
    /// maker replacement's look after a refused broadcast.
    #[tokio::test]
    async fn a_maker_receipt_of_another_hash_is_no_answer() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        let (hash, tx) = taker::pending_trade(&client, stuck_removal(taker::NOW + 600)).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.block(taker::HEAD, taker::NOW);
        rpc.raw(&[]);
        rpc.fails("replacement transaction underpriced");
        rpc.quantity(taker::STUCK_NONCE + 1);
        rpc.foreign_receipt(&adjusted_receipt(&client, B256::repeat_byte(0x0e), 0));
        rpc.response(&None::<TransactionReceipt>);
        let err = client
            .replace_v1_maker(hash, Urgency::Normal)
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
        assert!(rpc.is_drained());
    }

    /// The maker writes classify a failed era read by the taker writes'
    /// rule: no answer (a timeout, a bare internal error, a gateway with
    /// no backend) is transient; revert data under any code is the
    /// contract's answer.
    #[tokio::test]
    async fn the_maker_era_read_tells_an_answer_from_no_answer() {
        for (failure, transient) in [
            ("upstream timeout", true),
            ("bare internal error", true),
            ("no backends", true),
            ("internal error with revert data", false),
            ("code 3", false),
        ] {
            for write in ["open", "close", "replace"] {
                let (client, rpc) = mock::client();
                match failure {
                    "upstream timeout" => rpc.fails("upstream timeout"),
                    "bare internal error" => rpc.answers_error(-32_603, "internal error", None),
                    "no backends" => rpc.answers_error(-32_011, "no backends available", None),
                    "internal error with revert data" => {
                        rpc.answers_error(-32_603, "internal error", Some(r#""0xbcffc83f""#));
                    }
                    _ => rpc.reverts(&[]),
                }
                let err = match write {
                    "open" => client
                        .open_maker_v1(&open(), Urgency::Normal)
                        .await
                        .unwrap_err(),
                    "close" => client
                        .close_maker_v1(U256::ONE, Urgency::Normal)
                        .await
                        .unwrap_err(),
                    _ => {
                        let err = client
                            .replace_v1_maker(STUCK, Urgency::Normal)
                            .await
                            .unwrap_err();
                        assert!(err.is_unconfirmed(), "{failure}: {err}");
                        assert_eq!(err.hash_to_replace(), Some(STUCK));
                        err
                    }
                };
                assert_eq!(err.is_transient(), transient, "{failure}, {write}: {err}");
                assert!(rpc.is_drained(), "{failure}, {write}");
            }
        }
    }

    /// A maker close whose receipt poll first gets another hash's receipt
    /// waits for its own: the outcome, `MakerNotClosed` here, names the
    /// close's own hash.
    #[tokio::test(start_paused = true)]
    async fn a_maker_close_waits_for_its_own_receipt() {
        let (client, rpc) = mock::client();
        client.chain().set_base_fee(100);
        answer_removal(&rpc, &client, deep());
        rpc.block(101, 1_795_141_002);
        rpc.quantity(600_000);
        rpc.response(&mock::SENT);
        rpc.foreign_receipt(&adjusted_receipt(&client, B256::repeat_byte(0x0e), 0));
        rpc.response(&adjusted_receipt(&client, mock::SENT, 20_000_000));
        let err = client
            .close_maker_v1(U256::ONE, Urgency::Normal)
            .await
            .unwrap_err();
        let hash = rpc.sent()[0];
        assert!(
            matches!(err, PerpCityError::Transaction(TransactionError::MakerNotClosed {
                tx_hash, ..
            }) if tx_hash == hash),
            "{err}"
        );
        assert!(rpc.is_drained());
    }

    /// A mined maker write that used its whole limit is out of gas, and the
    /// `adjustMaker` estimate is evicted.
    #[tokio::test]
    async fn a_mined_maker_write_out_of_gas_evicts_its_estimate() {
        let selector = Perp::adjustMakerCall::SELECTOR;
        let (client, rpc) = mock::client();
        client
            .gas_limit_cache
            .lock()
            .unwrap()
            .put(selector, 150_000, now_ms());
        let request = taker::trade_request(&client, stuck_removal(taker::NOW + 600));
        let (hash, tx) = taker::as_seen(&client, request, client.address(), Some(92)).await;
        rpc.immutables(Era::V1);
        rpc.response(&tx);
        rpc.response(&taker::failed_receipt(&client, hash, taker::STUCK_GAS));
        let err = client
            .replace_v1_maker(hash, Urgency::Normal)
            .await
            .unwrap_err();
        assert!(
            matches!(err, PerpCityError::Transaction(TransactionError::OutOfGas {
                tx_hash, gas_used: taker::STUCK_GAS, gas_limit: taker::STUCK_GAS,
            }) if tx_hash == hash),
            "{err}"
        );
        assert_eq!(
            client
                .gas_limit_cache
                .lock()
                .unwrap()
                .get(&selector, now_ms()),
            None
        );
        assert!(rpc.is_drained());
    }

    const STUCK: B256 = B256::repeat_byte(0x5a);

    /// Solady's `TransferFromFailed()`, which a transfer that runs out of
    /// gas inside the token reverts with.
    const TRANSFER_FROM_FAILED: [u8; 4] = [0x79, 0x39, 0xf4, 0x24];

    /// The cause of a replacement refused before the look-up, which keeps
    /// the given hash, unresolved.
    fn kept_cause(err: PerpCityError) -> PerpCityError {
        assert!(err.is_unconfirmed() && !err.is_transient(), "{err}");
        assert_eq!(err.tx_hash(), Some(STUCK));
        assert_eq!(
            err.hash_to_replace(),
            None,
            "nothing to replace on this build"
        );
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
