//! Custody: who held each position, folded from the tape's transfers; and
//! the wallet set a question is scoped to, which custody answers for.

use std::collections::{BTreeMap, BTreeSet};

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

use crate::events::MarketEvent;
use crate::history::fold::Fold;

use super::{ChainPoint, TapeEvent};

/// The wallets a question is scoped to: one agent's, a cohort's, or
/// anyone's. A market series filtered by a set is a cohort series; by one
/// wallet, an agent's. The set is addresses alone; how it was drawn, from
/// a file or a walk of the master's transfers, is the caller's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wallets(BTreeSet<Address>);

impl Wallets {
    /// One wallet: an agent's own scope.
    pub fn one(wallet: Address) -> Self {
        Self(BTreeSet::from([wallet]))
    }

    /// Whether `wallet` is in the set.
    pub fn contains(&self, wallet: Address) -> bool {
        self.0.contains(&wallet)
    }

    /// Add `wallet`; whether it was new.
    pub fn insert(&mut self, wallet: Address) -> bool {
        self.0.insert(wallet)
    }

    /// The wallets, ascending.
    pub fn iter(&self) -> impl Iterator<Item = Address> + '_ {
        self.0.iter().copied()
    }

    /// Wallets in the set.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set names no wallet.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<Address> for Wallets {
    fn from_iter<I: IntoIterator<Item = Address>>(wallets: I) -> Self {
        Self(wallets.into_iter().collect())
    }
}

impl Extend<Address> for Wallets {
    fn extend<I: IntoIterator<Item = Address>>(&mut self, wallets: I) {
        self.0.extend(wallets);
    }
}

impl<'a> IntoIterator for &'a Wallets {
    type Item = &'a Address;
    type IntoIter = std::collections::btree_set::Iter<'a, Address>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// A mark about one position, which custody can attribute to a holder:
/// what the custody filter on arrivals is written against.
pub trait Positioned {
    /// The position the mark is about.
    fn pos_id(&self) -> U256;
}

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

    /// Whether one of `wallets` held `pos_id` at `at`: the attribution of
    /// an event to a scope. A position's mint lands before its opening
    /// trade in the same transaction, so an open is its minter's.
    pub fn held_by_at(&self, pos_id: U256, wallets: &Wallets, at: ChainPoint) -> bool {
        self.owner_at(pos_id, at)
            .is_some_and(|holder| wallets.contains(holder))
    }

    /// Who holds `pos_id` now: the recipient of its last transfer, `None`
    /// once it is burned or if this tape never saw it minted. Unlike
    /// [`latest_owner`](Self::latest_owner), a burn ends the holding.
    pub fn holder(&self, pos_id: U256) -> Option<Address> {
        let (_, holder) = self.spans.get(&pos_id)?.last()?;
        (!holder.is_zero()).then_some(*holder)
    }

    /// Whether one of `wallets` holds `pos_id` now; false once it is burned.
    pub fn held_by(&self, pos_id: U256, wallets: &Wallets) -> bool {
        self.holder(pos_id)
            .is_some_and(|holder| wallets.contains(holder))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wallet_set_is_a_set() {
        let a = Address::repeat_byte(0x0A);
        let b = Address::repeat_byte(0x0B);
        let mut ours = Wallets::one(a);
        assert!(ours.contains(a));
        assert!(!ours.contains(b));
        assert!(ours.insert(b));
        assert!(!ours.insert(b), "already in");
        assert_eq!(ours.len(), 2);
        assert_eq!(ours.iter().collect::<Vec<_>>(), vec![a, b], "ascending");
        let same: Wallets = [b, a, a].into_iter().collect();
        assert_eq!(same, ours);
        assert!(Wallets::default().is_empty());
    }

    /// A burn ends a holding: the position's final owner is still named,
    /// but nobody holds it now.
    #[test]
    fn a_burned_position_has_a_final_owner_and_no_holder() {
        let a = Address::repeat_byte(0x0A);
        let pos = U256::from(7);
        let transfer = |block, from, to| TapeEvent {
            block_number: block,
            block_hash: Default::default(),
            log_index: 0,
            timestamp: block,
            tx_hash: Default::default(),
            event: MarketEvent::PositionTransferred {
                from,
                to,
                pos_id: pos,
            },
        };
        let minted = OwnershipLog::fold(&[transfer(1, Address::ZERO, a)]);
        assert_eq!(minted.holder(pos), Some(a));
        assert!(minted.held_by(pos, &Wallets::one(a)));

        let burned =
            OwnershipLog::fold(&[transfer(1, Address::ZERO, a), transfer(2, a, Address::ZERO)]);
        assert_eq!(
            burned.latest_owner(pos),
            Some(a),
            "the final owner, for attribution"
        );
        assert_eq!(burned.holder(pos), None, "held by nobody now");
        assert!(!burned.held_by(pos, &Wallets::one(a)));
        assert_eq!(burned.holder(U256::from(8)), None, "never minted");
    }
}
