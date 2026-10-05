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

mod activity;
mod market;
mod pool;
mod positions;
mod seed;
mod solvency;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::result::Result as StdResult;

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;

use crate::client::{OpenInterest, SolvencyState, StateAt};
use crate::errors::{Result, ValidationError};
use crate::events::{CumulativesInfo, ModuleKind};
use crate::math::BlockContext;
use crate::math::capacity::{Capacity, MarketCapacity};
use crate::math::pricing::{Emas, Mark};
use crate::math::swap::TickLiquidity;
use crate::units::{FundingRate, LUnits, PerSide, Price, UsdcAtoms, UtilizationRate};

use self::activity::Activity;
pub use self::activity::{Liquidation, Settlement, Swap, SwapAction};
use self::market::{Modules, Prices, Rates, Utilization};
use self::pool::Pool;
pub use self::positions::{PositionKind, PositionState, Positions};
use self::seed::Seed;
use self::solvency::Solvency;
use super::History;
use super::fold::{Arrivals, Fold, Latest, Retention, Sample, Sequenced, Series};
use super::tape::{ChainPoint, OwnershipLog, TapeAddresses, TapeEvent};

/// What the fold does not know, in three kinds, each with its own cure. A
/// reading with a nonzero gap is forensic, not a decision's input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gaps {
    /// What the contract moved without saying so. Cured by the cutover.
    pub silences: Silences,
    /// What the fold's start did not supply. Cured by a seed.
    pub unknowns: Unknowns,
    /// What the driver did wrong. Cured by [`Replay::catch_up`].
    pub faults: Faults,
}

/// How many times a figure may have moved without an event saying so,
/// since the last event that stated it. Both are the live contract builds':
/// the next build emits what repaid the debt on every swap and the margin
/// total on every path that moves it, and these go with it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Silences {
    /// Moves of `totalMargin` no event carries, since the last
    /// `MarginTransferred`: a donation adds the debt it repaid, a bad-debt
    /// booking adds the insurance it consumed, and a swap while debt stands
    /// removes less than its gross fees by what repaid the debt.
    pub total_margin_unemitted: u32,
    /// Swaps that paid an insurance fee while bad debt stood, since the
    /// last event that stated the debt. The fee repays debt first, and no
    /// event on the live builds carries the amount.
    pub bad_debt_unemitted: u32,
}

/// Positions standing on a figure the fold was never told: a fold from a
/// segment knows what moved and not where anything stands, and no live
/// event carries margin. A seed supplies all three.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unknowns {
    /// Open positions whose level the fold does not know: the open unseen
    /// and no read to supply it, so their size or band is what moved since,
    /// not where they stand. Includes `taker_size_unknown`.
    pub partial_positions: u32,
    /// Open takers whose size no event stated: first seen mid-life, or
    /// converted from a maker, whose inventory the live builds never emit.
    pub taker_size_unknown: u32,
    /// Open positions whose margin the fold does not know: every position a
    /// read did not supply, and every one an event has touched since.
    pub margin_unknown: u32,
}

/// What the driver did that the fold would not take.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Faults {
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
/// positions, custody, and what arrived. `apply` hands every event to
/// each; `combine` merges each with its counterpart. The one rule of the
/// live build's, how a swap's fees leave the margin total, lives in the
/// solvency fold alone.
///
/// What a figure was is asked as often as what it is, so the folds keep
/// the history of what they track: the prices, capacity, open interest and
/// the stated books as a [`Series`], the swaps, liquidations, settlements
/// and prints as [`Arrivals`], all trimmed to the replay's
/// [`retention`](Self::retaining).
///
/// Three starts: [`from_genesis`](Self::from_genesis) before the market's
/// first event, [`seeded`](Self::seeded) from the reads at a block, and the
/// trait's `fold` for a segment. From any of them, [`catch_up`](Self::catch_up)
/// applies the tape from the block after the fold's to the lagged head.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Replay {
    perp: Address,
    events: u64,
    /// The block of the last event applied, or the seed's.
    block: Latest<BlockContext>,
    market: Sequenced<Market>,
}

/// The nine folds the market is composed of; `Replay` keeps them behind
/// the chain-order guard.
#[derive(Debug, Clone, Default, PartialEq)]
struct Market {
    prices: Prices,
    rates: Rates,
    utilization: Utilization,
    solvency: Solvency,
    modules: Modules,
    pool: Pool,
    positions: Positions,
    custody: OwnershipLog,
    activity: Activity,
}

impl Market {
    /// Keep only `retention` of every series from here on.
    fn retain(&mut self, retention: Retention) {
        self.prices.retain(retention);
        self.utilization.retain(retention);
        self.solvency.retain(retention);
        self.activity.retain(retention);
    }
}

impl Fold for Market {
    fn apply(&mut self, event: &TapeEvent) {
        self.prices.apply(event);
        self.rates.apply(event);
        self.utilization.apply(event);
        self.solvency.apply(event);
        self.modules.apply(event);
        self.pool.apply(event);
        self.positions.apply(event);
        self.custody.apply(event);
        self.activity.apply(event);
    }

    fn combine(&mut self, later: Self) {
        self.prices.combine(later.prices);
        self.rates.combine(later.rates);
        self.utilization.combine(later.utilization);
        self.solvency.combine(later.solvency);
        self.modules.combine(later.modules);
        self.pool.combine(later.pool);
        self.positions.combine(later.positions);
        self.custody.combine(later.custody);
        self.activity.combine(later.activity);
    }
}

impl Replay {
    fn market(&self) -> &Market {
        self.market.inner()
    }

    /// Keep only `retention` of every series: the last window for a
    /// monitor, everything for a forensic or research fold. The default is
    /// everything. A series trimmed to a window answers only inside it and
    /// says so.
    pub fn retaining(mut self, retention: Retention) -> Self {
        self.market.inner_mut().retain(retention);
        self
    }

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
            events: 0,
            block: Latest::default(),
            market: Sequenced::new(Market {
                utilization: Utilization::genesis(),
                solvency: Solvency::genesis(),
                pool: Pool::genesis(),
                ..Market::default()
            }),
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
    /// [`Faults::refused`].
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
        let at = Sample {
            point: end_of_block,
            timestamp: seed.block.timestamp,
            value: (),
        };
        let market = Market {
            prices: Prices::seeded(at, seed.pool_price, seed.index, seed.emas),
            rates: Rates::seeded(seed.rates, seed.cumulatives),
            utilization: Utilization::seeded(at, seed.capacity),
            solvency: Solvency::seeded(at, seed.solvency),
            modules: Modules::seeded(seed.modules),
            pool: Pool::seeded(seed.ticks, seed.tick)?,
            positions: Positions::seeded(seed.pool_price, seed.positions),
            custody: OwnershipLog::default(),
            activity: Activity::seeded(seed.pool_price, seed.index),
        };
        Ok(Self {
            perp: seed.perp,
            events: 0,
            block: Latest::stated(seed.block),
            market: Sequenced::standing_at(market, end_of_block),
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

    /// Where the fold stands: the last event's chain point, or the end of
    /// the seed's block.
    pub fn point(&self) -> Option<ChainPoint> {
        self.market.point()
    }

    /// The block of the last event applied, or the seed's.
    pub fn block(&self) -> Option<BlockContext> {
        self.block.get()
    }

    /// The pool price after each swap; its latest is the pool price now.
    pub fn pool_price(&self) -> &Series<Price> {
        &self.market().prices.pool
    }

    /// The beacon's prints as the index that held between them; its latest
    /// is the index now.
    pub fn index(&self) -> &Series<Price> {
        &self.market().prices.index
    }

    /// The stored EMA pair and the touch it is current as of, as the last
    /// `RatesAndEmasRefreshed` left them.
    pub fn emas(&self) -> Option<Emas> {
        self.market().prices.emas.get()
    }

    /// The funding rate the last touch set.
    pub fn funding_per_day(&self) -> Option<FundingRate> {
        self.market().rates.funding_per_day.get()
    }

    /// The utilization fee rates the last touch set, per side.
    pub fn util_fee_per_day(&self) -> Option<PerSide<UtilizationRate>> {
        self.market().rates.util_fee_per_day.get()
    }

    /// The market's accumulators at the last accrual.
    pub fn cumulatives(&self) -> Option<CumulativesInfo> {
        self.market().rates.cumulatives.get()
    }

    /// Taker open interest per side, as each update stated it.
    pub fn open_interest(&self) -> &Series<OpenInterest> {
        &self.market().utilization.open_interest
    }

    /// Capacity per side, as each update stated it.
    pub fn capacity(&self) -> &Series<Capacity> {
        &self.market().utilization.capacity
    }

    /// Capacity and its draw at `block`, as [`StateAt::capacity`](crate::StateAt::capacity)
    /// reads them; `None` until both have been stated.
    pub fn capacity_at(&self, block: BlockContext) -> Option<MarketCapacity> {
        self.market().utilization.at(block)
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
        self.market().prices.mark_at(block, ema_window)
    }

    /// The solvency books: the debt as last stated, the margin total as
    /// last stated less the swap fees the build removed since. `None` until
    /// both have been stated. Read [`Self::gaps`] beside it.
    pub fn solvency(&self) -> Option<SolvencyState> {
        self.market().solvency.state()
    }

    /// The margin total as each `MarginTransferred` stated it. The fees
    /// swaps removed since the last statement are not in it; the books
    /// ([`Self::solvency`]) carry those.
    pub fn margin_total(&self) -> &Series<UsdcAtoms> {
        &self.market().solvency.margin_stated
    }

    /// The bad debt as each booking, socialization or donation stated it.
    pub fn bad_debt(&self) -> &Series<UsdcAtoms> {
        &self.market().solvency.debt_stated
    }

    /// USDC that entered as margin since the fold's start: every positive
    /// `MarginTransferred` delta, summed. What bad debt is set against,
    /// since no trading loss can exceed what was ever deposited.
    pub fn deposited(&self) -> UsdcAtoms {
        self.market().solvency.deposited
    }

    /// Every taker swap, in chain order.
    pub fn swaps(&self) -> &Arrivals<Swap> {
        &self.market().activity.swaps
    }

    /// Every liquidation, one per position per liquidating transaction,
    /// whichever build's events carried it, with the prices around it.
    pub fn liquidations(&self) -> &Arrivals<Liquidation> {
        &self.market().activity.liquidations
    }

    /// Every settlement a position made: funding, utilization fees and,
    /// for makers, LP fees.
    pub fn settlements(&self) -> &Arrivals<Settlement> {
        &self.market().activity.settlements
    }

    /// The beacon's prints as arrivals, for when their timing is the
    /// question; [`Self::index`] is the same prints as the value that held.
    pub fn prints(&self) -> &Arrivals<Price> {
        &self.market().activity.prints
    }

    /// The module of `kind` in force, once a `ModuleSet` has named one.
    pub fn module(&self, kind: ModuleKind) -> Option<Address> {
        self.market().modules.get(kind)
    }

    /// Who held each position when: the custody fold, taken in the same
    /// pass.
    pub fn custody(&self) -> &OwnershipLog {
        &self.market().custody
    }

    /// Every position the tape mentioned: takers sized by their swaps,
    /// makers by their liquidity changes.
    pub fn positions(&self) -> &Positions {
        &self.market().positions
    }

    /// One position, if the tape mentioned it.
    pub fn position(&self, pos_id: U256) -> Option<&PositionState> {
        self.positions().get(pos_id)
    }

    /// The pool's tick, as [`StateAt::pool`](crate::StateAt::pool) reads
    /// it: where the last swap that moved it left it. `None` until a swap
    /// has moved it, since the pool's first tick is the factory's to say.
    pub fn pool_tick(&self) -> Option<i32> {
        self.market().pool.tick.get()
    }

    /// Liquidity at every initialized tick, as `PoolSnapshot::ticks` reads
    /// it. `None` for a fold that did not start at genesis: a segment knows
    /// what changed, not what stands.
    pub fn pool_ticks(&self) -> Option<BTreeMap<i32, TickLiquidity>> {
        self.market().pool.ticks()
    }

    /// Liquidity active at the pool's tick, as `PoolSnapshot::liquidity`
    /// reads it: the net of every initialized tick at or below it. `None`
    /// until the tick is known, or for a fold that did not start at genesis.
    pub fn pool_liquidity(&self) -> Option<LUnits> {
        self.market().pool.liquidity()
    }

    /// What the fold does not know, in three kinds with three cures. All
    /// decided at the read, from the latest totals and the positions, so the
    /// counts combine like the rest.
    pub fn gaps(&self) -> Gaps {
        let market = self.market();
        let mut unknowns = Unknowns::default();
        for (_, position) in market.positions.open() {
            unknowns.margin_unknown += u32::from(position.margin().is_none());
            unknowns.partial_positions += u32::from(!position.level_known());
            if let PositionKind::Taker { sized: false, .. } = position.kind() {
                unknowns.taker_size_unknown += 1;
            }
        }
        Gaps {
            silences: market.solvency.silences(),
            unknowns,
            faults: Faults {
                refused: self.market.refused(),
            },
        }
    }
}

impl Fold for Replay {
    /// Every event goes through the chain-order guard: one at or before the
    /// fold's point is refused and counted, never applied.
    fn apply(&mut self, event: &TapeEvent) {
        if self.market.accept(event) {
            self.events += 1;
            self.block.set(BlockContext {
                number: event.block_number,
                hash: event.block_hash,
                timestamp: event.timestamp,
            });
        }
    }

    fn combine(&mut self, later: Self) {
        if self.perp.is_zero() {
            self.perp = later.perp;
        }
        self.events += later.events;
        self.block.combine(later.block);
        self.market.combine(later.market);
    }
}
