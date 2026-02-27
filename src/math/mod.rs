//! Pure math functions for the PerpCity protocol.
//!
//! These operate directly on Alloy primitives (`U256`, `I256`) and f64 —
//! no structs, no state, just math. Each submodule corresponds to a domain:
//!
//! | Module | Purpose |
//! |---|---|
//! | [`tick`] | Tick ↔ price conversions, tick alignment, `getSqrtRatioAtTick` |
//! | [`liquidity`] | Liquidity estimation for maker positions |
//! | [`position`] | Entry price, size, value, leverage, liquidation price |

pub mod liquidity;
pub mod position;
pub mod tick;
