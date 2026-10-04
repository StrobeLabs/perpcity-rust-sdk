//! The pool's liquidity from the tape: how much stands at each initialized
//! tick, where the tick is, and so how much is active.
//!
//! The tick map is the PoolManager's own bookkeeping replayed: a liquidity
//! change adds to the lower tick's net and gross and to the upper tick's
//! gross while subtracting from its net. The sums are signed so a segment
//! can hold removals it never saw the adds of, and only a fold from the
//! pool's genesis knows the map whole. The tick comes from `TicksCrossed`,
//! which the perp emits with the pool's own tick after every swap that moved
//! it; the root is not rebuilt, since the emitted pool price is its floored
//! square.

use std::collections::BTreeMap;

use crate::events::MarketEvent;
use crate::math::swap::TickLiquidity;
use crate::units::{LDelta, LUnits};

use super::super::fold::{Fold, Latest};
use super::super::tape::TapeEvent;

/// Signed sums at one tick; the pool's `liquidityNet` and `liquidityGross`
/// once every change since genesis is in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TickSums {
    net: i128,
    gross: i128,
}

impl TickSums {
    fn add(&mut self, net: i128, gross: i128) {
        self.net = self.net.wrapping_add(net);
        self.gross = self.gross.wrapping_add(gross);
    }

    fn is_zero(self) -> bool {
        self.net == 0 && self.gross == 0
    }
}

/// The pool's liquidity by tick, and its tick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Pool {
    ticks: BTreeMap<i32, TickSums>,
    tick: Latest<i32>,
    /// Whether every change since the pool's first is in, which only a fold
    /// from genesis can say.
    from_genesis: bool,
}

impl Pool {
    /// Before the first event: no liquidity at any tick, and every change
    /// to come will be seen.
    pub(super) fn genesis() -> Self {
        Self {
            from_genesis: true,
            ..Self::default()
        }
    }

    /// The pool's tick, where the last swap that moved it left it. `None`
    /// until a swap has, since the first tick is the factory's to say.
    pub(super) fn tick(&self) -> Option<i32> {
        self.tick.get()
    }

    /// Liquidity at every initialized tick, as the pool read returns it.
    /// `None` for a fold that did not start at genesis: a segment knows what
    /// changed, not what stands.
    pub(super) fn ticks(&self) -> Option<BTreeMap<i32, TickLiquidity>> {
        if !self.from_genesis {
            return None;
        }
        self.ticks
            .iter()
            .map(|(&tick, sums)| {
                let gross = u128::try_from(sums.gross).ok()?;
                Some((
                    tick,
                    TickLiquidity {
                        gross: LUnits::new(gross),
                        net: LDelta::new(sums.net),
                    },
                ))
            })
            .collect()
    }

    /// Liquidity active at the pool's tick: the net of every initialized
    /// tick at or below it. `None` until the tick is known, or off genesis.
    pub(super) fn liquidity(&self) -> Option<LUnits> {
        if !self.from_genesis {
            return None;
        }
        let tick = self.tick()?;
        let active = self
            .ticks
            .range(..=tick)
            .fold(0i128, |active, (_, sums)| active.wrapping_add(sums.net));
        u128::try_from(active).ok().map(LUnits::new)
    }

    /// Add to a tick's sums, dropping the tick when both return to zero, as
    /// the pool drops a tick no liquidity references.
    fn changed(&mut self, tick: i32, net: i128, gross: i128) {
        let sums = self.ticks.entry(tick).or_default();
        sums.add(net, gross);
        if sums.is_zero() {
            self.ticks.remove(&tick);
        }
    }
}

impl Fold for Pool {
    fn apply(&mut self, event: &TapeEvent) {
        match event.event {
            MarketEvent::ModifyLiquidity {
                tick_lower,
                tick_upper,
                liquidity_delta,
                ..
            } => {
                let delta = liquidity_delta.units();
                self.changed(tick_lower, delta, delta);
                self.changed(tick_upper, delta.wrapping_neg(), delta);
            }
            MarketEvent::TicksCrossed { ending_tick, .. } => self.tick.set(ending_tick),
            _ => {}
        }
    }

    fn combine(&mut self, later: Self) {
        for (tick, sums) in later.ticks {
            self.changed(tick, sums.net, sums.gross);
        }
        self.tick.combine(later.tick);
    }
}
