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
//!
//! Storage-slot derivation for the deployed contract layouts is not math
//! and lives in the crate-internal `storage` module beside `contracts`.

use alloy::primitives::B256;
use serde::{Deserialize, Serialize};

pub mod ema;
pub(crate) mod fixed_point;
pub mod liquidity;
pub mod maker_equity;
pub mod position;
pub mod swap;
pub mod tick;

/// The block a market snapshot's state was read at.
///
/// Shared by [`swap::TakerMarketSnapshot`] and
/// [`maker_equity::MakerMarketSnapshot`]: every field in a snapshot comes
/// from this one block, and chain reads derived from the snapshot pin to
/// [`Self::hash`].
///
/// The client's snapshot loaders pin this block
/// [`SNAPSHOT_BLOCK_LAG`](crate::constants::SNAPSHOT_BLOCK_LAG) behind the
/// chain head, so [`Self::hash`] is generally not the newest head and
/// [`Self::timestamp`] trails wall-clock time by the lag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockContext {
    /// Block number.
    pub number: u64,
    /// Canonical block hash.
    pub hash: B256,
    /// Block timestamp (seconds since the Unix epoch).
    pub timestamp: u64,
}
