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
use crate::units::{Price, UsdcAtoms, UsdcDelta};

use super::super::fold::{Fold, Latest};
use super::super::series::{Arrival, Arrivals, Retention};
use super::super::tape::TapeEvent;

/// What a taker's swap was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapAction {
    /// The position's opening trade.
    Opened,
    /// A change of size on an open position.
    Adjusted,
    /// The closing trade, a liquidation's included.
    Closed,
}

/// A taker's swap, as the event settled it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Swap {
    /// The position that swapped.
    pub pos_id: U256,
    /// What the swap was for.
    pub action: SwapAction,
    /// What moved, at what price, for what fees.
    pub info: SwapInfo,
}

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
    /// The positions already recorded as liquidated in the current
    /// transaction, so a close and the dedicated event that follows it
    /// make one arrival.
    in_tx: LiquidatedInTx,
}

/// The positions recorded as liquidated in one transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LiquidatedInTx {
    tx: Option<B256>,
    positions: Vec<U256>,
}

impl LiquidatedInTx {
    /// Note `pos_id` liquidated in `tx`; whether it is the first time.
    fn note(&mut self, tx: B256, pos_id: U256) -> bool {
        if self.tx != Some(tx) {
            self.tx = Some(tx);
            self.positions.clear();
        }
        if self.positions.contains(&pos_id) {
            return false;
        }
        self.positions.push(pos_id);
        true
    }

    /// Whether `pos_id` was already noted in `tx`.
    fn noted(&self, tx: B256, pos_id: U256) -> bool {
        self.tx == Some(tx) && self.positions.contains(&pos_id)
    }

    /// Merge the segment after this one: the same transaction continues
    /// the set; a different one replaces it.
    fn combine(&mut self, later: Self) {
        match (self.tx, later.tx) {
            (Some(mine), Some(theirs)) if mine == theirs => {
                for pos_id in later.positions {
                    if !self.positions.contains(&pos_id) {
                        self.positions.push(pos_id);
                    }
                }
            }
            (_, Some(_)) => *self = later,
            (_, None) => {}
        }
    }
}

impl Activity {
    /// As a read supplied the prices at one block.
    pub(super) fn seeded(pool_price: Price, index: Price) -> Self {
        Self {
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

    fn arrival<M>(event: &TapeEvent, mark: M) -> Arrival<M> {
        Arrival {
            point: event.point(),
            timestamp: event.timestamp,
            tx: event.tx_hash,
            mark,
        }
    }

    fn liquidated(
        &mut self,
        event: &TapeEvent,
        pos_id: U256,
        role: PositionRole,
        fee: UsdcAtoms,
        pool_price_after: Option<Price>,
    ) {
        if !self.in_tx.note(event.tx_hash, pos_id) {
            return;
        }
        let mark = Liquidation {
            pos_id,
            role,
            fee,
            pool_price_before: self.pool_price.get(),
            pool_price_after,
            index_then: self.index.get(),
        };
        self.liquidations.push(Self::arrival(event, mark));
    }

    fn swapped(&mut self, event: &TapeEvent, pos_id: U256, action: SwapAction, info: SwapInfo) {
        self.swaps.push(Self::arrival(
            event,
            Swap {
                pos_id,
                action,
                info,
            },
        ));
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
                self.settlements.push(Self::arrival(
                    event,
                    Settlement::taker(pos_id, funding, util_fees),
                ));
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
                self.settlements.push(Self::arrival(
                    event,
                    Settlement::taker(pos_id, funding, util_fees),
                ));
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
                self.settlements.push(Self::arrival(
                    event,
                    Settlement::taker(pos_id, funding, util_fees),
                ));
            }
            MarketEvent::MakerAdjusted { pos_id, settle }
            | MarketEvent::MakerBackstopped { pos_id, settle, .. } => {
                self.settlements
                    .push(Self::arrival(event, Settlement::maker(pos_id, settle)));
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
                    .push(Self::arrival(event, Settlement::maker(pos_id, settle)));
            }
            MarketEvent::MakerLiquidated {
                pos_id,
                liquidation_fee,
                ..
            } => self.liquidated(event, pos_id, PositionRole::Maker, liquidation_fee, None),
            MarketEvent::IndexUpdated { index } => {
                self.prints.push(Self::arrival(event, index));
                self.index.set(index);
            }
            _ => {}
        }
    }

    /// Two things a cut inside a transaction or before a price would have
    /// known: a liquidation the later segment recorded again, because this
    /// one had already recorded it in the same transaction, is dropped; a
    /// liquidation the later segment recorded before its first swap or
    /// print is filled with the price this segment held then. Both as the
    /// fold of both segments in sequence would have done.
    fn combine(&mut self, mut later: Self) {
        later
            .liquidations
            .drop_where(|a| self.in_tx.noted(a.tx, a.mark.pos_id));
        for arrival in later.liquidations.arrivals_mut() {
            if arrival.mark.pool_price_before.is_none() {
                arrival.mark.pool_price_before = self.pool_price.get();
            }
            if arrival.mark.index_then.is_none() {
                arrival.mark.index_then = self.index.get();
            }
        }
        self.swaps.combine(later.swaps);
        self.liquidations.combine(later.liquidations);
        self.settlements.combine(later.settlements);
        self.prints.combine(later.prints);
        self.pool_price.combine(later.pool_price);
        self.index.combine(later.index);
        self.in_tx.combine(later.in_tx);
    }
}
