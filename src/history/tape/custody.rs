//! Custody: who held each position, folded from the tape's transfers.

use std::collections::BTreeMap;

use alloy::primitives::{Address, U256};

use crate::events::MarketEvent;
use crate::history::fold::Fold;

use super::{ChainPoint, TapeEvent};

/// Who held each of a market's positions, over time: the custody
/// timeline folded from a tape's [`MarketEvent::PositionTransferred`]
/// events.
///
/// A mint is a transfer from the zero address and a burn (a full close,
/// or a liquidation) is a transfer to it, so custody is `None` before a
/// position's mint and after its burn.
///
/// ```
/// use perpcity_sdk::history::OwnershipLog;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let tape: Vec<perpcity_sdk::history::TapeEvent> = Vec::new();
/// let custody = OwnershipLog::fold(&tape);
/// for pos_id in custody.positions() {
///     let _owner = custody.latest_owner(pos_id);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OwnershipLog {
    /// Position id → `(from this point on, this address holds it)`, in
    /// chain order. The zero address marks a burn.
    spans: BTreeMap<U256, Vec<(ChainPoint, Address)>>,
}

impl Fold for OwnershipLog {
    fn apply(&mut self, event: &TapeEvent) {
        if let MarketEvent::PositionTransferred { to, pos_id, .. } = event.event {
            let timeline = self.spans.entry(pos_id).or_default();
            let point = event.point();
            debug_assert!(
                timeline.last().is_none_or(|&(last, _)| last < point),
                "tape out of chain order at {point:?}"
            );
            timeline.push((point, to));
        }
    }

    fn combine(&mut self, later: Self) {
        for (pos_id, timeline) in later.spans {
            self.spans.entry(pos_id).or_default().extend(timeline);
        }
    }
}

impl OwnershipLog {
    /// Fold the custody timeline out of a tape's transfer events; other
    /// events are skipped.
    ///
    /// `events` must be in chain order, as every reader in this module
    /// returns them. The same as [`Fold::fold`], kept inherent so the fold
    /// is reachable without importing the trait.
    pub fn fold<'a>(events: impl IntoIterator<Item = &'a TapeEvent>) -> Self {
        <Self as Fold>::fold(events)
    }

    /// Who held `pos_id` at `at`: the recipient of its latest transfer at
    /// or before that point. `None` before the mint, after the burn, or
    /// for a position this tape never saw minted.
    ///
    /// This is the attribution a measurement over past events wants: a
    /// position handed between wallets mid-life has more than one owner,
    /// and only a point-in-time read credits each event to the wallet
    /// that held it then.
    pub fn owner_at(&self, pos_id: U256, at: ChainPoint) -> Option<Address> {
        let timeline = self.spans.get(&pos_id)?;
        let held = timeline.partition_point(|&(point, _)| point <= at);
        let (_, owner) = timeline[..held].last()?;
        (!owner.is_zero()).then_some(*owner)
    }

    /// The last wallet that held `pos_id`, ignoring its burn — the
    /// position's final owner, for a caller that wants one address per
    /// position rather than a timeline. `None` for a position this tape
    /// never saw minted.
    pub fn latest_owner(&self, pos_id: U256) -> Option<Address> {
        self.spans
            .get(&pos_id)?
            .iter()
            .rev()
            .map(|&(_, owner)| owner)
            .find(|owner| !owner.is_zero())
    }

    /// Every transfer of `pos_id`, oldest first, as
    /// `(point, new owner)`; the zero address ends the position.
    pub fn transfers(&self, pos_id: U256) -> impl Iterator<Item = (ChainPoint, Address)> + '_ {
        self.spans.get(&pos_id).into_iter().flatten().copied()
    }

    /// Every position id with custody history, ascending.
    pub fn positions(&self) -> impl Iterator<Item = U256> + '_ {
        self.spans.keys().copied()
    }

    /// Positions with custody history.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Whether the tape held no position transfers at all.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}
