//! Positions as the tape describes them, folded beside the market's
//! totals. A taker's size and USD leg are the sums of its swaps' two
//! deltas; a maker's band is the range its first liquidity change named
//! and the sum of the changes since, and its two legs the sums of what each
//! change moved at the pool's exact price, which is what a maker converted
//! to a taker holds. All are sums, so a segment that did not see a
//! position's open knows what moved and not where it stands, and says so.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use alloy::primitives::U256;

use crate::events::{MarketEvent, SwapInfo};
use crate::history::fold::{First, Fold, Latest};
use crate::history::tape::{ChainPoint, OwnershipLog, TapeEvent, Wallets};
use crate::math::liquidity::liquidity_change_delta;
use crate::math::range::{MakerBand, TickRange};
use crate::units::{LDelta, LUnits, PerpDelta, Price, SqrtPrice, UsdcAtoms, UsdcDelta};

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
        /// The USDC its swaps moved since the fold first saw it, the
        /// position's `amount1`: paid for a long, received for a short.
        usd: UsdcDelta,
        /// Whether `moved` and `usd` are the position's two legs, because
        /// the fold saw the position's first swap, or the open of the maker
        /// it was converted from. A position first seen mid-life has a size
        /// no event carries.
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
        /// The perp its liquidity changes moved, the contract's
        /// `delta.amount0`: paid in on an add, taken out on a removal. What
        /// it holds when it converts to a taker. Changes the fold has not
        /// priced are not in it ([`PositionState::unpriced`]).
        moved: PerpDelta,
        /// The USDC they moved, the position's `delta.amount1`.
        usd: UsdcDelta,
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
                Self::Taker { moved, usd, sized },
                Self::Taker {
                    moved: later_moved,
                    usd: later_usd,
                    sized: later_sized,
                },
            ) => Self::Taker {
                moved: moved + later_moved,
                usd: usd + later_usd,
                sized: sized || later_sized,
            },
            (
                Self::Maker {
                    range,
                    liquidity,
                    sized,
                    deposit_pool_price,
                    moved,
                    usd,
                },
                Self::Maker {
                    range: later_range,
                    liquidity: later_liquidity,
                    sized: later_sized,
                    deposit_pool_price: later_deposit,
                    moved: later_moved,
                    usd: later_usd,
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
                moved: moved + later_moved,
                usd: usd + later_usd,
            },
            // The later segment saw the maker convert: the taker holds what
            // the band moved before the cut and everything after it.
            (
                Self::Maker {
                    sized, moved, usd, ..
                },
                Self::Taker {
                    moved: later_moved,
                    usd: later_usd,
                    sized: later_sized,
                },
            ) => Self::Taker {
                moved: moved + later_moved,
                usd: usd + later_usd,
                sized: sized || later_sized,
            },
            // A taker's liquidity changes are no band of its own, but what
            // they moved is.
            (
                Self::Taker { moved, usd, sized },
                Self::Maker {
                    moved: later_moved,
                    usd: later_usd,
                    sized: later_sized,
                    ..
                },
            ) => Self::Taker {
                moved: moved + later_moved,
                usd: usd + later_usd,
                sized: sized || later_sized,
            },
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
    unpriced: u32,
}

impl PositionState {
    /// What the position is and what the tape said about its size.
    pub fn kind(&self) -> PositionKind {
        self.kind
    }

    /// Whether the fold knows where the position stands: a taker's size
    /// or a maker's liquidity, from the open it saw or the read that
    /// seeded it. False for a position first seen mid-life, a converted
    /// maker with a liquidity change [`unpriced`](Self::unpriced), and a
    /// kind the tape never named.
    pub fn level_known(&self) -> bool {
        match self.kind {
            PositionKind::Taker { sized, .. } => sized && self.unpriced == 0,
            PositionKind::Maker { sized, .. } => sized,
            PositionKind::Unknown => false,
        }
    }

    /// Liquidity changes whose amounts the fold does not know, so they are
    /// missing from the maker's `moved` and `usd` and from the taker a
    /// conversion makes of it. A change is priced at the pool's exact price
    /// and tick, which its creation and its swaps state: a fold from genesis
    /// prices every one, a segment prices those before its first swap when
    /// it is combined after the segment before it, and a tape without the
    /// pool's swaps prices none.
    pub fn unpriced(&self) -> u32 {
        self.unpriced
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
    /// saw everything that set it: the swaps since its open, or for a
    /// converted maker every liquidity change since the band's open, priced.
    /// `None` for a maker, a taker first seen mid-life, and a converted
    /// maker with a change [`unpriced`](Self::unpriced).
    pub fn taker_size(&self) -> Option<PerpDelta> {
        match self.kind {
            PositionKind::Taker {
                moved, sized: true, ..
            } if self.unpriced == 0 => Some(moved),
            _ => None,
        }
    }

    /// The taker's USD leg, as the contract's `delta.amount1`: what the
    /// position paid for its perp if long, received for it if short, so
    /// that with [`Self::taker_size`] and a mark the position's PnL is a
    /// multiplication. `None` exactly when the size is.
    pub fn taker_usd(&self) -> Option<UsdcDelta> {
        match self.kind {
            PositionKind::Taker {
                usd, sized: true, ..
            } if self.unpriced == 0 => Some(usd),
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
            unpriced: 0,
        }
    }

    /// A position as a read described it: its level known, its margin
    /// known, no event seen.
    fn seeded(position: SeedPosition) -> Self {
        let (kind, margin) = match position {
            SeedPosition::Taker { size, usd, margin } => (
                PositionKind::Taker {
                    moved: size,
                    usd,
                    sized: true,
                },
                margin,
            ),
            SeedPosition::Maker {
                range,
                liquidity,
                moved,
                usd,
                margin,
            } => (
                PositionKind::Maker {
                    range: Some(range),
                    liquidity,
                    sized: true,
                    deposit_pool_price: None,
                    moved,
                    usd,
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
            unpriced: 0,
        }
    }

    /// A swap moved the position's two legs; `opens` when it was the
    /// position's first. Whatever swaps is a taker from then on, holding
    /// what it moved as a maker too, as the contract's delta does.
    fn swapped(&mut self, swap: SwapInfo, opens: bool) {
        self.kind = match self.kind {
            PositionKind::Taker { moved, usd, sized } => PositionKind::Taker {
                moved: moved + swap.perp_delta,
                usd: usd + swap.usd_delta,
                sized: sized || opens,
            },
            PositionKind::Maker {
                moved, usd, sized, ..
            } => PositionKind::Taker {
                moved: moved + swap.perp_delta,
                usd: usd + swap.usd_delta,
                sized: sized || opens,
            },
            PositionKind::Unknown => PositionKind::Taker {
                moved: swap.perp_delta,
                usd: swap.usd_delta,
                sized: opens,
            },
        };
    }

    /// Whatever has liquidity in a range is a maker; a taker's liquidity
    /// changes are no band of its own. What a change moved is the
    /// position's either way, as everything that moves the contract's
    /// delta is: `moves`, or `None` when the fold could not price it.
    fn liquidity_changed(
        &mut self,
        range: Option<TickRange>,
        delta: LDelta,
        moves: Option<(PerpDelta, UsdcDelta)>,
    ) {
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
                    moved: PerpDelta::ZERO,
                    usd: UsdcDelta::ZERO,
                };
            }
            PositionKind::Taker { .. } => {}
        }
        match moves {
            Some((perp, usd)) => self.moved(perp, usd),
            None => self.unpriced += 1,
        }
    }

    /// Add what a liquidity change moved to the position's legs, as a maker
    /// or as the taker it became.
    fn moved(&mut self, perp: PerpDelta, usd_moved: UsdcDelta) {
        match &mut self.kind {
            PositionKind::Maker { moved, usd, .. } | PositionKind::Taker { moved, usd, .. } => {
                *moved += perp;
                *usd += usd_moved;
            }
            PositionKind::Unknown => {}
        }
    }

    /// An unpriced change, priced once the price it happened at is known.
    fn priced(&mut self, perp: PerpDelta, usd: UsdcDelta) {
        self.moved(perp, usd);
        self.unpriced -= 1;
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
                moved: PerpDelta::ZERO,
                usd: UsdcDelta::ZERO,
            };
        }
        if self.opened.is_set() {
            return;
        }
        self.opened.set(at);
        match &mut self.kind {
            PositionKind::Maker {
                sized,
                deposit_pool_price,
                ..
            } => {
                *sized = true;
                *deposit_pool_price = pool_price;
            }
            // Its legs are the sums since an open, whichever role it took.
            PositionKind::Taker { sized, .. } => *sized = true,
            PositionKind::Unknown => {}
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

    /// The maker became a taker holding what its liquidity changes moved,
    /// which no event states but the pool's events price.
    fn converted(&mut self) {
        self.kind = match self.kind {
            PositionKind::Maker {
                sized, moved, usd, ..
            } => PositionKind::Taker { moved, usd, sized },
            PositionKind::Unknown => PositionKind::Taker {
                moved: PerpDelta::ZERO,
                usd: UsdcDelta::ZERO,
                sized: false,
            },
            taker @ PositionKind::Taker { .. } => taker,
        };
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
        self.unpriced += later.unpriced;
    }
}

/// The pool's exact price and tick, which a liquidity change moves its
/// amounts at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PoolPoint {
    pub(super) sqrt_price: SqrtPrice,
    pub(super) tick: i32,
}

impl PoolPoint {
    /// What `change` moved with the pool here; `None` for a change the
    /// contract could not have made, which stays unpriced.
    fn price(self, change: Change) -> Option<(PerpDelta, UsdcDelta)> {
        liquidity_change_delta(self.sqrt_price, self.tick, &change.range, change.delta).ok()
    }
}

/// A liquidity change a segment saw before its first pool price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Change {
    range: TickRange,
    delta: LDelta,
}

/// Every position the tape has mentioned, by id.
///
/// The fold watches the swaps for the pool price too, since a maker's
/// deposit is classified at the price standing when it opens, and the
/// pool's own creation and swaps for its exact price and tick, which every
/// liquidity change is priced at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Positions {
    by_id: BTreeMap<U256, PositionState>,
    pool_price: Latest<Price>,
    pool: Latest<PoolPoint>,
    /// Changes seen before this segment's first pool price, in chain order
    /// by position: all made at the price the segment before ended on, and
    /// priced there when the two combine.
    waiting: BTreeMap<U256, Vec<Change>>,
}

impl Positions {
    /// Every position a read described at one block, the pool price then,
    /// which a maker opened before the next swap is classified at, and the
    /// pool's exact price and tick, which the next liquidity change is
    /// priced at.
    pub(super) fn seeded(
        pool_price: Price,
        pool: PoolPoint,
        positions: impl IntoIterator<Item = (U256, SeedPosition)>,
    ) -> Self {
        Self {
            by_id: positions
                .into_iter()
                .map(|(pos_id, position)| (pos_id, PositionState::seeded(position)))
                .collect(),
            pool_price: Latest::stated(pool_price),
            pool: Latest::stated(pool),
            waiting: BTreeMap::new(),
        }
    }

    /// The pool's exact price and tick, once its creation or a swap stated
    /// them: what a read at the fold's block would return, for the tests'
    /// seeds.
    #[cfg(test)]
    pub(super) fn pool(&self) -> Option<PoolPoint> {
        self.pool.get()
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

    /// The open positions one of `wallets` holds now, as `custody` records
    /// it, ascending by id: a cohort's or an agent's book. Open by the
    /// fold's own account, so a close whose burn the tape has not yet
    /// carried is already off the book.
    pub fn held_by<'a>(
        &'a self,
        wallets: &'a Wallets,
        custody: &'a OwnershipLog,
    ) -> impl Iterator<Item = (U256, &'a PositionState)> + 'a {
        self.open()
            .filter(move |(pos_id, _)| custody.held_by(*pos_id, wallets))
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
                let pos_id = U256::from_be_bytes(salt.0);
                let range = TickRange::new(tick_lower, tick_upper).ok();
                let change = range.map(|range| Change {
                    range,
                    delta: liquidity_delta,
                });
                let pool = self.pool.get();
                let moves = pool
                    .zip(change)
                    .and_then(|(pool, change)| pool.price(change));
                self.touch(pos_id, at)
                    .liquidity_changed(range, liquidity_delta, moves);
                // Before the segment's first pool price the change waits for
                // the segment before; after it, an unpriced change stays so.
                if pool.is_none()
                    && let Some(change) = change
                {
                    self.waiting.entry(pos_id).or_default().push(change);
                }
            }
            MarketEvent::PoolInitialized {
                sqrt_price, tick, ..
            }
            | MarketEvent::PoolSwapped {
                sqrt_price, tick, ..
            } => self.pool.set(PoolPoint { sqrt_price, tick }),
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
                state.swapped(swap, true);
                state.opened.set(at);
            }
            MarketEvent::TakerAdjusted { pos_id, swap, .. } => {
                self.pool_price.set(swap.pool_price);
                self.touch(pos_id, at).swapped(swap, false);
            }
            MarketEvent::TakerClosed {
                pos_id,
                swap,
                is_liquidation,
                ..
            } => {
                self.pool_price.set(swap.pool_price);
                let state = self.touch(pos_id, at);
                state.swapped(swap, false);
                state.closed.set(at);
                state.liquidations += u32::from(is_liquidation);
            }
            _ => {}
        }
    }

    fn combine(&mut self, mut later: Self) {
        // The changes the later segment saw before its first pool price
        // were made at this segment's last; with none here either, they
        // wait on, now for the segment before this one.
        let pool_at_cut = self.pool.get();
        for (pos_id, changes) in std::mem::take(&mut later.waiting) {
            let Some(pool) = pool_at_cut else {
                self.waiting.entry(pos_id).or_default().extend(changes);
                continue;
            };
            let Some(state) = later.by_id.get_mut(&pos_id) else {
                continue;
            };
            for (perp, usd) in changes.into_iter().filter_map(|change| pool.price(change)) {
                state.priced(perp, usd);
            }
        }
        self.pool.combine(later.pool);
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
