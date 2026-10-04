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
use crate::units::{LDelta, LUnits, PerpDelta, Price};

use super::tape::{ChainPoint, TapeEvent};

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
        /// The range, from the first liquidity change the fold saw.
        range: Option<TickRange>,
        /// Liquidity changed since the fold first saw the position; the
        /// liquidity standing, once the open was seen.
        liquidity: LDelta,
        /// The pool price at the open, which its capacity was classified
        /// at. `None` when the fold did not see the open, or no swap had
        /// printed a price before it.
        deposit_pool_price: Option<Price>,
    },
}

/// One position as the fold knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionState {
    kind: PositionKind,
    opened: Option<ChainPoint>,
    last: ChainPoint,
    closed: Option<ChainPoint>,
    liquidations: u32,
}

impl PositionState {
    /// What the position is and what the tape said about its size.
    pub fn kind(&self) -> PositionKind {
        self.kind
    }

    /// The event that opened it, if the fold saw it; `None` for a position
    /// first seen mid-life.
    pub fn opened(&self) -> Option<ChainPoint> {
        self.opened
    }

    /// The last event that touched it.
    pub fn last(&self) -> ChainPoint {
        self.last
    }

    /// The event that closed it, once one has.
    pub fn closed(&self) -> Option<ChainPoint> {
        self.closed
    }

    /// Whether no close has been seen.
    pub fn is_open(&self) -> bool {
        self.closed.is_none()
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
    /// whole: the range from a liquidity change and the liquidity from the
    /// open onward. `None` for a taker, for a maker whose open the fold did
    /// not see, and for one whose liquidity is gone.
    pub fn maker_band(&self) -> Option<MakerBand> {
        let PositionKind::Maker {
            range: Some(range),
            liquidity,
            ..
        } = self.kind
        else {
            return None;
        };
        self.opened?;
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
            opened: None,
            last: at,
            closed: None,
            liquidations: 0,
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
                    deposit_pool_price: None,
                };
            }
            PositionKind::Taker { .. } => {}
        }
    }

    /// A maker's open, if none was seen yet: the deposit is classified at
    /// the pool price then.
    fn maker_opened(&mut self, at: ChainPoint, pool_price: Option<Price>) {
        if let PositionKind::Unknown = self.kind {
            self.kind = PositionKind::Maker {
                range: None,
                liquidity: LDelta::ZERO,
                deposit_pool_price: None,
            };
        }
        if self.opened.is_some() {
            return;
        }
        self.opened = Some(at);
        if let PositionKind::Maker {
            deposit_pool_price, ..
        } = &mut self.kind
        {
            *deposit_pool_price = pool_price;
        }
    }

    /// A segment that saw the open before any swap of its own did not know
    /// the price then; the segment before it did, as its last price.
    fn deposit_priced_at(&mut self, price_at_cut: Option<Price>) {
        if self.opened.is_none() {
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
    /// Every field is a sum, a first occurrence or a latest, so the merge
    /// equals the fold of the two segments' events in order.
    fn combine(&mut self, later: Self) {
        // The deposit price is the open's: whichever segment saw the open
        // knows it, and the other's is a later open the fold ignored.
        let opened_here = self.opened.is_some();
        self.last = later.last;
        self.opened = self.opened.or(later.opened);
        self.closed = self.closed.or(later.closed);
        self.liquidations += later.liquidations;
        self.kind = match (self.kind, later.kind) {
            // A segment that learned nothing about what it is leaves the
            // other's answer standing.
            (kind, PositionKind::Unknown) | (PositionKind::Unknown, kind) => kind,
            (
                PositionKind::Taker { moved, sized },
                PositionKind::Taker {
                    moved: later_moved,
                    sized: later_sized,
                },
            ) => PositionKind::Taker {
                moved: moved + later_moved,
                sized: sized || later_sized,
            },
            (
                PositionKind::Maker {
                    range,
                    liquidity,
                    deposit_pool_price,
                },
                PositionKind::Maker {
                    range: later_range,
                    liquidity: later_liquidity,
                    deposit_pool_price: later_deposit,
                },
            ) => PositionKind::Maker {
                range: range.or(later_range),
                liquidity: LDelta::new(liquidity.units().wrapping_add(later_liquidity.units())),
                deposit_pool_price: if opened_here {
                    deposit_pool_price
                } else {
                    later_deposit
                },
            },
            // The later segment saw the maker swap or convert, and knows
            // only what moved since.
            (PositionKind::Maker { .. }, taker @ PositionKind::Taker { .. }) => taker,
            // A taker's liquidity changes are not its own; the fold ignores
            // them, so the merge does too.
            (taker @ PositionKind::Taker { .. }, PositionKind::Maker { .. }) => taker,
        };
    }
}

/// Every position the tape has mentioned, by id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Positions {
    by_id: BTreeMap<U256, PositionState>,
}

impl Positions {
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

    /// Apply one event; `pool_price` is the fold's price at that event,
    /// which a deposit is classified at. A position's first sight is
    /// [`PositionKind::Unknown`] until an event says what it is, so a
    /// segment records what it learned and never what it assumed.
    pub(super) fn apply(&mut self, event: &TapeEvent, pool_price: Option<Price>) {
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
                let state = self.touch(U256::from_be_bytes(salt.0), at);
                state.liquidity_changed(range, liquidity_delta);
            }
            MarketEvent::MakerOpened { pos_id } => {
                let state = self.touch(pos_id, at);
                state.maker_opened(at, pool_price);
            }
            MarketEvent::MakerAdjusted { pos_id, .. }
            | MarketEvent::MakerBackstopped { pos_id, .. } => {
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
                state.closed.get_or_insert(at);
                state.liquidations += u32::from(is_liquidation);
            }
            MarketEvent::MakerLiquidated { pos_id, .. } => {
                let state = self.touch(pos_id, at);
                state.liquidations += 1;
            }
            MarketEvent::TakerLiquidated { pos_id, .. } => {
                let state = self.touch(pos_id, at);
                state.liquidations += 1;
            }
            MarketEvent::TakerOpened { pos_id, swap } => {
                let state = self.touch(pos_id, at);
                state.swapped(swap.perp_delta, true);
                state.opened.get_or_insert(at);
            }
            MarketEvent::TakerAdjusted { pos_id, swap, .. } => {
                let state = self.touch(pos_id, at);
                state.swapped(swap.perp_delta, false);
            }
            MarketEvent::TakerClosed {
                pos_id,
                swap,
                is_liquidation,
                ..
            } => {
                let state = self.touch(pos_id, at);
                state.swapped(swap.perp_delta, false);
                state.closed.get_or_insert(at);
                state.liquidations += u32::from(is_liquidation);
            }
            MarketEvent::TakerBackstopped { pos_id, .. } => {
                self.touch(pos_id, at);
            }
            _ => {}
        }
    }

    /// Merge the segment that follows; `price_at_cut` is the earlier
    /// segment's last pool price, which an open in the later segment before
    /// any swap of its own was classified at.
    pub(super) fn combine(&mut self, later: Self, price_at_cut: Option<Price>) {
        for (pos_id, mut state) in later.by_id {
            state.deposit_priced_at(price_at_cut);
            match self.by_id.entry(pos_id) {
                Entry::Vacant(slot) => {
                    slot.insert(state);
                }
                Entry::Occupied(mut slot) => {
                    slot.get_mut().combine(state);
                }
            }
        }
    }

    /// The position's state, of unknown kind if this is the fold's first
    /// sight of it, touched at `at`.
    fn touch(&mut self, pos_id: U256, at: ChainPoint) -> &mut PositionState {
        let state = self
            .by_id
            .entry(pos_id)
            .or_insert_with(|| PositionState::first(at));
        state.last = at;
        state
    }
}
