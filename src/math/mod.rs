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
//! | [`ema`] | Contract-exact EMA advancement (Solady `expWad` port) |
//! | [`swap`] | Local V4 taker swap simulation over a block-pinned book |
//! | [`maker_equity`] | Contract-exact maker settle preview over a block-pinned snapshot |
//! | [`storage`] | Storage-slot math for reads with no contract getter |

pub mod ema;
pub(crate) mod fixed_point;
pub mod liquidity;
pub mod maker_equity;
pub mod position;
pub mod storage;
pub mod swap;
pub mod tick;
