//! A market's state as the fold of its events: what the pinned reads
//! answer, rebuilt from the tape instead of from storage, one event at a
//! time.
//!
//! Every quantity here is a total the contract emitted (the latest write
//! wins), a sum of its deltas, or a first occurrence, so the fold
//! [`combine`](Fold::combine)s across segments of the tape and a fold of
//! any prefix is a checkpoint. The accessors return the same types the
//! reads on [`StateAt`](crate::StateAt) return, so a value rebuilt here is
//! compared to a value read there with `==`, not approximately; that
//! comparison is the test this type exists to pass.
//!
//! What the tape cannot carry is counted, never guessed: [`Gaps`] says how
//! many times a figure may have moved without an event saying so, and a
//! consumer gates on it.

use alloy::primitives::{Address, B256};

use crate::client::{Era, OpenInterest, SolvencyState};
use crate::errors::ValidationError;
use crate::events::{CumulativesInfo, MarketEvent, ModuleKind};
use crate::math::BlockContext;
use crate::math::capacity::{Capacity, MarketCapacity};
use crate::math::pricing::{Emas, Mark};
use crate::units::{FundingRate, PerSide, Price, UsdcAtoms, UtilizationRate};

use super::fold::Fold;
use super::tape::{ChainPoint, OwnershipLog, TapeEvent};

/// How many times a figure may have moved without an event saying so,
/// since the last event that stated it. A reading with a nonzero gap is
/// forensic, not a decision's input.
///
/// Both count silences in the live contract builds: the next build emits
/// what repaid the debt on every swap and the margin total on every path
/// that moves it, and these go with it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gaps {
    /// Moves of `totalMargin` no event carries, since the last
    /// `MarginTransferred`: a donation adds the debt it repaid, a bad-debt
    /// booking adds the insurance it consumed, a swap's fees leave it on a
    /// build the fold was not told, and on `v0.2.2` a swap while debt
    /// stands removes less than its gross fees by what repaid the debt.
    pub total_margin_unemitted: u32,
    /// Swaps that paid an insurance fee while bad debt stood, since the
    /// last event that stated the debt. The fee repays debt first, and no
    /// event on the live builds carries the amount.
    pub bad_debt_unemitted: u32,
}

/// A market rebuilt from its tape.
///
/// `apply` one [`TapeEvent`] at a time, from [`market_tape`](super::market_tape)
/// or from the stamped feed; the accessors answer at the last event applied.
/// Accessors whose read type carries a block take the caller's
/// [`BlockContext`], so a rebuilt snapshot compares equal to a pinned read
/// at that block.
///
/// The one place the fold must know the market's [`Era`] is the margin
/// total: every taker swap removes its protocol, creator and insurance fees
/// from `totalMargin` with no event, and whether the `MarginTransferred` of
/// the same transaction was emitted before or after that removal depends on
/// the build and the path. A fold told the era applies the build's rule and
/// is exact while no debt stands; a fold not told it counts every such
/// swap as a silence instead.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Replay {
    perp: Address,
    era: Option<Era>,
    events: u64,
    last: Option<(ChainPoint, BlockContext)>,
    pool_price: Option<Price>,
    index: Option<Price>,
    emas: Option<Emas>,
    funding_per_day: Option<FundingRate>,
    util_fee_per_day: Option<PerSide<UtilizationRate>>,
    cumulatives: Option<CumulativesInfo>,
    capacity: Option<Capacity>,
    open_interest: Option<OpenInterest>,
    /// The margin total as last stated by `MarginTransferred`.
    total_margin_stated: Option<UsdcAtoms>,
    /// Swap fees the build removed from the total since it was last stated.
    fees_removed_since_stated: UsdcAtoms,
    /// The `MarginTransferred` of the current transaction, if one has been
    /// seen: its transaction and whether it withdrew or moved nothing.
    margin_transfer_in_tx: Option<(B256, bool)>,
    bad_debt: Option<UsdcAtoms>,
    modules: [Option<Address>; 6],
    custody: OwnershipLog,
    /// Moves of the total no event carried, since it was last stated.
    total_margin_unemitted: u32,
    /// Swaps with an insurance fee since the last event that stated the
    /// debt; whether they were silences is decided at the read.
    insurance_swaps_since_debt_stated: u32,
    /// Whether this segment stated the margin total; decides how the
    /// counts combine.
    stated_total_margin: bool,
    /// Whether this segment stated the bad debt.
    stated_bad_debt: bool,
}

impl Replay {
    /// The market before its first event, on the build `era` names. What is
    /// zero before any event is zero and stated: no capacity, no open
    /// interest, no margin, no debt. What only the factory's creation log
    /// carries — the modules, the first price, the first EMAs — is unknown
    /// until the market's own events state it.
    pub fn from_genesis(perp: Address, era: Era) -> Self {
        Self {
            perp,
            era: Some(era),
            capacity: Some(PerSide::default()),
            open_interest: Some(PerSide::default()),
            total_margin_stated: Some(UsdcAtoms::ZERO),
            bad_debt: Some(UsdcAtoms::ZERO),
            stated_total_margin: true,
            stated_bad_debt: true,
            ..Self::default()
        }
    }

    /// A fold over a segment that is not the market's start, on the build
    /// `era` names: every total is unknown until the segment states it.
    /// [`Fold::fold`] is the same without the era, and counts every swap's
    /// fee removal as a silence.
    pub fn segment(era: Era) -> Self {
        Self {
            era: Some(era),
            ..Self::default()
        }
    }

    /// The build this fold applies the rules of, if it was told.
    pub fn era(&self) -> Option<Era> {
        self.era
    }

    /// The market this is a replay of.
    pub fn perp(&self) -> Address {
        self.perp
    }

    /// Events applied so far.
    pub fn applied(&self) -> u64 {
        self.events
    }

    /// Where the fold stands: the last event's chain point.
    pub fn point(&self) -> Option<ChainPoint> {
        self.last.map(|(point, _)| point)
    }

    /// The block of the last event applied.
    pub fn block(&self) -> Option<BlockContext> {
        self.last.map(|(_, block)| block)
    }

    /// The pool price after the last swap.
    pub fn pool_price(&self) -> Option<Price> {
        self.pool_price
    }

    /// The beacon's last print.
    pub fn index(&self) -> Option<Price> {
        self.index
    }

    /// The stored EMA pair and the touch it is current as of, as the last
    /// `RatesAndEmasRefreshed` left them.
    pub fn emas(&self) -> Option<Emas> {
        self.emas
    }

    /// The funding rate the last touch set.
    pub fn funding_per_day(&self) -> Option<FundingRate> {
        self.funding_per_day
    }

    /// The utilization fee rates the last touch set, per side.
    pub fn util_fee_per_day(&self) -> Option<PerSide<UtilizationRate>> {
        self.util_fee_per_day
    }

    /// The market's accumulators at the last accrual.
    pub fn cumulatives(&self) -> Option<CumulativesInfo> {
        self.cumulatives
    }

    /// Taker open interest, per side.
    pub fn open_interest(&self) -> Option<OpenInterest> {
        self.open_interest
    }

    /// Capacity and its draw at `block`, as [`StateAt::capacity`](crate::StateAt::capacity)
    /// reads them; `None` until both have been stated.
    pub fn capacity_at(&self, block: BlockContext) -> Option<MarketCapacity> {
        Some(MarketCapacity {
            block,
            capacity: self.capacity?,
            open_interest: self.open_interest?,
        })
    }

    /// What the contract marks from at `block`, as [`StateAt::mark`](crate::StateAt::mark)
    /// reads it: the pool price, the index and the stored EMAs advanced to
    /// the block's timestamp over `ema_window`. `None` until a swap, a
    /// print and a touch have all been seen.
    ///
    /// # Errors
    ///
    /// As [`Mark::advanced`].
    pub fn mark_at(
        &self,
        block: BlockContext,
        ema_window: u64,
    ) -> Result<Option<Mark>, ValidationError> {
        let (Some(pool_price), Some(index), Some(emas)) = (self.pool_price, self.index, self.emas)
        else {
            return Ok(None);
        };
        Mark::advanced(
            block,
            pool_price,
            index,
            emas.pair()?,
            emas.last_touch,
            ema_window,
        )
        .map(Some)
    }

    /// The solvency books: the debt as last stated, the margin total as
    /// last stated less the swap fees the build removed since. `None` until
    /// both have been stated. Read [`Self::gaps`] beside it.
    pub fn solvency(&self) -> Option<SolvencyState> {
        Some(SolvencyState {
            bad_debt: self.bad_debt?,
            total_margin: self
                .total_margin_stated?
                .saturating_sub(self.fees_removed_since_stated),
        })
    }

    /// The module of `kind` in force, once a `ModuleSet` has named one.
    pub fn module(&self, kind: ModuleKind) -> Option<Address> {
        self.modules[module_index(kind)]
    }

    /// Who held each position when: the custody fold, taken in the same
    /// pass.
    pub fn custody(&self) -> &OwnershipLog {
        &self.custody
    }

    /// What may have moved without an event saying so.
    ///
    /// A swap's insurance fee repays debt only while debt stands, so the
    /// swaps counted since the debt was last stated are a silence only when
    /// that statement was nonzero. Deciding that here, from the latest
    /// total, is what lets the count itself combine across segments.
    pub fn gaps(&self) -> Gaps {
        let debt_stands = self.bad_debt.is_some_and(|debt| !debt.is_zero());
        // On `v0.2.2` a swap while debt stands removes its credited fees,
        // the gross less what repaid the debt; the fold removed the gross.
        let fees_overstated = debt_stands && self.era == Some(Era::Upgradeable);
        Gaps {
            total_margin_unemitted: self.total_margin_unemitted
                + if fees_overstated {
                    self.insurance_swaps_since_debt_stated
                } else {
                    0
                },
            bad_debt_unemitted: if debt_stands {
                self.insurance_swaps_since_debt_stated
            } else {
                0
            },
        }
    }

    /// Whether this taker event's swap fees left the margin total after the
    /// transaction's `MarginTransferred`, so the fold must remove them, or
    /// before it, so the stated total already has them out. `None` when the
    /// fold was not told the build.
    fn swap_fees_removed_after_statement(&self, event: &TapeEvent) -> Option<bool> {
        let preceded_by_withdrawal = matches!(
            self.margin_transfer_in_tx,
            Some((tx, nonpositive)) if tx == event.tx_hash && nonpositive
        );
        Some(match (self.era?, &event.event) {
            // Both builds: the deposit is transferred, then the fees leave.
            (_, MarketEvent::TakerOpened { .. }) => true,
            // `58b42b7` removes the fees before it transfers margin, on every
            // adjust, close and liquidation, so the statement already has
            // them out.
            (Era::Legacy, _) => false,
            // `v0.2.2` transfers a deposit before removing the fees and a
            // withdrawal after; a liquidation transfers its fee after the
            // close event, so nothing preceded.
            (Era::Upgradeable, _) => !preceded_by_withdrawal,
        })
    }
}

fn module_index(kind: ModuleKind) -> usize {
    match kind {
        ModuleKind::Beacon => 0,
        ModuleKind::Fees => 1,
        ModuleKind::Funding => 2,
        ModuleKind::MarginRatios => 3,
        ModuleKind::PriceImpact => 4,
        ModuleKind::Pricing => 5,
    }
}

impl Fold for Replay {
    fn apply(&mut self, event: &TapeEvent) {
        let point = event.point();
        debug_assert!(
            self.last.is_none_or(|(last, _)| last < point),
            "tape out of chain order at {point:?}"
        );
        self.last = Some((
            point,
            BlockContext {
                number: event.block_number,
                hash: event.block_hash,
                timestamp: event.timestamp,
            },
        ));
        self.events += 1;

        match event.event {
            MarketEvent::TakerOpened { swap, .. }
            | MarketEvent::TakerAdjusted { swap, .. }
            | MarketEvent::TakerClosed { swap, .. } => {
                self.pool_price = Some(swap.pool_price);
                if !swap.insurance_fee.is_zero() {
                    self.insurance_swaps_since_debt_stated += 1;
                }
                // The swap's protocol, creator and insurance fees leave the
                // margin total with no event; the LP fee stays, it is the
                // makers'.
                let removed = swap.protocol_fee + swap.creator_fee + swap.insurance_fee;
                if !removed.is_zero() {
                    match self.swap_fees_removed_after_statement(event) {
                        Some(true) => self.fees_removed_since_stated += removed,
                        Some(false) => {}
                        None => self.total_margin_unemitted += 1,
                    }
                }
            }
            MarketEvent::CapacityUpdated { capacity } => self.capacity = Some(capacity),
            MarketEvent::OpenInterestUpdated { open_interest } => {
                self.open_interest = Some(open_interest);
            }
            MarketEvent::CumulativesAccrued { cumulatives } => {
                self.cumulatives = Some(cumulatives);
            }
            MarketEvent::RatesAndEmasRefreshed {
                funding_per_day,
                util_fee_per_day,
                last_touch,
                pool_price_ema,
                index_ema,
            } => {
                self.funding_per_day = Some(funding_per_day);
                self.util_fee_per_day = Some(util_fee_per_day);
                self.emas = Some(Emas {
                    amm_price: pool_price_ema,
                    index: index_ema,
                    last_touch,
                });
            }
            MarketEvent::IndexUpdated { index } => self.index = Some(index),
            MarketEvent::MarginTransferred {
                total_margin,
                margin_delta,
            } => {
                self.total_margin_stated = Some(total_margin);
                self.fees_removed_since_stated = UsdcAtoms::ZERO;
                self.stated_total_margin = true;
                self.total_margin_unemitted = 0;
                let nonpositive = margin_delta.is_negative() || margin_delta.is_zero();
                self.margin_transfer_in_tx = Some((event.tx_hash, nonpositive));
            }
            MarketEvent::BadDebtAccounted { bad_debt_after, .. } => {
                self.state_bad_debt(bad_debt_after);
                // The insurance it consumed was added to the margin total,
                // and nothing says how much.
                self.total_margin_unemitted += 1;
            }
            MarketEvent::LossSocialized { bad_debt_after, .. } => {
                self.state_bad_debt(bad_debt_after);
            }
            MarketEvent::Donated { bad_debt, .. } => {
                self.state_bad_debt(bad_debt);
                // The debt it repaid was added to the margin total, and
                // nothing says how much.
                self.total_margin_unemitted += 1;
            }
            MarketEvent::ModuleSet { module, address } => {
                self.modules[module_index(module)] = Some(address);
            }
            MarketEvent::PositionTransferred { .. } => self.custody.apply(event),
            MarketEvent::MakerOpened { .. }
            | MarketEvent::MakerAdjusted { .. }
            | MarketEvent::MakerConverted { .. }
            | MarketEvent::MakerClosed { .. }
            | MarketEvent::MakerLiquidated { .. }
            | MarketEvent::MakerBackstopped { .. }
            | MarketEvent::TakerLiquidated { .. }
            | MarketEvent::TakerBackstopped { .. }
            | MarketEvent::TicksCrossed { .. }
            | MarketEvent::TickInitialized { .. }
            | MarketEvent::TickDeleted { .. }
            | MarketEvent::ModifyLiquidity { .. } => {}
        }
    }

    fn combine(&mut self, later: Self) {
        debug_assert!(
            self.last
                .zip(later.last)
                .is_none_or(|((a, _), (b, _))| a < b),
            "segments combined out of order"
        );
        if self.perp.is_zero() {
            self.perp = later.perp;
        }
        self.era = later.era.or(self.era);
        self.events += later.events;
        self.last = later.last.or(self.last);
        self.margin_transfer_in_tx = later.margin_transfer_in_tx.or(self.margin_transfer_in_tx);
        self.pool_price = later.pool_price.or(self.pool_price);
        self.index = later.index.or(self.index);
        self.emas = later.emas.or(self.emas);
        self.funding_per_day = later.funding_per_day.or(self.funding_per_day);
        self.util_fee_per_day = later.util_fee_per_day.or(self.util_fee_per_day);
        self.cumulatives = later.cumulatives.or(self.cumulatives);
        self.capacity = later.capacity.or(self.capacity);
        self.open_interest = later.open_interest.or(self.open_interest);
        self.bad_debt = later.bad_debt.or(self.bad_debt);
        for (mine, theirs) in self.modules.iter_mut().zip(later.modules) {
            *mine = theirs.or(*mine);
        }
        self.custody.combine(later.custody);
        // A later segment that stated a total replaces the count of
        // silences before it; one that did not adds its own.
        if later.stated_total_margin {
            self.total_margin_stated = later.total_margin_stated;
            self.fees_removed_since_stated = later.fees_removed_since_stated;
            self.total_margin_unemitted = later.total_margin_unemitted;
            self.stated_total_margin = true;
        } else {
            self.fees_removed_since_stated += later.fees_removed_since_stated;
            self.total_margin_unemitted += later.total_margin_unemitted;
        }
        if later.stated_bad_debt {
            self.insurance_swaps_since_debt_stated = later.insurance_swaps_since_debt_stated;
            self.stated_bad_debt = true;
        } else {
            self.insurance_swaps_since_debt_stated += later.insurance_swaps_since_debt_stated;
        }
    }
}

impl Replay {
    fn state_bad_debt(&mut self, bad_debt: UsdcAtoms) {
        self.bad_debt = Some(bad_debt);
        self.stated_bad_debt = true;
        self.insurance_swaps_since_debt_stated = 0;
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{B256, U256};

    use super::*;
    use crate::constants::Q96;
    use crate::events::SwapInfo;
    use crate::math::pricing::calculate_emas;
    use crate::units::{PerpAtoms, PerpDelta, UsdcDelta};

    fn row(block: u64, log_index: u64, event: MarketEvent) -> TapeEvent {
        TapeEvent {
            block_number: block,
            block_hash: B256::with_last_byte(block as u8),
            log_index,
            timestamp: 1_700_000_000 + block * 10,
            tx_hash: B256::with_last_byte(3),
            event,
        }
    }

    /// The fixture tape folded as a `58b42b7` market from its genesis.
    fn legacy(tape: &[TapeEvent]) -> Replay {
        let mut market = Replay::from_genesis(Address::ZERO, Era::Legacy);
        for row in tape {
            market.apply(row);
        }
        market
    }

    fn block_of(row: &TapeEvent) -> BlockContext {
        BlockContext {
            number: row.block_number,
            hash: row.block_hash,
            timestamp: row.timestamp,
        }
    }

    fn price(units: u64) -> Price {
        Price::from_x96(Q96 * U256::from(units))
    }

    fn swap(pool_price: Price, insurance_fee: u128) -> SwapInfo {
        SwapInfo {
            perp_delta: PerpDelta::new(1_000_000),
            usd_delta: UsdcDelta::new(-40_000_000),
            pool_price,
            total_fee: UsdcDelta::new(100_000),
            lp_fee: UsdcAtoms::new(70_000),
            protocol_fee: UsdcAtoms::new(10_000),
            creator_fee: UsdcAtoms::new(10_000),
            insurance_fee: UsdcAtoms::new(insurance_fee),
        }
    }

    fn per_side(long: u128, short: u128) -> PerSide<PerpAtoms> {
        PerSide::new(PerpAtoms::new(long), PerpAtoms::new(short))
    }

    /// A market's first stretch: a touch, a print, capacity, a maker's
    /// mint, a swap, the books.
    fn tape() -> Vec<TapeEvent> {
        vec![
            row(
                10,
                0,
                MarketEvent::RatesAndEmasRefreshed {
                    funding_per_day: FundingRate::from_wad(1_000_000_000_000_000),
                    util_fee_per_day: PerSide::new(
                        UtilizationRate::from_wad(100),
                        UtilizationRate::from_wad(200),
                    ),
                    last_touch: 1_700_000_100,
                    pool_price_ema: price(40),
                    index_ema: price(41),
                },
            ),
            row(10, 1, MarketEvent::IndexUpdated { index: price(42) }),
            row(
                11,
                0,
                MarketEvent::CapacityUpdated {
                    capacity: per_side(5_000_000, 6_000_000),
                },
            ),
            row(
                11,
                1,
                MarketEvent::PositionTransferred {
                    from: Address::ZERO,
                    to: Address::repeat_byte(0x0A),
                    pos_id: U256::from(1),
                },
            ),
            row(
                12,
                0,
                MarketEvent::TakerOpened {
                    pos_id: U256::from(2),
                    swap: swap(price(43), 10_000),
                },
            ),
            row(
                12,
                1,
                MarketEvent::OpenInterestUpdated {
                    open_interest: per_side(1_000_000, 0),
                },
            ),
            row(
                12,
                2,
                MarketEvent::MarginTransferred {
                    margin_delta: UsdcDelta::new(100_000_000),
                    total_margin: UsdcAtoms::new(100_000_000),
                },
            ),
            row(
                13,
                0,
                MarketEvent::BadDebtAccounted {
                    bad_debt: UsdcAtoms::new(2_000_000),
                    insurance_after: UsdcAtoms::ZERO,
                    bad_debt_after: UsdcAtoms::new(2_000_000),
                },
            ),
            row(
                13,
                1,
                MarketEvent::MarginTransferred {
                    margin_delta: UsdcDelta::new(-10_000_000),
                    total_margin: UsdcAtoms::new(90_000_000),
                },
            ),
            row(
                14,
                0,
                MarketEvent::ModuleSet {
                    module: ModuleKind::Pricing,
                    address: Address::repeat_byte(0x4D),
                },
            ),
        ]
    }

    #[test]
    fn the_totals_are_the_last_stated_and_the_snapshots_carry_the_callers_block() {
        let tape = tape();
        let market = legacy(&tape);
        let last = block_of(tape.last().unwrap());

        assert_eq!(market.applied(), 10);
        assert_eq!(market.block(), Some(last));
        assert_eq!(market.pool_price(), Some(price(43)));
        assert_eq!(market.index(), Some(price(42)));
        assert_eq!(
            market.capacity_at(last),
            Some(MarketCapacity {
                block: last,
                capacity: per_side(5_000_000, 6_000_000),
                open_interest: per_side(1_000_000, 0),
            })
        );
        assert_eq!(
            market.solvency(),
            Some(SolvencyState {
                bad_debt: UsdcAtoms::new(2_000_000),
                total_margin: UsdcAtoms::new(90_000_000),
            })
        );
        assert_eq!(
            market.module(ModuleKind::Pricing),
            Some(Address::repeat_byte(0x4D))
        );
        assert_eq!(market.module(ModuleKind::Beacon), None);
        assert_eq!(
            market.custody().latest_owner(U256::from(1)),
            Some(Address::repeat_byte(0x0A))
        );
        assert_eq!(market.gaps(), Gaps::default(), "every total was restated");
    }

    #[test]
    fn the_mark_is_the_emas_advanced_to_the_callers_block() {
        let tape = tape();
        let market = legacy(&tape);
        let at = BlockContext {
            number: 20,
            hash: B256::with_last_byte(20),
            timestamp: 1_700_003_700,
        };
        let ema_window = 3_600;
        let mark = market.mark_at(at, ema_window).unwrap().unwrap();
        assert_eq!(mark.block, at);
        assert_eq!(mark.pool_price, price(43));
        assert_eq!(mark.index, price(42));
        let stored = market.emas().unwrap();
        let expected = calculate_emas(
            stored.pair().unwrap(),
            crate::math::pricing::PricePair::try_from_x96(price(43).x96(), price(42).x96())
                .unwrap(),
            stored.last_touch,
            at.timestamp,
            ema_window,
        )
        .unwrap();
        assert_eq!(mark.emas, expected);

        assert_eq!(
            Replay::fold(&tape[..1]).mark_at(at, ema_window).unwrap(),
            None,
            "no mark before a swap and a print"
        );
    }

    /// A donation or a booking moves the margin total silently; a swap's
    /// insurance fee repays debt silently. Each counts until the next
    /// statement, and a statement clears it.
    #[test]
    fn silences_are_counted_until_the_next_statement() {
        let mut tape = tape();
        tape.push(row(
            15,
            0,
            MarketEvent::Donated {
                donor: Address::repeat_byte(0xD0),
                amount: UsdcAtoms::new(1_000_000),
                bad_debt: UsdcAtoms::new(1_000_000),
                insurance: UsdcAtoms::ZERO,
            },
        ));
        tape.push(row(
            16,
            0,
            MarketEvent::TakerOpened {
                pos_id: U256::from(3),
                swap: swap(price(44), 10_000),
            },
        ));
        let market = legacy(&tape);
        assert_eq!(
            market.gaps(),
            Gaps {
                total_margin_unemitted: 1,
                bad_debt_unemitted: 1,
            }
        );
        assert_eq!(
            market.solvency().unwrap().bad_debt,
            UsdcAtoms::new(1_000_000)
        );

        tape.push(row(
            17,
            0,
            MarketEvent::LossSocialized {
                original_amount: UsdcAtoms::new(500_000),
                fee_charged: UsdcAtoms::new(5_000),
                bad_debt_after: UsdcAtoms::new(995_000),
            },
        ));
        tape.push(row(
            17,
            1,
            MarketEvent::MarginTransferred {
                margin_delta: UsdcDelta::new(-495_000),
                total_margin: UsdcAtoms::new(90_505_000),
            },
        ));
        let market = legacy(&tape);
        assert_eq!(market.gaps(), Gaps::default());

        // A swap while no debt stands repays nothing, so it is no silence.
        let mut clear = tape.clone();
        clear.push(row(
            18,
            0,
            MarketEvent::LossSocialized {
                original_amount: UsdcAtoms::new(995_000),
                fee_charged: UsdcAtoms::new(995_000),
                bad_debt_after: UsdcAtoms::ZERO,
            },
        ));
        clear.push(row(
            19,
            0,
            MarketEvent::TakerOpened {
                pos_id: U256::from(4),
                swap: swap(price(45), 10_000),
            },
        ));
        assert_eq!(legacy(&clear).gaps().bad_debt_unemitted, 0);
    }

    /// The fold of the whole tape equals the combination of the folds of
    /// its two halves, at every cut, gaps included.
    #[test]
    fn the_fold_of_a_concatenation_is_the_combination_of_the_folds() {
        let mut tape = tape();
        tape.push(row(
            15,
            0,
            MarketEvent::Donated {
                donor: Address::repeat_byte(0xD0),
                amount: UsdcAtoms::new(1_000_000),
                bad_debt: UsdcAtoms::new(1_000_000),
                insurance: UsdcAtoms::ZERO,
            },
        ));
        tape.push(row(
            16,
            0,
            MarketEvent::TakerOpened {
                pos_id: U256::from(3),
                swap: swap(price(44), 10_000),
            },
        ));
        tape.push(row(
            16,
            1,
            MarketEvent::PositionTransferred {
                from: Address::repeat_byte(0x0A),
                to: Address::repeat_byte(0x0B),
                pos_id: U256::from(1),
            },
        ));
        let whole = Replay::fold(&tape);
        for cut in 0..=tape.len() {
            let mut left = Replay::fold(&tape[..cut]);
            left.combine(Replay::fold(&tape[cut..]));
            assert_eq!(left, whole, "cut at {cut}");
        }
    }

    /// From genesis the books, capacity and open interest are zero and
    /// stated, so a market that never booked debt reads as debt-free rather
    /// than unknown; what the creation log alone carries stays unknown. A
    /// genesis fold continued by a segment fold is the genesis fold of the
    /// whole.
    #[test]
    fn genesis_states_the_zeros_and_leaves_the_creation_log_unknown() {
        let perp = Address::repeat_byte(0xF0);
        let empty = Replay::from_genesis(perp, Era::Legacy);
        assert_eq!(empty.perp(), perp);
        assert_eq!(
            empty.solvency(),
            Some(SolvencyState {
                bad_debt: UsdcAtoms::ZERO,
                total_margin: UsdcAtoms::ZERO,
            })
        );
        let genesis_block = BlockContext::default();
        assert_eq!(
            empty.capacity_at(genesis_block),
            Some(MarketCapacity {
                block: genesis_block,
                capacity: PerSide::default(),
                open_interest: PerSide::default(),
            })
        );
        assert_eq!(empty.pool_price(), None);
        assert_eq!(empty.emas(), None);
        assert_eq!(empty.module(ModuleKind::Pricing), None);
        assert_eq!(empty.gaps(), Gaps::default());

        // A tape that never books debt: the books are still known.
        let tape = tape();
        let debt_free: Vec<TapeEvent> = tape
            .iter()
            .filter(|row| !matches!(row.event, MarketEvent::BadDebtAccounted { .. }))
            .copied()
            .collect();
        let mut from_genesis = Replay::from_genesis(perp, Era::Legacy);
        for row in &debt_free {
            from_genesis.apply(row);
        }
        assert_eq!(
            from_genesis.solvency(),
            Some(SolvencyState {
                bad_debt: UsdcAtoms::ZERO,
                total_margin: UsdcAtoms::new(90_000_000),
            })
        );
        assert_eq!(
            Replay::fold(&debt_free).solvency(),
            None,
            "a segment fold does not know the debt was never booked"
        );

        // Genesis then a segment is genesis over the whole.
        for cut in 0..=tape.len() {
            let mut left = Replay::from_genesis(perp, Era::Legacy);
            for row in &tape[..cut] {
                left.apply(row);
            }
            left.combine(Replay::fold(&tape[cut..]));
            let mut whole = Replay::from_genesis(perp, Era::Legacy);
            for row in &tape {
                whole.apply(row);
            }
            assert_eq!(left, whole, "cut at {cut}");
        }
    }

    fn with_tx(mut row: TapeEvent, tx: u8) -> TapeEvent {
        row.tx_hash = B256::with_last_byte(tx);
        row
    }

    fn margin_transferred(delta: i128, total: u128) -> MarketEvent {
        MarketEvent::MarginTransferred {
            margin_delta: UsdcDelta::new(delta),
            total_margin: UsdcAtoms::new(total),
        }
    }

    /// A swap's protocol, creator and insurance fees (30,000 here) leave
    /// the margin total with no event, and when they leave relative to the
    /// transaction's `MarginTransferred` is the build's and the path's.
    #[test]
    fn swap_fees_leave_the_margin_total_as_each_build_orders_them() {
        let open = |tx| {
            vec![
                with_tx(
                    row(10, 0, margin_transferred(1_000_000_000, 1_000_000_000)),
                    tx,
                ),
                with_tx(
                    row(
                        10,
                        1,
                        MarketEvent::TakerOpened {
                            pos_id: U256::from(1),
                            swap: swap(price(40), 10_000),
                        },
                    ),
                    tx,
                ),
            ]
        };
        // Both builds transfer the deposit first, then remove the fees.
        for era in [Era::Legacy, Era::Upgradeable] {
            let mut market = Replay::from_genesis(Address::ZERO, era);
            for r in &open(1) {
                market.apply(r);
            }
            assert_eq!(
                market.solvency().unwrap().total_margin,
                UsdcAtoms::new(999_970_000),
                "{era:?} open"
            );
            assert_eq!(market.gaps(), Gaps::default());
        }

        let adjust_with = |delta: i128, tx| {
            vec![
                with_tx(row(20, 0, margin_transferred(delta, 500_000_000)), tx),
                with_tx(
                    row(
                        20,
                        1,
                        MarketEvent::TakerAdjusted {
                            pos_id: U256::from(1),
                            swap: swap(price(41), 10_000),
                            funding: UsdcDelta::new(0),
                            util_fees: UsdcAtoms::ZERO,
                        },
                    ),
                    tx,
                ),
            ]
        };
        // `58b42b7` removes the fees before every transfer: the stated total
        // already has them out, whatever the delta's sign.
        for delta in [50_000_000, -50_000_000, 0] {
            let mut market = Replay::from_genesis(Address::ZERO, Era::Legacy);
            for r in open(1).iter().chain(&adjust_with(delta, 2)) {
                market.apply(r);
            }
            assert_eq!(
                market.solvency().unwrap().total_margin,
                UsdcAtoms::new(500_000_000),
                "legacy adjust with delta {delta}"
            );
        }
        // `v0.2.2` transfers a deposit before the removal, a withdrawal
        // after it.
        let mut deposit = Replay::from_genesis(Address::ZERO, Era::Upgradeable);
        for r in open(1).iter().chain(&adjust_with(50_000_000, 2)) {
            deposit.apply(r);
        }
        assert_eq!(
            deposit.solvency().unwrap().total_margin,
            UsdcAtoms::new(499_970_000)
        );
        for delta in [-50_000_000, 0] {
            let mut withdrawal = Replay::from_genesis(Address::ZERO, Era::Upgradeable);
            for r in open(1).iter().chain(&adjust_with(delta, 2)) {
                withdrawal.apply(r);
            }
            assert_eq!(
                withdrawal.solvency().unwrap().total_margin,
                UsdcAtoms::new(500_000_000),
                "upgradeable adjust with delta {delta}"
            );
        }

        // A `v0.2.2` liquidation closes before it transfers the fee, so the
        // fees leave after no statement and the transfer that follows
        // restates the total.
        let liquidation = vec![
            with_tx(
                row(
                    30,
                    0,
                    MarketEvent::TakerClosed {
                        pos_id: U256::from(1),
                        swap: swap(price(42), 10_000),
                        funding: UsdcDelta::new(0),
                        util_fees: UsdcAtoms::ZERO,
                        liquidation_fee: UsdcAtoms::ZERO,
                        is_liquidation: false,
                    },
                ),
                3,
            ),
            with_tx(
                row(
                    30,
                    1,
                    MarketEvent::TakerLiquidated {
                        pos_id: U256::from(1),
                        perp_amount: PerpAtoms::new(1_000_000),
                        liquidation_fee: UsdcAtoms::new(2_000_000),
                    },
                ),
                3,
            ),
        ];
        let mut liquidated = Replay::from_genesis(Address::ZERO, Era::Upgradeable);
        for r in open(1).iter().chain(&liquidation) {
            liquidated.apply(r);
        }
        assert_eq!(
            liquidated.solvency().unwrap().total_margin,
            UsdcAtoms::new(999_940_000),
            "the close's fees left after the open's statement"
        );
        liquidated.apply(&with_tx(
            row(30, 2, margin_transferred(-2_000_000, 997_940_000)),
            3,
        ));
        assert_eq!(
            liquidated.solvency().unwrap().total_margin,
            UsdcAtoms::new(997_940_000)
        );

        // A fold not told the build removes nothing and counts the silence;
        // a segment told it removes as the build does.
        let blind = Replay::fold(&open(1));
        assert_eq!(blind.gaps().total_margin_unemitted, 1);
        let mut told = Replay::segment(Era::Legacy);
        for r in &open(1) {
            told.apply(r);
        }
        assert_eq!(told.gaps().total_margin_unemitted, 0);
        assert_eq!(
            told.solvency(),
            None,
            "a segment never told the debt does not invent it"
        );
    }
}
