//! Position tracking with automated trigger evaluation.
//!
//! The [`PositionManager`] tracks open positions and evaluates stop-loss,
//! take-profit, and trailing-stop triggers against the current mark price.
//!
//! # Trigger precedence
//!
//! When multiple triggers fire simultaneously:
//! 1. **Stop-loss** (highest priority — capital preservation)
//! 2. **Take-profit**
//! 3. **Trailing stop**
//!
//! Only one trigger is returned per position per `check_triggers` call.
//!
//! # Example
//!
//! ```
//! use perpcity_rust_sdk::hft::position_manager::{ManagedPosition, PositionManager};
//! use std::collections::HashMap;
//!
//! let mut mgr = PositionManager::new();
//! mgr.track(ManagedPosition {
//!     perp_id: [0xAA; 32],
//!     position_id: 1,
//!     is_long: true,
//!     entry_price: 100.0,
//!     margin: 10.0,
//!     stop_loss: Some(90.0),
//!     take_profit: Some(120.0),
//!     trailing_stop_pct: None,
//!     trailing_stop_anchor: None,
//! });
//!
//! let mut prices = HashMap::new();
//! prices.insert([0xAA; 32], 85.0);
//! let triggers = mgr.check_triggers(&prices);
//! assert_eq!(triggers.len(), 1);
//! ```

use std::collections::HashMap;

/// What kind of trigger fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerType {
    /// Price hit the stop-loss level.
    StopLoss,
    /// Price hit the take-profit level.
    TakeProfit,
    /// Price pulled back beyond the trailing stop threshold.
    TrailingStop,
}

/// A triggered action for a position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TriggerAction {
    /// The type of trigger that fired.
    pub trigger_type: TriggerType,
    /// The position's unique ID.
    pub position_id: u64,
    /// The perp market identifier.
    pub perp_id: [u8; 32],
    /// The price threshold that was breached.
    pub trigger_price: f64,
}

/// A position being tracked for automated trigger evaluation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ManagedPosition {
    /// Perp market identifier.
    pub perp_id: [u8; 32],
    /// Position NFT ID.
    pub position_id: u64,
    /// `true` = long, `false` = short.
    pub is_long: bool,
    /// Entry price in USDC.
    pub entry_price: f64,
    /// Margin deposited in USDC.
    pub margin: f64,

    /// Stop-loss price. `None` = disabled.
    pub stop_loss: Option<f64>,
    /// Take-profit price. `None` = disabled.
    pub take_profit: Option<f64>,
    /// Trailing stop percentage (e.g. 0.02 = 2%). `None` = disabled.
    pub trailing_stop_pct: Option<f64>,
    /// High-water mark (longs) or low-water mark (shorts) for trailing stop.
    ///
    /// Updated automatically by [`check_triggers`](PositionManager::check_triggers).
    /// Set to `None` initially; the first price observation will seed it.
    pub trailing_stop_anchor: Option<f64>,
}

impl ManagedPosition {
    /// Update the trailing stop anchor (high/low water mark) for a new price.
    ///
    /// - Longs: tracks the **highest** price seen.
    /// - Shorts: tracks the **lowest** price seen.
    #[inline]
    fn update_anchor(&mut self, current_price: f64) {
        if self.trailing_stop_pct.is_none() {
            return;
        }
        match self.trailing_stop_anchor {
            None => {
                self.trailing_stop_anchor = Some(current_price);
            }
            Some(anchor) => {
                let should_update = if self.is_long {
                    current_price > anchor
                } else {
                    current_price < anchor
                };
                if should_update {
                    self.trailing_stop_anchor = Some(current_price);
                }
            }
        }
    }

    /// Compute the trailing stop trigger price from the current anchor.
    ///
    /// - Long: `anchor * (1 - pct)`
    /// - Short: `anchor * (1 + pct)`
    #[inline]
    fn trailing_stop_price(&self) -> Option<f64> {
        let pct = self.trailing_stop_pct?;
        let anchor = self.trailing_stop_anchor?;
        if self.is_long {
            Some(anchor * (1.0 - pct))
        } else {
            Some(anchor * (1.0 + pct))
        }
    }

    /// Evaluate which trigger (if any) fires at the given price.
    ///
    /// Must be called after `update_anchor` so trailing stop is current.
    #[inline]
    fn check(&self, current_price: f64) -> Option<TriggerAction> {
        // 1. Stop-loss (highest priority)
        if let Some(sl) = self.stop_loss {
            let triggered = if self.is_long {
                current_price <= sl
            } else {
                current_price >= sl
            };
            if triggered {
                return Some(TriggerAction {
                    trigger_type: TriggerType::StopLoss,
                    position_id: self.position_id,
                    perp_id: self.perp_id,
                    trigger_price: sl,
                });
            }
        }

        // 2. Take-profit
        if let Some(tp) = self.take_profit {
            let triggered = if self.is_long {
                current_price >= tp
            } else {
                current_price <= tp
            };
            if triggered {
                return Some(TriggerAction {
                    trigger_type: TriggerType::TakeProfit,
                    position_id: self.position_id,
                    perp_id: self.perp_id,
                    trigger_price: tp,
                });
            }
        }

        // 3. Trailing stop
        if let Some(ts_price) = self.trailing_stop_price() {
            let triggered = if self.is_long {
                current_price <= ts_price
            } else {
                current_price >= ts_price
            };
            if triggered {
                return Some(TriggerAction {
                    trigger_type: TriggerType::TrailingStop,
                    position_id: self.position_id,
                    perp_id: self.perp_id,
                    trigger_price: ts_price,
                });
            }
        }

        None
    }
}

/// Manages tracked positions and evaluates triggers.
///
/// Not thread-safe — intended to be owned by a single trading loop.
/// For concurrent access, wrap in `Mutex` or `RwLock`.
#[derive(Debug)]
pub struct PositionManager {
    positions: HashMap<u64, ManagedPosition>,
}

impl PositionManager {
    /// Create an empty manager.
    pub fn new() -> Self {
        Self {
            positions: HashMap::new(),
        }
    }

    /// Start tracking a position.
    pub fn track(&mut self, pos: ManagedPosition) {
        self.positions.insert(pos.position_id, pos);
    }

    /// Stop tracking a position. Returns `true` if it was tracked.
    pub fn untrack(&mut self, position_id: u64) -> bool {
        self.positions.remove(&position_id).is_some()
    }

    /// Get a reference to a tracked position.
    pub fn get(&self, position_id: u64) -> Option<&ManagedPosition> {
        self.positions.get(&position_id)
    }

    /// Get a mutable reference to a tracked position (e.g. to update triggers).
    pub fn get_mut(&mut self, position_id: u64) -> Option<&mut ManagedPosition> {
        self.positions.get_mut(&position_id)
    }

    /// Evaluate all positions against per-perp mark prices.
    ///
    /// Each position is only evaluated against the price for its own `perp_id`.
    /// Positions whose `perp_id` is not in the prices map are skipped.
    /// Returns at most one [`TriggerAction`] per position.
    pub fn check_triggers(&mut self, prices: &HashMap<[u8; 32], f64>) -> Vec<TriggerAction> {
        let mut actions = Vec::new();
        self.check_triggers_into(prices, &mut actions);
        actions
    }

    /// Zero-allocation trigger check: appends fired triggers to `out`.
    ///
    /// Each position is only evaluated against the price for its own `perp_id`.
    /// Call with a reusable `Vec` to avoid heap allocation on the hot path:
    ///
    /// ```
    /// # use perpcity_rust_sdk::hft::position_manager::{PositionManager, TriggerAction};
    /// # use std::collections::HashMap;
    /// let mut mgr = PositionManager::new();
    /// let mut buf: Vec<TriggerAction> = Vec::with_capacity(16);
    /// let mut prices = HashMap::new();
    /// prices.insert([0xAA; 32], 105.0);
    /// // On each price tick:
    /// buf.clear();
    /// mgr.check_triggers_into(&prices, &mut buf);
    /// // Process buf — no allocation after the first call.
    /// ```
    #[inline]
    pub fn check_triggers_into(
        &mut self,
        prices: &HashMap<[u8; 32], f64>,
        out: &mut Vec<TriggerAction>,
    ) {
        for pos in self.positions.values_mut() {
            let Some(&current_price) = prices.get(&pos.perp_id) else {
                continue;
            };
            pos.update_anchor(current_price);
            if let Some(action) = pos.check(current_price) {
                out.push(action);
            }
        }
    }

    /// Number of tracked positions.
    pub fn count(&self) -> usize {
        self.positions.len()
    }
}

impl Default for PositionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_pos(id: u64, entry: f64) -> ManagedPosition {
        ManagedPosition {
            perp_id: [0xAA; 32],
            position_id: id,
            is_long: true,
            entry_price: entry,
            margin: 100.0,
            stop_loss: None,
            take_profit: None,
            trailing_stop_pct: None,
            trailing_stop_anchor: None,
        }
    }

    fn short_pos(id: u64, entry: f64) -> ManagedPosition {
        ManagedPosition {
            perp_id: [0xBB; 32],
            position_id: id,
            is_long: false,
            entry_price: entry,
            margin: 100.0,
            stop_loss: None,
            take_profit: None,
            trailing_stop_pct: None,
            trailing_stop_anchor: None,
        }
    }

    // ── Basic management ───────────────────────────────────────

    #[test]
    fn track_and_untrack() {
        let mut mgr = PositionManager::new();
        mgr.track(long_pos(1, 100.0));
        assert_eq!(mgr.count(), 1);
        assert!(mgr.get(1).is_some());
        assert!(mgr.untrack(1));
        assert_eq!(mgr.count(), 0);
        assert!(!mgr.untrack(1)); // already removed
    }

    /// Helper: build a price map for a single perp_id.
    fn prices_for(perp_id: [u8; 32], price: f64) -> HashMap<[u8; 32], f64> {
        HashMap::from([(perp_id, price)])
    }

    /// Helper: long positions use perp_id [0xAA; 32].
    fn long_prices(price: f64) -> HashMap<[u8; 32], f64> {
        prices_for([0xAA; 32], price)
    }

    /// Helper: short positions use perp_id [0xBB; 32].
    fn short_prices(price: f64) -> HashMap<[u8; 32], f64> {
        prices_for([0xBB; 32], price)
    }

    // ── Stop-loss triggers ─────────────────────────────────────

    #[test]
    fn long_stop_loss_triggers_below() {
        let mut mgr = PositionManager::new();
        let mut pos = long_pos(1, 100.0);
        pos.stop_loss = Some(90.0);
        mgr.track(pos);

        // Price above SL: no trigger
        let t = mgr.check_triggers(&long_prices(95.0));
        assert!(t.is_empty());

        // Price at SL: triggers
        let t = mgr.check_triggers(&long_prices(90.0));
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].trigger_type, TriggerType::StopLoss);
        assert_eq!(t[0].trigger_price, 90.0);
    }

    #[test]
    fn short_stop_loss_triggers_above() {
        let mut mgr = PositionManager::new();
        let mut pos = short_pos(1, 100.0);
        pos.stop_loss = Some(110.0);
        mgr.track(pos);

        let t = mgr.check_triggers(&short_prices(105.0));
        assert!(t.is_empty());

        let t = mgr.check_triggers(&short_prices(110.0));
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].trigger_type, TriggerType::StopLoss);
    }

    // ── Take-profit triggers ───────────────────────────────────

    #[test]
    fn long_take_profit_triggers_above() {
        let mut mgr = PositionManager::new();
        let mut pos = long_pos(1, 100.0);
        pos.take_profit = Some(120.0);
        mgr.track(pos);

        let t = mgr.check_triggers(&long_prices(115.0));
        assert!(t.is_empty());

        let t = mgr.check_triggers(&long_prices(125.0));
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].trigger_type, TriggerType::TakeProfit);
    }

    #[test]
    fn short_take_profit_triggers_below() {
        let mut mgr = PositionManager::new();
        let mut pos = short_pos(1, 100.0);
        pos.take_profit = Some(80.0);
        mgr.track(pos);

        let t = mgr.check_triggers(&short_prices(85.0));
        assert!(t.is_empty());

        let t = mgr.check_triggers(&short_prices(75.0));
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].trigger_type, TriggerType::TakeProfit);
    }

    // ── Trailing stop ──────────────────────────────────────────

    #[test]
    fn long_trailing_stop() {
        let mut mgr = PositionManager::new();
        let mut pos = long_pos(1, 100.0);
        pos.trailing_stop_pct = Some(0.05); // 5% trailing
        mgr.track(pos);

        // Price rises to 110 → anchor set to 110
        let t = mgr.check_triggers(&long_prices(110.0));
        assert!(t.is_empty());
        assert_eq!(mgr.get(1).unwrap().trailing_stop_anchor, Some(110.0));

        // Price rises to 120 → anchor updates to 120
        let t = mgr.check_triggers(&long_prices(120.0));
        assert!(t.is_empty());
        assert_eq!(mgr.get(1).unwrap().trailing_stop_anchor, Some(120.0));

        // Price drops to 115 → trailing stop = 120 * 0.95 = 114
        // 115 > 114 → no trigger
        let t = mgr.check_triggers(&long_prices(115.0));
        assert!(t.is_empty());

        // Price drops to 113 → 113 < 114 → trigger!
        let t = mgr.check_triggers(&long_prices(113.0));
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].trigger_type, TriggerType::TrailingStop);
        // Trigger price should be 120 * 0.95 = 114.0
        assert!((t[0].trigger_price - 114.0).abs() < 1e-10);
    }

    #[test]
    fn short_trailing_stop() {
        let mut mgr = PositionManager::new();
        let mut pos = short_pos(1, 100.0);
        pos.trailing_stop_pct = Some(0.05);
        mgr.track(pos);

        // Price drops to 90 → anchor set to 90
        let t = mgr.check_triggers(&short_prices(90.0));
        assert!(t.is_empty());
        assert_eq!(mgr.get(1).unwrap().trailing_stop_anchor, Some(90.0));

        // Price drops to 80 → anchor updates to 80
        let t = mgr.check_triggers(&short_prices(80.0));
        assert!(t.is_empty());

        // Price rises to 84 → trailing stop = 80 * 1.05 = 84
        // 84 >= 84 → trigger!
        let t = mgr.check_triggers(&short_prices(84.0));
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].trigger_type, TriggerType::TrailingStop);
    }

    // ── Priority / precedence ──────────────────────────────────

    #[test]
    fn stop_loss_takes_priority_over_trailing_stop() {
        let mut mgr = PositionManager::new();
        let mut pos = long_pos(1, 100.0);
        pos.stop_loss = Some(85.0);
        pos.trailing_stop_pct = Some(0.05);
        pos.trailing_stop_anchor = Some(100.0); // trailing stop at 95
        mgr.track(pos);

        // Price = 80: both SL (85) and trailing (95) would fire
        let t = mgr.check_triggers(&long_prices(80.0));
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].trigger_type, TriggerType::StopLoss);
    }

    #[test]
    fn take_profit_takes_priority_over_trailing_stop() {
        let mut mgr = PositionManager::new();
        let mut pos = short_pos(1, 100.0);
        pos.take_profit = Some(80.0);
        pos.trailing_stop_pct = Some(0.50); // 50% trailing (absurdly wide)
        pos.trailing_stop_anchor = Some(50.0); // trailing stop at 75
        mgr.track(pos);

        // Price = 70: TP (80) fires because 70 <= 80, trailing (75) also fires
        let t = mgr.check_triggers(&short_prices(70.0));
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].trigger_type, TriggerType::TakeProfit);
    }

    // ── No triggers set ────────────────────────────────────────

    #[test]
    fn no_triggers_means_no_actions() {
        let mut mgr = PositionManager::new();
        mgr.track(long_pos(1, 100.0)); // no SL, TP, or trailing
        let t = mgr.check_triggers(&long_prices(50.0));
        assert!(t.is_empty());
    }

    // ── Multiple positions (different perps) ───────────────────

    #[test]
    fn multiple_positions_independent_perps() {
        let mut mgr = PositionManager::new();

        let mut pos1 = long_pos(1, 100.0); // perp_id [0xAA; 32]
        pos1.stop_loss = Some(90.0);
        mgr.track(pos1);

        let mut pos2 = short_pos(2, 100.0); // perp_id [0xBB; 32]
        pos2.take_profit = Some(80.0);
        mgr.track(pos2);

        // Only BTC price available — only pos1 evaluated
        let t = mgr.check_triggers(&long_prices(85.0));
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].position_id, 1);

        // Both prices available
        let mut both = long_prices(75.0);
        both.insert([0xBB; 32], 75.0);
        let t = mgr.check_triggers(&both);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn position_skipped_when_no_price_for_perp() {
        let mut mgr = PositionManager::new();
        let mut pos = long_pos(1, 100.0);
        pos.stop_loss = Some(90.0);
        mgr.track(pos);

        // Pass price for a different perp — position should be skipped
        let t = mgr.check_triggers(&short_prices(50.0));
        assert!(t.is_empty());
    }

    // ── Trailing stop anchor updates correctly ─────────────────

    #[test]
    fn anchor_only_moves_favorably() {
        let mut mgr = PositionManager::new();
        let mut pos = long_pos(1, 100.0);
        pos.trailing_stop_pct = Some(0.10);
        mgr.track(pos);

        mgr.check_triggers(&long_prices(110.0)); // anchor → 110
        mgr.check_triggers(&long_prices(105.0)); // anchor stays 110 (not 105)
        assert_eq!(mgr.get(1).unwrap().trailing_stop_anchor, Some(110.0));

        mgr.check_triggers(&long_prices(115.0)); // anchor → 115
        assert_eq!(mgr.get(1).unwrap().trailing_stop_anchor, Some(115.0));
    }

    #[test]
    fn trailing_stop_no_pct_means_no_anchor_update() {
        let mut mgr = PositionManager::new();
        let pos = long_pos(1, 100.0); // no trailing_stop_pct
        mgr.track(pos);

        mgr.check_triggers(&long_prices(200.0));
        assert_eq!(mgr.get(1).unwrap().trailing_stop_anchor, None);
    }
}
