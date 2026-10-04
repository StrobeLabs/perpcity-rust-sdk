//! Positions as the tape describes them, folded beside the market's
//! totals. A taker's size is the sum of its swaps' perp deltas; a maker's
//! band is the range its first liquidity change named and the sum of the
//! changes since. Both are sums, so a segment that did not see a
//! position's open knows what moved and not where it stands, and says so.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use alloy::primitives::U256;

use crate::events::MarketEvent;
use crate::math::range::{MakerBand, TickRange};
use crate::units::{LDelta, LUnits, PerpDelta, Price, UsdcAtoms};

use super::super::fold::{First, Fold, Latest};
use super::super::tape::{ChainPoint, TapeEvent};
use super::seed::SeedPosition;

/// What a position is, and what the tape has said about its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionKind {
    /// Mentioned only by events that say neither what it is nor what moved:
    /// a liquidation or a backstop seen without the action it followed. A
    /// fold from genesis never meets one; a segment can.
    Unknown,
    /// A taker, sized by its swaps.
    Taker {
        /// The perp its swaps moved since the fold first saw it.
        moved: PerpDelta,
        /// Whether `moved` is the size, because the fold saw the position's
        /// first swap. A position first seen mid-life, or a maker converted
        /// to a taker, has a size no live event carries.
        sized: bool,
    },
    /// A maker, a range with liquidity in it.
    Maker {
        /// The range, from the first liquidity change the fold saw or the
        /// read that seeded it.
        range: Option<TickRange>,
        /// Liquidity changed since the fold first saw the position; the
        /// liquidity standing, once `sized`.
        liquidity: LDelta,
        /// Whether `liquidity` is the liquidity standing, because the fold
        /// saw the open or a read supplied the level.
        sized: bool,
        /// The pool price at the open, which its capacity was classified
        /// at. `None` when the fold did not see the open, or no swap had
        /// printed a price before it.
        deposit_pool_price: Option<Price>,
    },
}

impl PositionKind {
    /// The earlier segment's kind is `self`; `later` saw the position after.
    fn combine(self, later: Self, opened_here: bool) -> Self {
        match (self, later) {
            // A segment that learned nothing about what it is leaves the
            // other's answer standing.
            (kind, Self::Unknown) | (Self::Unknown, kind) => kind,
            (
                Self::Taker { moved, sized },
                Self::Taker {
                    moved: later_moved,
                    sized: later_sized,
                },
            ) => Self::Taker {
                moved: moved + later_moved,
                sized: sized || later_sized,
            },
            (
                Self::Maker {
                    range,
                    liquidity,
                    sized,
                    deposit_pool_price,
                },
                Self::Maker {
                    range: later_range,
                    liquidity: later_liquidity,
                    sized: later_sized,
                    deposit_pool_price: later_deposit,
                },
            ) => Self::Maker {
                range: range.or(later_range),
                liquidity: LDelta::new(liquidity.units().wrapping_add(later_liquidity.units())),
                sized: sized || later_sized,
                // The deposit price is the open's: whichever segment saw
                // the open knows it, and the other's is a later open the
                // fold ignored.
                deposit_pool_price: if opened_here {
                    deposit_pool_price
                } else {
                    later_deposit
                },
            },
            // The later segment saw the maker swap or convert, and knows
            // only what moved since.
            (Self::Maker { .. }, taker @ Self::Taker { .. }) => taker,
            // A taker's liquidity changes are not its own; the fold ignores
            // them, so the merge does too.
            (taker @ Self::Taker { .. }, Self::Maker { .. }) => taker,
        }
    }
}

/// One position as the fold knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionState {
    kind: PositionKind,
    opened: First<ChainPoint>,
    last: Latest<ChainPoint>,
    closed: First<ChainPoint>,
    liquidations: u32,
    /// The margin a read supplied; gone once any event touches the
    /// position, since no live event carries it.
    margin: Option<UsdcAtoms>,
}

impl PositionState {
    /// What the position is and what the tape said about its size.
    pub fn kind(&self) -> PositionKind {
        self.kind
    }

    /// Whether the fold knows where the position stands: a taker's size
    /// or a maker's liquidity, from the open it saw or the read that
    /// seeded it. False for a position first seen mid-life, a maker
    /// converted to a taker, and a kind the tape never named.
    pub fn level_known(&self) -> bool {
        match self.kind {
            PositionKind::Taker { sized, .. } | PositionKind::Maker { sized, .. } => sized,
            PositionKind::Unknown => false,
        }
    }

    /// The event that opened it, if the fold saw it; `None` for a position
    /// first seen mid-life or supplied by a read.
    pub fn opened(&self) -> Option<ChainPoint> {
        self.opened.get()
    }

    /// The last event that touched it; `None` for a position a read
    /// supplied that no event has touched since.
    pub fn last(&self) -> Option<ChainPoint> {
        self.last.get()
    }

    /// The position's margin, when a read supplied it and no event has
    /// touched the position since. No live event carries margin, so the
    /// fold never knows it from the tape alone.
    pub fn margin(&self) -> Option<UsdcAtoms> {
        self.margin
    }

    /// The event that closed it, once one has.
    pub fn closed(&self) -> Option<ChainPoint> {
        self.closed.get()
    }

    /// Whether no close has been seen.
    pub fn is_open(&self) -> bool {
        !self.closed.is_set()
    }

    /// Liquidations the fold saw land on it, whole or partial.
    pub fn liquidations(&self) -> u32 {
        self.liquidations
    }

    /// The taker's size, as the contract's `delta.amount0`, when the fold
    /// saw the swap that set it: `None` for a maker, a taker first seen
    /// mid-life, or a maker converted to a taker.
    pub fn taker_size(&self) -> Option<PerpDelta> {
        match self.kind {
            PositionKind::Taker { moved, sized: true } => Some(moved),
            _ => None,
        }
    }

    /// The maker's band, as `makerDetails` holds it, when the fold knows it
    /// whole: the range from a liquidity change or a read, the liquidity
    /// from the open or the read onward. `None` for a taker, for a maker
    /// whose level the fold does not know, and for one whose liquidity is
    /// gone.
    pub fn maker_band(&self) -> Option<MakerBand> {
        let PositionKind::Maker {
            range: Some(range),
            liquidity,
            sized: true,
            ..
        } = self.kind
        else {
            return None;
        };
        let units = u128::try_from(liquidity.units()).ok()?;
        (units != 0).then(|| MakerBand::new(range, LUnits::new(units)))
    }

    /// The pool price the band was deposited at, when the fold saw the open
    /// and a swap had printed a price before it.
    pub fn deposit_pool_price(&self) -> Option<Price> {
        match self.kind {
            PositionKind::Maker {
                deposit_pool_price, ..
            } => deposit_pool_price,
            PositionKind::Taker { .. } | PositionKind::Unknown => None,
        }
    }

    fn first(at: ChainPoint) -> Self {
        Self {
            kind: PositionKind::Unknown,
            opened: First::default(),
            last: Latest::stated(at),
            closed: First::default(),
            liquidations: 0,
            margin: None,
        }
    }

    /// A position as a read described it: its level known, its margin
    /// known, no event seen.
    fn seeded(position: SeedPosition) -> Self {
        let (kind, margin) = match position {
            SeedPosition::Taker { size, margin } => (
                PositionKind::Taker {
                    moved: size,
                    sized: true,
                },
                margin,
            ),
            SeedPosition::Maker {
                range,
                liquidity,
                margin,
            } => (
                PositionKind::Maker {
                    range: Some(range),
                    liquidity,
                    sized: true,
                    deposit_pool_price: None,
                },
                margin,
            ),
            SeedPosition::Unknown { margin } => (PositionKind::Unknown, margin),
        };
        Self {
            kind,
            opened: First::default(),
            last: Latest::default(),
            closed: First::default(),
            liquidations: 0,
            margin: Some(margin),
        }
    }

    /// A swap moved the position's perp by `delta`; `opens` when it was the
    /// position's first. Whatever swaps is a taker from then on.
    fn swapped(&mut self, delta: PerpDelta, opens: bool) {
        self.kind = match self.kind {
            PositionKind::Taker { moved, sized } => PositionKind::Taker {
                moved: moved + delta,
                sized: sized || opens,
            },
            PositionKind::Maker { .. } | PositionKind::Unknown => PositionKind::Taker {
                moved: delta,
                sized: opens,
            },
        };
    }

    /// Whatever has liquidity in a range is a maker; a taker's liquidity
    /// changes are not its own and are ignored.
    fn liquidity_changed(&mut self, range: Option<TickRange>, delta: LDelta) {
        match &mut self.kind {
            PositionKind::Maker {
                range: known,
                liquidity,
                ..
            } => {
                *known = known.or(range);
                *liquidity = LDelta::new(liquidity.units().wrapping_add(delta.units()));
            }
            PositionKind::Unknown => {
                self.kind = PositionKind::Maker {
                    range,
                    liquidity: delta,
                    sized: false,
                    deposit_pool_price: None,
                };
            }
            PositionKind::Taker { .. } => {}
        }
    }

    /// A maker's open, if none was seen yet: the liquidity so far is the
    /// level, and the deposit is classified at the pool price then.
    fn maker_opened(&mut self, at: ChainPoint, pool_price: Option<Price>) {
        if let PositionKind::Unknown = self.kind {
            self.kind = PositionKind::Maker {
                range: None,
                liquidity: LDelta::ZERO,
                sized: false,
                deposit_pool_price: None,
            };
        }
        if self.opened.is_set() {
            return;
        }
        self.opened.set(at);
        if let PositionKind::Maker {
            sized,
            deposit_pool_price,
            ..
        } = &mut self.kind
        {
            *sized = true;
            *deposit_pool_price = pool_price;
        }
    }

    /// A segment that saw the open before any swap of its own did not know
    /// the price then; the segment before it did, as its last price.
    fn deposit_priced_at(&mut self, price_at_cut: Option<Price>) {
        if !self.opened.is_set() {
            return;
        }
        if let PositionKind::Maker {
            deposit_pool_price: deposit @ None,
            ..
        } = &mut self.kind
        {
            *deposit = price_at_cut;
        }
    }

    /// The maker became a taker with the inventory its band left it, which
    /// no live event carries.
    fn converted(&mut self) {
        if let PositionKind::Maker { .. } | PositionKind::Unknown = self.kind {
            self.kind = PositionKind::Taker {
                moved: PerpDelta::ZERO,
                sized: false,
            };
        }
    }

    /// The earlier segment is `self`; `later` saw the same position after.
    /// The later segment touched it, so whatever margin a read had
    /// supplied is gone.
    fn combine(&mut self, later: Self) {
        self.kind = self.kind.combine(later.kind, self.opened.is_set());
        self.opened.combine(later.opened);
        self.closed.combine(later.closed);
        self.last.combine(later.last);
        self.liquidations += later.liquidations;
        self.margin = later.margin;
    }
}

/// Every position the tape has mentioned, by id.
///
/// The fold watches the swaps for the pool price too, since a maker's
/// deposit is classified at the price standing when it opens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Positions {
    by_id: BTreeMap<U256, PositionState>,
    pool_price: Latest<Price>,
}

impl Positions {
    /// Every position a read described at one block, and the pool price
    /// then, which a maker opened before the next swap is classified at.
    pub(super) fn seeded(
        pool_price: Price,
        positions: impl IntoIterator<Item = (U256, SeedPosition)>,
    ) -> Self {
        Self {
            by_id: positions
                .into_iter()
                .map(|(pos_id, position)| (pos_id, PositionState::seeded(position)))
                .collect(),
            pool_price: Latest::stated(pool_price),
        }
    }

    /// One position, if the tape has mentioned it.
    pub fn get(&self, pos_id: U256) -> Option<&PositionState> {
        self.by_id.get(&pos_id)
    }

    /// Every position mentioned, ascending by id.
    pub fn iter(&self) -> impl Iterator<Item = (U256, &PositionState)> {
        self.by_id.iter().map(|(id, state)| (*id, state))
    }

    /// The positions no close has been seen for, ascending by id.
    pub fn open(&self) -> impl Iterator<Item = (U256, &PositionState)> {
        self.iter().filter(|(_, state)| state.is_open())
    }

    /// Positions mentioned.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the tape mentioned no position.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// The position's state, of unknown kind if this is the fold's first
    /// sight of it, touched at `at`. Any touch moves the margin, which no
    /// event carries, so a margin a read supplied is forgotten here.
    fn touch(&mut self, pos_id: U256, at: ChainPoint) -> &mut PositionState {
        let state = self
            .by_id
            .entry(pos_id)
            .or_insert_with(|| PositionState::first(at));
        state.last.set(at);
        state.margin = None;
        state
    }
}

impl Fold for Positions {
    /// A position's first sight is [`PositionKind::Unknown`] until an event
    /// says what it is, so a segment records what it learned and never what
    /// it assumed.
    fn apply(&mut self, event: &TapeEvent) {
        let at = event.point();
        match event.event {
            MarketEvent::ModifyLiquidity {
                tick_lower,
                tick_upper,
                liquidity_delta,
                salt,
                ..
            } => {
                let range = TickRange::new(tick_lower, tick_upper).ok();
                self.touch(U256::from_be_bytes(salt.0), at)
                    .liquidity_changed(range, liquidity_delta);
            }
            MarketEvent::MakerOpened { pos_id } => {
                let pool_price = self.pool_price.get();
                self.touch(pos_id, at).maker_opened(at, pool_price);
            }
            MarketEvent::MakerAdjusted { pos_id, .. }
            | MarketEvent::MakerBackstopped { pos_id, .. }
            | MarketEvent::TakerBackstopped { pos_id, .. } => {
                self.touch(pos_id, at);
            }
            MarketEvent::MakerConverted {
                pos_id,
                is_liquidation,
                ..
            } => {
                let state = self.touch(pos_id, at);
                state.converted();
                state.liquidations += u32::from(is_liquidation);
            }
            MarketEvent::MakerClosed {
                pos_id,
                is_liquidation,
                ..
            } => {
                let state = self.touch(pos_id, at);
                state.closed.set(at);
                state.liquidations += u32::from(is_liquidation);
            }
            MarketEvent::MakerLiquidated { pos_id, .. }
            | MarketEvent::TakerLiquidated { pos_id, .. } => {
                self.touch(pos_id, at).liquidations += 1;
            }
            MarketEvent::TakerOpened { pos_id, swap } => {
                self.pool_price.set(swap.pool_price);
                let state = self.touch(pos_id, at);
                state.swapped(swap.perp_delta, true);
                state.opened.set(at);
            }
            MarketEvent::TakerAdjusted { pos_id, swap, .. } => {
                self.pool_price.set(swap.pool_price);
                self.touch(pos_id, at).swapped(swap.perp_delta, false);
            }
            MarketEvent::TakerClosed {
                pos_id,
                swap,
                is_liquidation,
                ..
            } => {
                self.pool_price.set(swap.pool_price);
                let state = self.touch(pos_id, at);
                state.swapped(swap.perp_delta, false);
                state.closed.set(at);
                state.liquidations += u32::from(is_liquidation);
            }
            _ => {}
        }
    }

    fn combine(&mut self, later: Self) {
        // An open the later segment saw before any swap of its own was
        // classified at this segment's last price.
        let price_at_cut = self.pool_price.get();
        for (pos_id, mut state) in later.by_id {
            state.deposit_priced_at(price_at_cut);
            match self.by_id.entry(pos_id) {
                Entry::Vacant(slot) => {
                    slot.insert(state);
                }
                Entry::Occupied(mut slot) => slot.get_mut().combine(state),
            }
        }
        self.pool_price.combine(later.pool_price);
    }
}
