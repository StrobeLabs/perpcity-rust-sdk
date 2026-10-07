//! What arrived on the market, as point processes: every swap, every
//! liquidation, every settlement, every print. The marks carry what the
//! event said and, for a liquidation, the prices standing around it.
//!
//! A liquidation is the one record the vocabulary lacks. On the beta builds
//! it is a tailed `TakerClosed`, a `MakerConverted` or `MakerClosed` with
//! `is_liquidation`, or a dedicated `TakerLiquidated`/`MakerLiquidated`
//! after the close, by build and side. This fold says it once: one arrival
//! per position per liquidating transaction, whichever events carried it.

use alloy::primitives::{B256, U256};

use crate::client::PositionRole;
use crate::events::{MakerSettle, MarketEvent, SwapInfo};
use crate::history::fold::{Arrivals, Fold, Latest, Retention, Sample};
use crate::history::tape::{Positioned, Swap, SwapAction, TapeEvent};
use crate::units::{Price, UsdcAtoms, UsdcDelta};

/// A position liquidated, with the prices standing around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Liquidation {
    /// The position liquidated.
    pub pos_id: U256,
    /// Whether it was a band or a directional position.
    pub role: PositionRole,
    /// The fee the liquidator took, where the event carried it.
    pub fee: UsdcAtoms,
    /// The pool price as the last swap before this transaction left it.
    pub pool_price_before: Option<Price>,
    /// The pool price after the liquidation's own swap: a taker's close is
    /// a swap, a maker's is not.
    pub pool_price_after: Option<Price>,
    /// The beacon's index as last printed before this transaction.
    pub index_then: Option<Price>,
}

/// What a position settled when it was adjusted, converted, closed or
/// backstopped: funding, utilization fees and, for a maker, the LP fees
/// it earned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    /// The position that settled.
    pub pos_id: U256,
    /// Whether it was a band or a directional position.
    pub role: PositionRole,
    /// Funding settled, signed as the contract settles it.
    pub funding: UsdcDelta,
    /// Utilization fees settled, both sides summed for a maker.
    pub util_fees: UsdcAtoms,
    /// LP fees earned; zero for a taker.
    pub lp_fees: UsdcAtoms,
}

impl Settlement {
    fn maker(pos_id: U256, settle: MakerSettle) -> Self {
        Self {
            pos_id,
            role: PositionRole::Maker,
            funding: settle.funding,
            util_fees: settle.util_fees.long + settle.util_fees.short,
            lp_fees: settle.lp_fees,
        }
    }

    fn taker(pos_id: U256, funding: UsdcDelta, util_fees: UsdcAtoms) -> Self {
        Self {
            pos_id,
            role: PositionRole::Taker,
            funding,
            util_fees,
            lp_fees: UsdcAtoms::ZERO,
        }
    }
}

impl Positioned for Liquidation {
    fn pos_id(&self) -> U256 {
        self.pos_id
    }
}

impl Positioned for Settlement {
    fn pos_id(&self) -> U256 {
        self.pos_id
    }
}

/// The market's point processes.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Activity {
    pub(super) swaps: Arrivals<Swap>,
    pub(super) liquidations: Arrivals<Liquidation>,
    pub(super) settlements: Arrivals<Settlement>,
    pub(super) prints: Arrivals<Price>,
    /// The prices a liquidation is recorded against; watched here so a
    /// segment's record is filled from the one before it at the cut.
    pool_price: Latest<Price>,
    index: Latest<Price>,
    /// What the current transaction has done so far, since a liquidation
    /// is said across several of its events.
    in_tx: InTx,
}

/// One transaction's progress: the pool price when it began, the last
/// swap each position made in it, and the positions already recorded as
/// liquidated in it. A liquidation's record is assembled from these, so
/// the tailed close of one build and the close-then-dedicated-event of the
/// other give the same record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct InTx {
    tx: Option<B256>,
    /// The pool price as the last swap before this transaction left it.
    price_at_start: Option<Price>,
    /// The pool price after each position's last swap in this transaction.
    swaps: Vec<(U256, Price)>,
    liquidated: Vec<U256>,
}

impl InTx {
    /// Make `tx` the current transaction, if it is not already, with
    /// `pool_price` as the price standing when it began.
    fn enter(&mut self, tx: B256, pool_price: Option<Price>) {
        if self.tx != Some(tx) {
            *self = Self {
                tx: Some(tx),
                price_at_start: pool_price,
                swaps: Vec::new(),
                liquidated: Vec::new(),
            };
        }
    }

    fn swapped(&mut self, pos_id: U256, after: Price) {
        match self.swaps.iter_mut().find(|(id, _)| *id == pos_id) {
            Some(entry) => entry.1 = after,
            None => self.swaps.push((pos_id, after)),
        }
    }

    fn swap_of(&self, pos_id: U256) -> Option<Price> {
        self.swaps
            .iter()
            .find(|(id, _)| *id == pos_id)
            .map(|(_, price)| *price)
    }

    /// Note `pos_id` liquidated; whether it is the first time this
    /// transaction says so.
    fn liquidate(&mut self, pos_id: U256) -> bool {
        if self.liquidated.contains(&pos_id) {
            return false;
        }
        self.liquidated.push(pos_id);
        true
    }

    fn liquidated(&self, tx: B256, pos_id: U256) -> bool {
        self.tx == Some(tx) && self.liquidated.contains(&pos_id)
    }

    /// Merge the segment after this one: the same transaction continues
    /// this one's progress; a different one replaces it, taking the price
    /// this segment held if the later one had none.
    fn combine(&mut self, later: Self, price_before_later: Option<Price>) {
        match (self.tx, later.tx) {
            (Some(mine), Some(theirs)) if mine == theirs => {
                for (pos_id, after) in later.swaps {
                    self.swapped(pos_id, after);
                }
                for pos_id in later.liquidated {
                    self.liquidate(pos_id);
                }
            }
            (_, Some(_)) => {
                *self = later;
                if self.price_at_start.is_none() {
                    self.price_at_start = price_before_later;
                }
            }
            (_, None) => {}
        }
    }
}

impl Activity {
    /// Before the market's first event: every process complete from the
    /// start.
    pub(super) fn genesis() -> Self {
        Self {
            swaps: Arrivals::from_genesis(),
            liquidations: Arrivals::from_genesis(),
            settlements: Arrivals::from_genesis(),
            prints: Arrivals::from_genesis(),
            ..Self::default()
        }
    }

    /// As a read supplied the prices at the end of a block, `at`: every
    /// process complete after it.
    pub(super) fn seeded(at: Sample<()>, pool_price: Price, index: Price) -> Self {
        Self {
            swaps: Arrivals::after(at),
            liquidations: Arrivals::after(at),
            settlements: Arrivals::after(at),
            prints: Arrivals::after(at),
            pool_price: Latest::stated(pool_price),
            index: Latest::stated(index),
            ..Self::default()
        }
    }

    pub(super) fn retain(&mut self, retention: Retention) {
        self.swaps.retain(retention);
        self.liquidations.retain(retention);
        self.settlements.retain(retention);
        self.prints.retain(retention);
    }

    pub(super) fn advance(&mut self, at: Sample<()>) {
        self.swaps.advance(at);
        self.liquidations.advance(at);
        self.settlements.advance(at);
        self.prints.advance(at);
    }

    /// Record `pos_id` liquidated in this event's transaction, once per
    /// transaction however many events say it. The price before is the
    /// one standing when the transaction began; the price after is the
    /// position's own swap in it, which the tailed close carries on the
    /// event and the dedicated event finds in the transaction's progress.
    fn liquidated(
        &mut self,
        event: &TapeEvent,
        pos_id: U256,
        role: PositionRole,
        fee: UsdcAtoms,
        pool_price_after: Option<Price>,
    ) {
        self.in_tx.enter(event.tx_hash, self.pool_price.get());
        if !self.in_tx.liquidate(pos_id) {
            return;
        }
        let mark = Liquidation {
            pos_id,
            role,
            fee,
            pool_price_before: self.in_tx.price_at_start,
            pool_price_after: pool_price_after.or_else(|| self.in_tx.swap_of(pos_id)),
            index_then: self.index.get(),
        };
        self.liquidations.push(event.arrival(mark));
    }

    fn swapped(&mut self, event: &TapeEvent, pos_id: U256, action: SwapAction, info: SwapInfo) {
        self.in_tx.enter(event.tx_hash, self.pool_price.get());
        self.in_tx.swapped(pos_id, info.pool_price);
        self.swaps.push(event.arrival(Swap {
            pos_id,
            action,
            info,
        }));
        self.pool_price.set(info.pool_price);
    }
}

impl Fold for Activity {
    fn apply(&mut self, event: &TapeEvent) {
        match event.event {
            MarketEvent::TakerOpened { pos_id, swap } => {
                self.swapped(event, pos_id, SwapAction::Opened, swap);
            }
            MarketEvent::TakerAdjusted {
                pos_id,
                swap,
                funding,
                util_fees,
            } => {
                self.swapped(event, pos_id, SwapAction::Adjusted, swap);
                self.settlements
                    .push(event.arrival(Settlement::taker(pos_id, funding, util_fees)));
            }
            MarketEvent::TakerClosed {
                pos_id,
                swap,
                funding,
                util_fees,
                liquidation_fee,
                is_liquidation,
            } => {
                // The record wants the price before the close's own swap.
                if is_liquidation {
                    self.liquidated(
                        event,
                        pos_id,
                        PositionRole::Taker,
                        liquidation_fee,
                        Some(swap.pool_price),
                    );
                }
                self.swapped(event, pos_id, SwapAction::Closed, swap);
                self.settlements
                    .push(event.arrival(Settlement::taker(pos_id, funding, util_fees)));
            }
            MarketEvent::TakerLiquidated {
                pos_id,
                liquidation_fee,
                ..
            } => self.liquidated(event, pos_id, PositionRole::Taker, liquidation_fee, None),
            MarketEvent::TakerBackstopped {
                pos_id,
                funding,
                util_fees,
                ..
            } => {
                self.settlements
                    .push(event.arrival(Settlement::taker(pos_id, funding, util_fees)));
            }
            MarketEvent::MakerAdjusted { pos_id, settle }
            | MarketEvent::MakerBackstopped { pos_id, settle, .. } => {
                self.settlements
                    .push(event.arrival(Settlement::maker(pos_id, settle)));
            }
            MarketEvent::MakerConverted {
                pos_id,
                settle,
                liquidation_fee,
                is_liquidation,
            }
            | MarketEvent::MakerClosed {
                pos_id,
                settle,
                liquidation_fee,
                is_liquidation,
            } => {
                if is_liquidation {
                    self.liquidated(event, pos_id, PositionRole::Maker, liquidation_fee, None);
                }
                self.settlements
                    .push(event.arrival(Settlement::maker(pos_id, settle)));
            }
            MarketEvent::MakerLiquidated {
                pos_id,
                liquidation_fee,
                ..
            } => self.liquidated(event, pos_id, PositionRole::Maker, liquidation_fee, None),
            MarketEvent::IndexUpdated { index } => {
                self.prints.push(event.arrival(index));
                self.index.set(index);
            }
            _ => {}
        }
        self.advance(event.sample(()));
    }

    /// What a cut inside a transaction or before a price would have known,
    /// repaired as the fold of both segments in sequence would have recorded
    /// it: a liquidation the later segment said again, because this one had
    /// already said it in the same transaction, is dropped; one recorded
    /// before the later segment saw a price, or saw the position's own swap
    /// earlier in the transaction, takes what this segment held.
    fn combine(&mut self, mut later: Self) {
        later
            .liquidations
            .drop_where(|a| self.in_tx.liquidated(a.tx, a.mark.pos_id));
        for arrival in later.liquidations.arrivals_mut() {
            let mark = &mut arrival.mark;
            let same_tx = self.in_tx.tx == Some(arrival.tx);
            if mark.pool_price_before.is_none() {
                mark.pool_price_before = if same_tx {
                    self.in_tx.price_at_start
                } else {
                    self.pool_price.get()
                };
            }
            if mark.pool_price_after.is_none() && same_tx {
                mark.pool_price_after = self.in_tx.swap_of(mark.pos_id);
            }
            if mark.index_then.is_none() {
                mark.index_then = self.index.get();
            }
        }
        self.swaps.combine(later.swaps);
        self.liquidations.combine(later.liquidations);
        self.settlements.combine(later.settlements);
        self.prints.combine(later.prints);
        let price_before_later = self.pool_price.get();
        self.pool_price.combine(later.pool_price);
        self.index.combine(later.index);
        self.in_tx.combine(later.in_tx, price_before_later);
    }
}
