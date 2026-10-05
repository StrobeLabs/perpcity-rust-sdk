//! The solvency books from the tape: the margin total and the bad debt as
//! the contract last stated them, and what moved each since without an
//! event saying so.
//!
//! This is the one fold with a rule of the live build's in it. Every taker
//! swap removes its protocol, creator and insurance fees from `totalMargin`
//! with no event, and whether the transaction's `MarginTransferred` was
//! emitted before or after that removal is the path's: an open or a
//! deposit is transferred before the fees leave, a withdrawal after, a
//! liquidation's fee after the close event. The next build emits the total
//! on that path, and the rule goes with it.

use std::ops::AddAssign;

use alloy::primitives::B256;

use crate::client::SolvencyState;
use crate::events::MarketEvent;
use crate::history::fold::{Fold, Latest, Retention, Sample, Series, Stated};
use crate::history::tape::TapeEvent;
use crate::units::UsdcAtoms;

use super::Silences;

/// What moved the margin total since it was last stated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct MarginSince {
    /// Swap fees the build removed from the total.
    fees_removed: UsdcAtoms,
    /// Moves no event carried: a donation adding the debt it repaid, a
    /// bad-debt booking adding the insurance it consumed.
    unemitted: u32,
}

impl AddAssign for MarginSince {
    fn add_assign(&mut self, rhs: Self) {
        self.fees_removed += rhs.fees_removed;
        self.unemitted += rhs.unemitted;
    }
}

/// The margin total and the bad debt, folded from the events that state
/// them and the swaps that move them silently.
///
/// The two totals are also kept as series of what the contract stated,
/// statement by statement: the margin total at each `MarginTransferred`,
/// the bad debt at each booking, socialization or donation. Those are the
/// contract's own levels, so they append across segments; the fees a swap
/// removes silently are tracked beside them, not in them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Solvency {
    margin: Stated<UsdcAtoms, MarginSince>,
    /// Swaps that paid an insurance fee since the debt was last stated;
    /// whether they were silences is decided at the read.
    debt: Stated<UsdcAtoms, u32>,
    /// The `MarginTransferred` of the current transaction, if one has been
    /// seen: its transaction and whether it withdrew or moved nothing.
    transfer_in_tx: Latest<(B256, bool)>,
    /// The margin total as each `MarginTransferred` stated it.
    pub(super) margin_stated: Series<UsdcAtoms>,
    /// The bad debt as each event stated it.
    pub(super) debt_stated: Series<UsdcAtoms>,
    /// USDC that entered as margin since the fold's start: every positive
    /// `MarginTransferred` delta, summed. What bad debt is set against.
    pub(super) deposited: UsdcAtoms,
}

impl Solvency {
    /// Before the first event: no margin, no debt, both stated.
    pub(super) fn genesis() -> Self {
        Self::seeded(Sample::origin(()), SolvencyState::default())
    }

    /// As a read returned the books at one block.
    pub(super) fn seeded(at: Sample<()>, read: SolvencyState) -> Self {
        Self {
            margin: Stated::with(read.total_margin),
            debt: Stated::with(read.bad_debt),
            transfer_in_tx: Latest::default(),
            margin_stated: Series::stated(at.with(read.total_margin)),
            debt_stated: Series::stated(at.with(read.bad_debt)),
            deposited: UsdcAtoms::new(0),
        }
    }

    pub(super) fn retain(&mut self, retention: Retention) {
        self.margin_stated.retain(retention);
        self.debt_stated.retain(retention);
    }

    fn debt_restated(&mut self, event: &TapeEvent, debt: UsdcAtoms) {
        self.debt.state(debt);
        self.debt_stated.push(event.sample(debt));
    }

    /// The books: the debt as last stated, the margin total as last stated
    /// less the swap fees removed since. `None` until both are stated.
    pub(super) fn state(&self) -> Option<SolvencyState> {
        Some(SolvencyState {
            bad_debt: self.debt.value()?,
            total_margin: self
                .margin
                .value()?
                .saturating_sub(self.margin.since.fees_removed),
        })
    }

    /// What may have moved without an event saying so.
    ///
    /// A swap's insurance fee repays debt only while debt stands, so the
    /// swaps counted since the debt was last stated are a silence only when
    /// that statement was nonzero. Deciding that here, from the latest
    /// total, is what lets the count itself combine across segments. A
    /// swap while debt stands also removes less than its gross fees, by
    /// what repaid the debt, so the same swaps are a silence on the margin.
    pub(super) fn silences(&self) -> Silences {
        let debt_stands = self.debt.value().is_some_and(|debt| !debt.is_zero());
        let silent_swaps = if debt_stands { self.debt.since } else { 0 };
        Silences {
            total_margin_unemitted: self.margin.since.unemitted + silent_swaps,
            bad_debt_unemitted: silent_swaps,
        }
    }

    /// Whether this taker event's swap fees left the margin total after the
    /// transaction's `MarginTransferred`, so the fold must remove them, or
    /// before it, so the stated total already has them out.
    fn fees_removed_after_statement(&self, event: &TapeEvent) -> bool {
        if matches!(event.event, MarketEvent::TakerOpened { .. }) {
            return true;
        }
        !matches!(
            self.transfer_in_tx.get(),
            Some((tx, nonpositive)) if tx == event.tx_hash && nonpositive
        )
    }
}

impl Fold for Solvency {
    fn apply(&mut self, event: &TapeEvent) {
        match event.event {
            MarketEvent::TakerOpened { swap, .. }
            | MarketEvent::TakerAdjusted { swap, .. }
            | MarketEvent::TakerClosed { swap, .. } => {
                if !swap.insurance_fee.is_zero() {
                    self.debt.since += 1;
                }
                // The swap's protocol, creator and insurance fees leave the
                // margin total with no event; the LP fee stays, it is the
                // makers'.
                let removed = swap.protocol_fee + swap.creator_fee + swap.insurance_fee;
                if !removed.is_zero() && self.fees_removed_after_statement(event) {
                    self.margin.since.fees_removed += removed;
                }
            }
            MarketEvent::MarginTransferred {
                total_margin,
                margin_delta,
            } => {
                self.margin.state(total_margin);
                self.margin_stated.push(event.sample(total_margin));
                if !margin_delta.is_negative() {
                    self.deposited += margin_delta.magnitude();
                }
                let nonpositive = margin_delta.is_negative() || margin_delta.is_zero();
                self.transfer_in_tx.set((event.tx_hash, nonpositive));
            }
            MarketEvent::BadDebtAccounted { bad_debt_after, .. } => {
                self.debt_restated(event, bad_debt_after);
                // The insurance it consumed was added to the margin total,
                // and nothing says how much.
                self.margin.since.unemitted += 1;
            }
            MarketEvent::LossSocialized { bad_debt_after, .. } => {
                self.debt_restated(event, bad_debt_after);
            }
            MarketEvent::Donated { bad_debt, .. } => {
                self.debt_restated(event, bad_debt);
                // The debt it repaid was added to the margin total, and
                // nothing says how much.
                self.margin.since.unemitted += 1;
            }
            _ => {}
        }
    }

    fn combine(&mut self, later: Self) {
        self.margin.combine(later.margin);
        self.debt.combine(later.debt);
        self.transfer_in_tx.combine(later.transfer_in_tx);
        self.margin_stated.combine(later.margin_stated);
        self.debt_stated.combine(later.debt_stated);
        self.deposited += later.deposited;
    }
}
