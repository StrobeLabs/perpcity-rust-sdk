//! The market's emitted totals, each a latest-wins value: the mark's three
//! inputs, the touch's rates and accumulators, capacity and the open
//! interest drawn on it, and the module in force for each kind.

use alloy::primitives::Address;

use crate::client::{MarketRates, OpenInterest};
use crate::contracts;
use crate::errors::ValidationError;
use crate::events::{CumulativesInfo, MarketEvent, ModuleKind};
use crate::math::BlockContext;
use crate::math::capacity::{Capacity, MarketCapacity};
use crate::math::pricing::{Emas, Mark};
use crate::units::{FundingRate, PerSide, Price, UtilizationRate};

use super::super::fold::{Fold, Latest};
use super::super::tape::TapeEvent;

/// What the contract marks from: the pool price after the last swap, the
/// beacon's last print, and the stored EMAs as the last touch left them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct Prices {
    pool: Latest<Price>,
    index: Latest<Price>,
    emas: Latest<Emas>,
}

impl Prices {
    /// As a read supplied them at one block.
    pub(super) fn seeded(pool: Price, index: Price, emas: Emas) -> Self {
        Self {
            pool: Latest::stated(pool),
            index: Latest::stated(index),
            emas: Latest::stated(emas),
        }
    }

    pub(super) fn pool(&self) -> Option<Price> {
        self.pool.get()
    }

    pub(super) fn index(&self) -> Option<Price> {
        self.index.get()
    }

    pub(super) fn emas(&self) -> Option<Emas> {
        self.emas.get()
    }

    /// The mark at `block`, with the EMAs advanced to its timestamp over
    /// `ema_window`; `None` until a swap, a print and a touch have been seen.
    pub(super) fn mark_at(
        &self,
        block: BlockContext,
        ema_window: u64,
    ) -> Result<Option<Mark>, ValidationError> {
        let (Some(pool), Some(index), Some(emas)) = (self.pool(), self.index(), self.emas()) else {
            return Ok(None);
        };
        Mark::advanced(
            block,
            pool,
            index,
            emas.pair()?,
            emas.last_touch,
            ema_window,
        )
        .map(Some)
    }
}

impl Fold for Prices {
    fn apply(&mut self, event: &TapeEvent) {
        match event.event {
            MarketEvent::TakerOpened { swap, .. }
            | MarketEvent::TakerAdjusted { swap, .. }
            | MarketEvent::TakerClosed { swap, .. } => self.pool.set(swap.pool_price),
            MarketEvent::IndexUpdated { index } => self.index.set(index),
            MarketEvent::RatesAndEmasRefreshed {
                last_touch,
                pool_price_ema,
                index_ema,
                ..
            } => self.emas.set(Emas {
                amm_price: pool_price_ema,
                index: index_ema,
                last_touch,
            }),
            _ => {}
        }
    }

    fn combine(&mut self, later: Self) {
        self.pool.combine(later.pool);
        self.index.combine(later.index);
        self.emas.combine(later.emas);
    }
}

/// What the touch sets: the funding and utilization rates, and the
/// accumulators at the last accrual.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct Rates {
    funding_per_day: Latest<FundingRate>,
    util_fee_per_day: Latest<PerSide<UtilizationRate>>,
    cumulatives: Latest<CumulativesInfo>,
}

impl Rates {
    /// As a read supplied them at one block.
    pub(super) fn seeded(rates: MarketRates, cumulatives: CumulativesInfo) -> Self {
        Self {
            funding_per_day: Latest::stated(rates.funding_per_day),
            util_fee_per_day: Latest::stated(rates.util_fee_per_day),
            cumulatives: Latest::stated(cumulatives),
        }
    }

    pub(super) fn funding_per_day(&self) -> Option<FundingRate> {
        self.funding_per_day.get()
    }

    pub(super) fn util_fee_per_day(&self) -> Option<PerSide<UtilizationRate>> {
        self.util_fee_per_day.get()
    }

    pub(super) fn cumulatives(&self) -> Option<CumulativesInfo> {
        self.cumulatives.get()
    }
}

impl Fold for Rates {
    fn apply(&mut self, event: &TapeEvent) {
        match event.event {
            MarketEvent::RatesAndEmasRefreshed {
                funding_per_day,
                util_fee_per_day,
                ..
            } => {
                self.funding_per_day.set(funding_per_day);
                self.util_fee_per_day.set(util_fee_per_day);
            }
            MarketEvent::CumulativesAccrued { cumulatives } => self.cumulatives.set(cumulatives),
            _ => {}
        }
    }

    fn combine(&mut self, later: Self) {
        self.funding_per_day.combine(later.funding_per_day);
        self.util_fee_per_day.combine(later.util_fee_per_day);
        self.cumulatives.combine(later.cumulatives);
    }
}

/// Capacity and the open interest drawn on it, both totals the contract
/// emits whole and both zero before a market's first event.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct Utilization {
    capacity: Latest<Capacity>,
    open_interest: Latest<OpenInterest>,
}

impl Utilization {
    /// Before the first event: nothing supplied, nothing drawn.
    pub(super) fn genesis() -> Self {
        Self {
            capacity: Latest::stated(PerSide::default()),
            open_interest: Latest::stated(PerSide::default()),
        }
    }

    /// As a read supplied them at one block.
    pub(super) fn seeded(read: MarketCapacity) -> Self {
        Self {
            capacity: Latest::stated(read.capacity),
            open_interest: Latest::stated(read.open_interest),
        }
    }

    pub(super) fn open_interest(&self) -> Option<OpenInterest> {
        self.open_interest.get()
    }

    /// Both at `block`, as the capacity read returns them.
    pub(super) fn at(&self, block: BlockContext) -> Option<MarketCapacity> {
        Some(MarketCapacity {
            block,
            capacity: self.capacity.get()?,
            open_interest: self.open_interest.get()?,
        })
    }
}

impl Fold for Utilization {
    fn apply(&mut self, event: &TapeEvent) {
        match event.event {
            MarketEvent::CapacityUpdated { capacity } => self.capacity.set(capacity),
            MarketEvent::OpenInterestUpdated { open_interest } => {
                self.open_interest.set(open_interest);
            }
            _ => {}
        }
    }

    fn combine(&mut self, later: Self) {
        self.capacity.combine(later.capacity);
        self.open_interest.combine(later.open_interest);
    }
}

/// The module in force for each of the six kinds, as governance last set
/// them. Unknown until a `ModuleSet` names one: the first set is on the
/// factory's creation log, which is not on the tape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Modules {
    in_force: [Latest<Address>; 6],
}

impl Modules {
    /// As `modules()` returned them at one block.
    pub(super) fn seeded(read: contracts::Modules) -> Self {
        let mut in_force = [Latest::default(); 6];
        for (kind, address) in [
            (ModuleKind::Beacon, read.beacon),
            (ModuleKind::Fees, read.fees),
            (ModuleKind::Funding, read.funding),
            (ModuleKind::MarginRatios, read.marginRatios),
            (ModuleKind::PriceImpact, read.priceImpact),
            (ModuleKind::Pricing, read.pricing),
        ] {
            in_force[Self::index(kind)].set(address);
        }
        Self { in_force }
    }

    pub(super) fn get(&self, kind: ModuleKind) -> Option<Address> {
        self.in_force[Self::index(kind)].get()
    }

    fn index(kind: ModuleKind) -> usize {
        match kind {
            ModuleKind::Beacon => 0,
            ModuleKind::Fees => 1,
            ModuleKind::Funding => 2,
            ModuleKind::MarginRatios => 3,
            ModuleKind::PriceImpact => 4,
            ModuleKind::Pricing => 5,
        }
    }
}

impl Fold for Modules {
    fn apply(&mut self, event: &TapeEvent) {
        if let MarketEvent::ModuleSet { module, address } = event.event {
            self.in_force[Self::index(module)].set(address);
        }
    }

    fn combine(&mut self, later: Self) {
        for (mine, theirs) in self.in_force.iter_mut().zip(later.in_force) {
            mine.combine(theirs);
        }
    }
}
