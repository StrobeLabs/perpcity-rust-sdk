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
//! many times a figure may have moved without an event saying so, and how
//! many positions stand on a figure no event stated, and a consumer gates
//! on it.

mod market;
mod pool;
mod positions;
mod seed;
mod solvency;

use std::collections::BTreeMap;
use std::result::Result as StdResult;

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;

use crate::client::{OpenInterest, SolvencyState, StateAt};
use crate::errors::{Result, ValidationError};
use crate::events::{CumulativesInfo, ModuleKind};
use crate::math::BlockContext;
use crate::math::capacity::MarketCapacity;
use crate::math::pricing::{Emas, Mark};
use crate::math::swap::TickLiquidity;
use crate::units::{FundingRate, LUnits, PerSide, Price, UtilizationRate};

use self::market::{Modules, Prices, Rates, Utilization};
use self::pool::Pool;
pub use self::positions::{PositionKind, PositionState, Positions};
use self::seed::Seed;
use self::solvency::Solvency;
use super::History;
use super::fold::{Fold, Latest};
use super::tape::{ChainPoint, OwnershipLog, TapeAddresses, TapeEvent};

/// How many times a figure may have moved without an event saying so,
/// since the last event that stated it. A reading with a nonzero gap is
/// forensic, not a decision's input.
///
/// All of these count silences in the live contract builds: the next build
/// emits what repaid the debt on every swap, the margin total on every path
/// that moves it, and a position's margin, size and band on every event
/// that changes them, and these go with it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gaps {
    /// Moves of `totalMargin` no event carries, since the last
    /// `MarginTransferred`: a donation adds the debt it repaid, a bad-debt
    /// booking adds the insurance it consumed, and a swap while debt stands
    /// removes less than its gross fees by what repaid the debt.
    pub total_margin_unemitted: u32,
    /// Swaps that paid an insurance fee while bad debt stood, since the
    /// last event that stated the debt. The fee repays debt first, and no
    /// event on the live builds carries the amount.
    pub bad_debt_unemitted: u32,
    /// Open positions whose level the fold does not know: the open unseen
    /// and no read to supply it, so their size or band is what moved since,
    /// not where they stand. Includes `taker_size_unknown`.
    pub partial_positions: u32,
    /// Open takers whose size no event stated: first seen mid-life, or
    /// converted from a maker, whose inventory the live builds never emit.
    pub taker_size_unknown: u32,
    /// Open positions whose margin the fold does not know: no live event
    /// carries margin, so this is every position a read did not supply and
    /// every one an event has touched since the read.
    pub margin_unknown: u32,
    /// Events refused for arriving at or before the fold's point: a driver
    /// out of order, a duplicate, or a seed's own block delivered again.
    /// Nothing refused is applied; a consumer that sees the count rise heals
    /// from the tape with [`Replay::catch_up`].
    pub refused: u32,
}

/// A market rebuilt from its tape.
///
/// `apply` one [`TapeEvent`] at a time, from [`market_tape`](super::market_tape)
/// or from the stamped feed; the accessors answer at the last event applied.
/// Accessors whose read type carries a block take the caller's
/// [`BlockContext`], so a rebuilt snapshot compares equal to a pinned read
/// at that block.
///
/// The market is a composition of folds, one per concern, each a `Fold` in
/// its own right: the mark's inputs, the touch's rates, capacity and open
/// interest, the solvency books, the modules, the pool's liquidity, the
/// positions, and custody. `apply` hands every event to each; `combine`
/// merges each with its counterpart. The one rule of the live build's, how
/// a swap's fees leave the margin total, lives in the solvency fold alone.
///
/// Three starts: [`from_genesis`](Self::from_genesis) before the market's
/// first event, [`seeded`](Self::seeded) from the reads at a block, and the
/// trait's `fold` for a segment. From any of them, [`catch_up`](Self::catch_up)
/// applies the tape from the block after the fold's to the lagged head.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Replay {
    perp: Address,
    events: u64,
    last: Latest<(ChainPoint, BlockContext)>,
    refused: u32,
    prices: Prices,
    rates: Rates,
    utilization: Utilization,
    solvency: Solvency,
    modules: Modules,
    pool: Pool,
    positions: Positions,
    custody: OwnershipLog,
}

impl Replay {
    /// The market before its first event. What is zero before any event is
    /// zero and stated: no capacity, no open interest, no margin, no debt,
    /// no liquidity at any tick. What only the factory's creation log
    /// carries — the modules, the first price and tick, the first EMAs — is
    /// unknown until the market's own events state it.
    ///
    /// A fold over a segment that is not the market's start is
    /// [`Fold::fold`], whose totals are unknown until the segment states
    /// them and whose tick map is never whole.
    pub fn from_genesis(perp: Address) -> Self {
        Self {
            perp,
            utilization: Utilization::genesis(),
            solvency: Solvency::genesis(),
            pool: Pool::genesis(),
            ..Self::default()
        }
    }

    /// The market as the reads return it at `state`'s block, so a live
    /// cache boots without a scan: every total stated, the tick map whole,
    /// every position's level and margin known. What only events carry — a
    /// position's open, a band's deposit price, custody — starts unknown
    /// and fills in from the events that follow.
    ///
    /// The fold stands at the end of the block, so the first event it takes
    /// is the next block's; a feed that started earlier delivers the block's
    /// own events again and they are refused, counted on
    /// [`Gaps::refused`].
    ///
    /// # Errors
    ///
    /// Any read's error. A position row that fails to read fails the seed:
    /// a seed with a hole is not a seed.
    pub async fn seeded(state: &StateAt) -> Result<Self> {
        Ok(Self::from_seed(Seed::read(state).await?)?)
    }

    fn from_seed(seed: Seed) -> StdResult<Self, ValidationError> {
        let end_of_block = ChainPoint {
            block: seed.block.number,
            log_index: u64::MAX,
        };
        Ok(Self {
            perp: seed.perp,
            events: 0,
            last: Latest::stated((end_of_block, seed.block)),
            refused: 0,
            prices: Prices::seeded(seed.pool_price, seed.index, seed.emas),
            rates: Rates::seeded(seed.rates, seed.cumulatives),
            utilization: Utilization::seeded(seed.capacity),
            solvency: Solvency::seeded(seed.solvency),
            modules: Modules::seeded(seed.modules),
            pool: Pool::seeded(seed.ticks, seed.tick)?,
            positions: Positions::seeded(seed.pool_price, seed.positions),
            custody: OwnershipLog::default(),
        })
    }

    /// Apply the market's tape from the block after this fold's to
    /// `to_block`, or to the handle's lagged head: the driver for a fold
    /// that follows a market by polling, and the way a fold heals after a
    /// feed dropped or refused events. Returns how many events it applied.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidConfig`] for a fold with no block to
    /// continue from: fold a tape from the market's first block, or seed
    /// from a read. Otherwise the scan's errors.
    pub async fn catch_up<P: Provider>(
        &mut self,
        history: &History<P>,
        addresses: TapeAddresses,
        to_block: Option<u64>,
    ) -> Result<usize> {
        let from = self
            .block()
            .ok_or_else(|| ValidationError::InvalidConfig {
                reason: "a fold with no block cannot catch up: fold a tape from the market's \
                         first block, or seed from a read"
                    .into(),
            })?
            .number
            + 1;
        let to = match to_block {
            Some(to) => to,
            None => history.tip().await?,
        };
        if to < from {
            return Ok(0);
        }
        let tape = history.market_tape(addresses, from, Some(to)).await?;
        for row in &tape {
            self.apply(row);
        }
        Ok(tape.len())
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
        self.last.get().map(|(point, _)| point)
    }

    /// The block of the last event applied.
    pub fn block(&self) -> Option<BlockContext> {
        self.last.get().map(|(_, block)| block)
    }

    /// The pool price after the last swap.
    pub fn pool_price(&self) -> Option<Price> {
        self.prices.pool()
    }

    /// The beacon's last print.
    pub fn index(&self) -> Option<Price> {
        self.prices.index()
    }

    /// The stored EMA pair and the touch it is current as of, as the last
    /// `RatesAndEmasRefreshed` left them.
    pub fn emas(&self) -> Option<Emas> {
        self.prices.emas()
    }

    /// The funding rate the last touch set.
    pub fn funding_per_day(&self) -> Option<FundingRate> {
        self.rates.funding_per_day()
    }

    /// The utilization fee rates the last touch set, per side.
    pub fn util_fee_per_day(&self) -> Option<PerSide<UtilizationRate>> {
        self.rates.util_fee_per_day()
    }

    /// The market's accumulators at the last accrual.
    pub fn cumulatives(&self) -> Option<CumulativesInfo> {
        self.rates.cumulatives()
    }

    /// Taker open interest, per side.
    pub fn open_interest(&self) -> Option<OpenInterest> {
        self.utilization.open_interest()
    }

    /// Capacity and its draw at `block`, as [`StateAt::capacity`](crate::StateAt::capacity)
    /// reads them; `None` until both have been stated.
    pub fn capacity_at(&self, block: BlockContext) -> Option<MarketCapacity> {
        self.utilization.at(block)
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
    ) -> StdResult<Option<Mark>, ValidationError> {
        self.prices.mark_at(block, ema_window)
    }

    /// The solvency books: the debt as last stated, the margin total as
    /// last stated less the swap fees the build removed since. `None` until
    /// both have been stated. Read [`Self::gaps`] beside it.
    pub fn solvency(&self) -> Option<SolvencyState> {
        self.solvency.state()
    }

    /// The module of `kind` in force, once a `ModuleSet` has named one.
    pub fn module(&self, kind: ModuleKind) -> Option<Address> {
        self.modules.get(kind)
    }

    /// Who held each position when: the custody fold, taken in the same
    /// pass.
    pub fn custody(&self) -> &OwnershipLog {
        &self.custody
    }

    /// Every position the tape mentioned: takers sized by their swaps,
    /// makers by their liquidity changes.
    pub fn positions(&self) -> &Positions {
        &self.positions
    }

    /// One position, if the tape mentioned it.
    pub fn position(&self, pos_id: U256) -> Option<&PositionState> {
        self.positions.get(pos_id)
    }

    /// The pool's tick, as [`StateAt::pool`](crate::StateAt::pool) reads
    /// it: where the last swap that moved it left it. `None` until a swap
    /// has moved it, since the pool's first tick is the factory's to say.
    pub fn pool_tick(&self) -> Option<i32> {
        self.pool.tick()
    }

    /// Liquidity at every initialized tick, as `PoolSnapshot::ticks` reads
    /// it. `None` for a fold that did not start at genesis: a segment knows
    /// what changed, not what stands.
    pub fn pool_ticks(&self) -> Option<BTreeMap<i32, TickLiquidity>> {
        self.pool.ticks()
    }

    /// Liquidity active at the pool's tick, as `PoolSnapshot::liquidity`
    /// reads it: the net of every initialized tick at or below it. `None`
    /// until the tick is known, or for a fold that did not start at genesis.
    pub fn pool_liquidity(&self) -> Option<LUnits> {
        self.pool.liquidity()
    }

    /// What may have moved without an event saying so, and what stands on
    /// a figure no event stated. Both are decided at the read, from the
    /// latest totals and the positions, so the counts combine like the rest.
    pub fn gaps(&self) -> Gaps {
        let silences = self.solvency.silences();
        let mut gaps = Gaps {
            total_margin_unemitted: silences.total_margin_unemitted,
            bad_debt_unemitted: silences.bad_debt_unemitted,
            refused: self.refused,
            ..Gaps::default()
        };
        for (_, position) in self.positions.open() {
            gaps.margin_unknown += u32::from(position.margin().is_none());
            gaps.partial_positions += u32::from(!position.level_known());
            if let PositionKind::Taker { sized: false, .. } = position.kind() {
                gaps.taker_size_unknown += 1;
            }
        }
        gaps
    }
}

impl Fold for Replay {
    /// An event at or before the fold's point is refused, never applied:
    /// chain order is the one assumption every sum here rests on, and a
    /// driver that breaks it is counted rather than trusted.
    fn apply(&mut self, event: &TapeEvent) {
        let point = event.point();
        if self.last.get().is_some_and(|(last, _)| point <= last) {
            self.refused += 1;
            return;
        }
        self.last.set((
            point,
            BlockContext {
                number: event.block_number,
                hash: event.block_hash,
                timestamp: event.timestamp,
            },
        ));
        self.events += 1;

        self.prices.apply(event);
        self.rates.apply(event);
        self.utilization.apply(event);
        self.solvency.apply(event);
        self.modules.apply(event);
        self.pool.apply(event);
        self.positions.apply(event);
        self.custody.apply(event);
    }

    fn combine(&mut self, later: Self) {
        debug_assert!(
            self.last
                .get()
                .zip(later.last.get())
                .is_none_or(|((a, _), (b, _))| a < b),
            "segments combined out of order"
        );
        if self.perp.is_zero() {
            self.perp = later.perp;
        }
        self.events += later.events;
        self.refused += later.refused;
        self.last.combine(later.last);

        self.prices.combine(later.prices);
        self.rates.combine(later.rates);
        self.utilization.combine(later.utilization);
        self.solvency.combine(later.solvency);
        self.modules.combine(later.modules);
        self.pool.combine(later.pool);
        self.positions.combine(later.positions);
        self.custody.combine(later.custody);
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{B256, U256};

    use alloy::primitives::I256;

    use super::seed::{Seed, SeedPosition};
    use super::*;
    use crate::client::MarketRates;
    use crate::constants::Q96;
    use crate::contracts::Modules as ContractModules;
    use crate::events::{MakerSettle, MarketEvent, SwapInfo};
    use crate::math::pricing::calculate_emas;
    use crate::math::range::{MakerBand, TickRange};
    use crate::units::{
        Earnings, Funding, FundingPerSqrtPrice, LDelta, PerpAtoms, PerpDelta, UsdcAtoms, UsdcDelta,
    };

    /// A row in `block`; the fixtures put one transaction in each block, so
    /// the transaction is the block's.
    fn row(block: u64, log_index: u64, event: MarketEvent) -> TapeEvent {
        TapeEvent {
            block_number: block,
            block_hash: B256::with_last_byte(block as u8),
            log_index,
            timestamp: 1_700_000_000 + block * 10,
            tx_hash: B256::repeat_byte(block as u8),
            event,
        }
    }

    /// The fixture tape folded as a market from its genesis.
    fn genesis(tape: &[TapeEvent]) -> Replay {
        let mut market = Replay::from_genesis(Address::ZERO);
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
        let market = genesis(&tape);
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
        assert_eq!(
            market.gaps(),
            Gaps {
                margin_unknown: 1,
                ..Gaps::default()
            },
            "every total was restated; one taker stands, with the margin no event carries"
        );
    }

    #[test]
    fn the_mark_is_the_emas_advanced_to_the_callers_block() {
        let tape = tape();
        let market = genesis(&tape);
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
    /// insurance fee while debt stands repays debt silently and removes
    /// less than its gross fees. Each counts until the next statement, and
    /// a statement clears it.
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
        let market = genesis(&tape);
        assert_eq!(
            market.gaps(),
            Gaps {
                total_margin_unemitted: 2,
                bad_debt_unemitted: 1,
                margin_unknown: 2,
                ..Gaps::default()
            },
            "the donation, and the swap's fees net of what repaid the debt; two takers stand"
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
        let market = genesis(&tape);
        assert_eq!(
            market.gaps(),
            Gaps {
                margin_unknown: 2,
                ..Gaps::default()
            }
        );

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
        assert_eq!(genesis(&clear).gaps().bad_debt_unemitted, 0);
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
        let empty = Replay::from_genesis(perp);
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
        let mut from_genesis = Replay::from_genesis(perp);
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
            let mut left = Replay::from_genesis(perp);
            for row in &tape[..cut] {
                left.apply(row);
            }
            left.combine(Replay::fold(&tape[cut..]));
            let mut whole = Replay::from_genesis(perp);
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
    /// transaction's `MarginTransferred` is the path's.
    #[test]
    fn swap_fees_leave_the_margin_total_as_the_build_orders_them() {
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
        // An open transfers the deposit first, then removes the fees.
        let opened = genesis(&open(1));
        assert_eq!(
            opened.solvency().unwrap().total_margin,
            UsdcAtoms::new(999_970_000)
        );
        let one_taker = Gaps {
            margin_unknown: 1,
            ..Gaps::default()
        };
        assert_eq!(opened.gaps(), one_taker);

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
        // An adjust transfers a deposit before the removal, a withdrawal
        // after it.
        let with_deposit: Vec<TapeEvent> = open(1)
            .into_iter()
            .chain(adjust_with(50_000_000, 2))
            .collect();
        assert_eq!(
            genesis(&with_deposit).solvency().unwrap().total_margin,
            UsdcAtoms::new(499_970_000)
        );
        for delta in [-50_000_000, 0] {
            let with_withdrawal: Vec<TapeEvent> =
                open(1).into_iter().chain(adjust_with(delta, 2)).collect();
            assert_eq!(
                genesis(&with_withdrawal).solvency().unwrap().total_margin,
                UsdcAtoms::new(500_000_000),
                "adjust with delta {delta}"
            );
        }

        // A liquidation closes before it transfers the fee, so the fees
        // leave after no statement and the transfer that follows restates
        // the total.
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
        let mut liquidated = genesis(&open(1));
        for r in &liquidation {
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

        // A segment fold removes the same fees from the total it was told,
        // and invents no debt it was never told.
        let segment = Replay::fold(&open(1));
        assert_eq!(segment.gaps(), one_taker);
        assert_eq!(
            segment.solvency(),
            None,
            "a segment never told the debt does not invent it"
        );
    }

    fn settle() -> MakerSettle {
        MakerSettle {
            funding: UsdcDelta::new(1_000),
            util_fees: PerSide::new(UsdcAtoms::new(10), UsdcAtoms::new(20)),
            lp_fees: UsdcAtoms::new(30),
        }
    }

    fn sized_swap(perp_delta: i128, pool_price: Price) -> SwapInfo {
        SwapInfo {
            perp_delta: PerpDelta::new(perp_delta),
            ..swap(pool_price, 0)
        }
    }

    fn modify(pos: u64, lower: i32, upper: i32, delta: i128) -> MarketEvent {
        MarketEvent::ModifyLiquidity {
            pool_id: B256::with_last_byte(7),
            sender: Address::ZERO,
            tick_lower: lower,
            tick_upper: upper,
            liquidity_delta: LDelta::new(delta),
            salt: B256::from(U256::from(pos)),
        }
    }

    fn tick(gross: u128, net: i128) -> TickLiquidity {
        TickLiquidity {
            gross: LUnits::new(gross),
            net: LDelta::new(net),
        }
    }

    /// Two positions' lives: taker 1 opens, adds, is liquidated in part and
    /// then whole; maker 2 deposits a band, trims it, and is converted when
    /// the rest is pulled, then closes as the taker it became.
    fn lifecycle() -> Vec<TapeEvent> {
        vec![
            row(
                20,
                0,
                MarketEvent::TakerOpened {
                    pos_id: U256::from(1),
                    swap: sized_swap(1_000_000, price(43)),
                },
            ),
            row(21, 0, modify(2, -600, 600, 1_000)),
            row(
                21,
                1,
                MarketEvent::MakerOpened {
                    pos_id: U256::from(2),
                },
            ),
            row(
                22,
                0,
                MarketEvent::TicksCrossed {
                    starting_tick: 0,
                    ending_tick: 10,
                    zero_for_one: false,
                },
            ),
            row(
                22,
                1,
                MarketEvent::TakerAdjusted {
                    pos_id: U256::from(1),
                    swap: sized_swap(500_000, price(44)),
                    funding: UsdcDelta::ZERO,
                    util_fees: UsdcAtoms::ZERO,
                },
            ),
            row(23, 0, modify(2, -600, 600, -400)),
            row(
                23,
                1,
                MarketEvent::MakerAdjusted {
                    pos_id: U256::from(2),
                    settle: settle(),
                },
            ),
            row(
                24,
                0,
                MarketEvent::TakerAdjusted {
                    pos_id: U256::from(1),
                    swap: sized_swap(-300_000, price(44)),
                    funding: UsdcDelta::ZERO,
                    util_fees: UsdcAtoms::ZERO,
                },
            ),
            row(
                24,
                1,
                MarketEvent::TakerLiquidated {
                    pos_id: U256::from(1),
                    perp_amount: PerpAtoms::new(300_000),
                    liquidation_fee: UsdcAtoms::new(1_000),
                },
            ),
            row(25, 0, modify(2, -600, 600, -600)),
            row(
                25,
                1,
                MarketEvent::MakerConverted {
                    pos_id: U256::from(2),
                    settle: settle(),
                    liquidation_fee: UsdcAtoms::ZERO,
                    is_liquidation: false,
                },
            ),
            row(
                26,
                0,
                MarketEvent::TakerClosed {
                    pos_id: U256::from(2),
                    swap: sized_swap(-700, price(45)),
                    funding: UsdcDelta::ZERO,
                    util_fees: UsdcAtoms::ZERO,
                    liquidation_fee: UsdcAtoms::ZERO,
                    is_liquidation: false,
                },
            ),
            row(
                26,
                1,
                MarketEvent::TakerClosed {
                    pos_id: U256::from(1),
                    swap: sized_swap(-1_200_000, price(45)),
                    funding: UsdcDelta::ZERO,
                    util_fees: UsdcAtoms::ZERO,
                    liquidation_fee: UsdcAtoms::ZERO,
                    is_liquidation: false,
                },
            ),
            row(
                26,
                2,
                MarketEvent::TakerLiquidated {
                    pos_id: U256::from(1),
                    perp_amount: PerpAtoms::new(1_200_000),
                    liquidation_fee: UsdcAtoms::new(4_000),
                },
            ),
        ]
    }

    /// A maker's band is the range its first liquidity change named and
    /// the sum of the changes since; the pool's tick map is the same changes
    /// summed per tick, and its liquidity the net at or below the tick.
    #[test]
    fn a_makers_band_and_the_pools_book_are_the_sum_of_liquidity_changes() {
        let tape = lifecycle();
        let band = |lower, upper, liquidity| {
            MakerBand::new(
                TickRange::new(lower, upper).unwrap(),
                LUnits::new(liquidity),
            )
        };

        // Deposited: the band stands, at the price of the swap before it.
        let deposited = genesis(&tape[..3]);
        let maker = deposited.position(U256::from(2)).unwrap();
        assert_eq!(maker.maker_band(), Some(band(-600, 600, 1_000)));
        assert_eq!(maker.deposit_pool_price(), Some(price(43)));
        assert_eq!(maker.opened(), Some(tape[2].point()));
        assert_eq!(
            deposited.pool_ticks(),
            Some(BTreeMap::from([
                (-600, tick(1_000, 1_000)),
                (600, tick(1_000, -1_000))
            ]))
        );
        assert_eq!(
            deposited.pool_tick(),
            None,
            "no swap has moved the tick, and the first tick is the factory's"
        );
        assert_eq!(deposited.pool_liquidity(), None);

        // A swap moves the tick into the band; the band's liquidity is active.
        let crossed = genesis(&tape[..5]);
        assert_eq!(crossed.pool_tick(), Some(10));
        assert_eq!(crossed.pool_liquidity(), Some(LUnits::new(1_000)));

        // Trimmed.
        let trimmed = genesis(&tape[..7]);
        assert_eq!(
            trimmed.position(U256::from(2)).unwrap().maker_band(),
            Some(band(-600, 600, 600))
        );
        assert_eq!(trimmed.pool_liquidity(), Some(LUnits::new(600)));

        // Pulled and converted: the band is gone, the tick map is empty, and
        // the position is a taker of a size no event carries.
        let converted = genesis(&tape[..11]);
        let taker = converted.position(U256::from(2)).unwrap();
        assert_eq!(taker.maker_band(), None);
        assert_eq!(taker.taker_size(), None);
        assert!(matches!(
            taker.kind(),
            PositionKind::Taker { sized: false, .. }
        ));
        assert!(taker.is_open());
        assert_eq!(converted.pool_ticks(), Some(BTreeMap::new()));
        assert_eq!(converted.pool_liquidity(), Some(LUnits::ZERO));
        assert_eq!(converted.gaps().taker_size_unknown, 1);

        // Closed as a taker.
        let closed = genesis(&tape[..12]);
        let taker = closed.position(U256::from(2)).unwrap();
        assert_eq!(taker.closed(), Some(tape[11].point()));
        assert_eq!(closed.gaps().taker_size_unknown, 0);
    }

    /// A taker's size is the sum of its swaps' perp deltas; each dedicated
    /// liquidation event, and each tailed close, counts one liquidation.
    #[test]
    fn a_taker_is_sized_by_its_swaps_and_its_liquidations_are_counted() {
        let tape = lifecycle();
        let one = U256::from(1);

        let opened = genesis(&tape[..1]);
        let taker = opened.position(one).unwrap();
        assert_eq!(taker.taker_size(), Some(PerpDelta::new(1_000_000)));
        assert_eq!(taker.opened(), Some(tape[0].point()));
        assert_eq!(taker.liquidations(), 0);

        let added = genesis(&tape[..5]);
        assert_eq!(
            added.position(one).unwrap().taker_size(),
            Some(PerpDelta::new(1_500_000))
        );

        let partly = genesis(&tape[..9]);
        let taker = partly.position(one).unwrap();
        assert_eq!(taker.taker_size(), Some(PerpDelta::new(1_200_000)));
        assert_eq!(taker.liquidations(), 1);
        assert!(taker.is_open());

        let whole = genesis(&tape);
        let taker = whole.position(one).unwrap();
        assert_eq!(taker.taker_size(), Some(PerpDelta::ZERO));
        assert_eq!(taker.liquidations(), 2);
        assert_eq!(taker.closed(), Some(tape[12].point()));
        assert_eq!(whole.positions().open().count(), 0);
        assert_eq!(whole.gaps().margin_unknown, 0);

        // The retired build's shape says it in the close's tail instead.
        let tailed = genesis(&[
            tape[0],
            row(
                21,
                0,
                MarketEvent::TakerClosed {
                    pos_id: one,
                    swap: sized_swap(-1_000_000, price(44)),
                    funding: UsdcDelta::ZERO,
                    util_fees: UsdcAtoms::ZERO,
                    liquidation_fee: UsdcAtoms::new(500),
                    is_liquidation: true,
                },
            ),
        ]);
        assert_eq!(tailed.position(one).unwrap().liquidations(), 1);
    }

    /// A segment that starts mid-life knows what moved and not where a
    /// position stands, and says so: no size, no band, no tick map, and the
    /// positions counted as partial.
    #[test]
    fn a_segment_knows_what_moved_and_not_where_positions_stand() {
        let tape = lifecycle();
        let segment = Replay::fold(&tape[3..9]);

        let taker = segment.position(U256::from(1)).unwrap();
        assert_eq!(taker.opened(), None);
        assert_eq!(taker.taker_size(), None);
        assert_eq!(
            taker.kind(),
            PositionKind::Taker {
                moved: PerpDelta::new(200_000),
                sized: false,
            }
        );
        assert_eq!(taker.liquidations(), 1);

        let maker = segment.position(U256::from(2)).unwrap();
        assert_eq!(maker.opened(), None);
        assert_eq!(maker.maker_band(), None, "the level is unknown");
        assert_eq!(maker.deposit_pool_price(), None);
        assert_eq!(
            maker.kind(),
            PositionKind::Maker {
                range: Some(TickRange::new(-600, 600).unwrap()),
                liquidity: LDelta::new(-400),
                sized: false,
                deposit_pool_price: None,
            }
        );

        assert_eq!(
            segment.pool_ticks(),
            None,
            "a segment's tick map is not whole"
        );
        assert_eq!(segment.pool_liquidity(), None);
        assert_eq!(segment.pool_tick(), Some(10));
        assert_eq!(
            segment.gaps(),
            Gaps {
                partial_positions: 2,
                taker_size_unknown: 1,
                margin_unknown: 2,
                ..Gaps::default()
            }
        );
    }

    /// The combine law over positions and the pool's liquidity, at every cut, from
    /// genesis and as segments.
    #[test]
    fn positions_and_the_book_combine_at_every_cut() {
        let tape = lifecycle();
        let whole = genesis(&tape);
        let segments = Replay::fold(&tape);
        for cut in 0..=tape.len() {
            let mut from_genesis = genesis(&tape[..cut]);
            from_genesis.combine(Replay::fold(&tape[cut..]));
            assert_eq!(from_genesis, whole, "genesis cut at {cut}");

            let mut left = Replay::fold(&tape[..cut]);
            left.combine(Replay::fold(&tape[cut..]));
            assert_eq!(left, segments, "segment cut at {cut}");
        }
    }

    /// The base fixture, an accrual, then the two positions' lives.
    fn whole_market() -> Vec<TapeEvent> {
        let mut tape = tape();
        tape.push(row(
            15,
            0,
            MarketEvent::CumulativesAccrued {
                cumulatives: CumulativesInfo {
                    funding: Funding::from_x96(I256::try_from(1_000i64).unwrap()),
                    funding_div_sqrt_p: FundingPerSqrtPrice::from_x96(
                        I256::try_from(2_000i64).unwrap(),
                    ),
                    util_payments: PerSide::new(
                        Earnings::from_x96(U256::from(3)),
                        Earnings::from_x96(U256::from(4)),
                    ),
                    util_earnings: PerSide::new(
                        Earnings::from_x96(U256::from(5)),
                        Earnings::from_x96(U256::from(6)),
                    ),
                },
            },
        ));
        tape.extend(lifecycle());
        tape
    }

    /// What a read would have returned at the fold's block: every figure
    /// the fold holds, taken from its accessors, with a margin invented for
    /// each position since the tape never carries one.
    fn seed_of(market: &Replay, at: BlockContext) -> Seed {
        let module = |kind| market.module(kind).unwrap_or(Address::ZERO);
        let emas = market.emas().unwrap();
        Seed {
            perp: market.perp(),
            block: at,
            pool_price: market.pool_price().unwrap(),
            index: market.index().unwrap(),
            emas,
            rates: MarketRates {
                funding_per_day: market.funding_per_day().unwrap(),
                util_fee_per_day: market.util_fee_per_day().unwrap(),
                last_touch: emas.last_touch,
            },
            cumulatives: market.cumulatives().unwrap(),
            capacity: market.capacity_at(at).unwrap(),
            solvency: market.solvency().unwrap(),
            modules: ContractModules {
                beacon: module(ModuleKind::Beacon),
                fees: module(ModuleKind::Fees),
                funding: module(ModuleKind::Funding),
                marginRatios: module(ModuleKind::MarginRatios),
                priceImpact: module(ModuleKind::PriceImpact),
                pricing: module(ModuleKind::Pricing),
            },
            ticks: market.pool_ticks().unwrap(),
            tick: market.pool_tick().unwrap(),
            positions: market
                .positions()
                .open()
                .map(|(pos_id, state)| {
                    let margin = UsdcAtoms::new(1_000_000 + pos_id.to::<u128>());
                    let position = match (state.taker_size(), state.maker_band()) {
                        (Some(size), _) => SeedPosition::Taker { size, margin },
                        (_, Some(band)) => SeedPosition::Maker {
                            range: band.range,
                            liquidity: LDelta::new(band.liquidity.units() as i128),
                            margin,
                        },
                        _ => SeedPosition::Unknown { margin },
                    };
                    (pos_id, position)
                })
                .collect(),
        }
    }

    /// A fold seeded from the reads at a block and continued over the tape
    /// after it answers every read-shaped question as the fold from genesis
    /// does: a seed is a checkpoint.
    #[test]
    fn a_seed_is_a_checkpoint() {
        let tape = whole_market();
        // After block 22: a taker sized, a maker banded, the tick known.
        let cut = tape.iter().position(|row| row.block_number > 22).unwrap();
        let prefix = genesis(&tape[..cut]);
        let at = block_of(&tape[cut - 1]);
        let seed = seed_of(&prefix, at);
        assert_eq!(seed.positions.len(), 2);

        let mut seeded = Replay::from_seed(seed).unwrap();
        assert_eq!(seeded.block(), Some(at));
        assert_eq!(seeded.gaps().margin_unknown, 0, "a seed knows every margin");
        assert_eq!(
            seeded.gaps().partial_positions,
            0,
            "a seed knows every level"
        );
        for row in &tape[cut..] {
            seeded.apply(row);
        }
        let whole = genesis(&tape);
        let last = block_of(tape.last().unwrap());

        assert_eq!(seeded.applied(), whole.applied() - cut as u64);
        assert_eq!(seeded.block(), whole.block());
        assert_eq!(seeded.capacity_at(last), whole.capacity_at(last));
        assert_eq!(
            seeded.mark_at(last, 600).unwrap(),
            whole.mark_at(last, 600).unwrap()
        );
        assert_eq!(seeded.funding_per_day(), whole.funding_per_day());
        assert_eq!(seeded.cumulatives(), whole.cumulatives());
        assert_eq!(seeded.solvency(), whole.solvency());
        assert_eq!(
            seeded.module(ModuleKind::Pricing),
            whole.module(ModuleKind::Pricing)
        );
        assert_eq!(seeded.pool_ticks(), whole.pool_ticks());
        assert_eq!(seeded.pool_tick(), whole.pool_tick());
        assert_eq!(seeded.pool_liquidity(), whole.pool_liquidity());
        for (pos_id, expected) in whole.positions().iter() {
            let actual = seeded.position(pos_id).unwrap();
            assert_eq!(actual.is_open(), expected.is_open(), "{pos_id} open");
            assert_eq!(actual.taker_size(), expected.taker_size(), "{pos_id} size");
            assert_eq!(actual.maker_band(), expected.maker_band(), "{pos_id} band");
            assert_eq!(actual.closed(), expected.closed(), "{pos_id} close");
        }
        assert_eq!(seeded.gaps(), whole.gaps());
    }

    /// A seeded position's margin is the read's until an event touches the
    /// position; then it is unknown, since no event carries it.
    #[test]
    fn a_seeded_margin_is_known_until_an_event_touches_the_position() {
        let tape = whole_market();
        let cut = tape.iter().position(|row| row.block_number > 22).unwrap();
        let prefix = genesis(&tape[..cut]);
        let mut seeded = Replay::from_seed(seed_of(&prefix, block_of(&tape[cut - 1]))).unwrap();

        let taker = seeded.position(U256::from(1)).unwrap();
        assert_eq!(taker.margin(), Some(UsdcAtoms::new(1_000_001)));
        assert_eq!(taker.last(), None, "no event has touched it since the read");
        assert_eq!(seeded.gaps().margin_unknown, 0);

        // The maker trims its band: its margin moved, the taker's did not.
        seeded.apply(&tape[cut]);
        seeded.apply(&tape[cut + 1]);
        assert_eq!(
            seeded.position(U256::from(2)).unwrap().margin(),
            None,
            "touched"
        );
        assert_eq!(
            seeded.position(U256::from(1)).unwrap().margin(),
            Some(UsdcAtoms::new(1_000_001))
        );
        assert_eq!(seeded.gaps().margin_unknown, 1);
    }

    /// An event at or before the fold's point is refused and counted, never
    /// applied: a duplicate, a driver out of order, or a seed's own block
    /// delivered again.
    #[test]
    fn an_event_at_or_before_the_folds_point_is_refused_and_counted() {
        let tape = lifecycle();
        let mut market = genesis(&tape[..3]);
        let before = market.clone();

        market.apply(&tape[2]);
        market.apply(&tape[0]);
        assert_eq!(market.gaps().refused, 2);
        assert_eq!(market.applied(), before.applied());
        assert_eq!(market.point(), before.point());
        assert_eq!(market.positions(), before.positions());
        assert_eq!(market.pool_ticks(), before.pool_ticks());

        market.apply(&tape[3]);
        assert_eq!(market.applied(), before.applied() + 1);

        // A seed stands at the end of its block.
        let prefix = genesis(&tape[..3]);
        let mut seeded =
            Replay::from_seed(seed_of(&genesis(&whole_market()[..16]), block_of(&tape[2])))
                .unwrap();
        let _ = prefix;
        seeded.apply(&tape[2]);
        assert_eq!(
            seeded.gaps().refused,
            1,
            "the seed's block, delivered again"
        );
        seeded.apply(&tape[3]);
        assert_eq!(seeded.gaps().refused, 1);
        assert_eq!(seeded.applied(), 1);
    }
}
