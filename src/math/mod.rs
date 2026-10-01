//! Pure math functions for the PerpCity protocol.
//!
//! These operate directly on Alloy primitives (`U256`, `I256`) and f64 —
//! no structs, no state, just math. Each submodule corresponds to a domain:
//!
//! | Module | Purpose |
//! |---|---|
//! | [`tick`] | Tick ↔ price conversions, tick alignment, `getSqrtRatioAtTick` |
//! | [`capacity`] | Taker capacity a maker band adds, and the liquidity for a target capacity |
//! | [`range`] | A maker's geometry: the validated tick range, and the band of liquidity in it, that the maker math is over |
//! | [`liquidity`] | Liquidity sizing and band token amounts for maker positions |
//! | [`position`] | Entry price, size, value, leverage, liquidation price |
//! | [`pricing`] | The pool price, the index, their contract-exact EMAs, and the mark: the deployed fair price |
//! | [`swap`] | Local V4 taker swap simulation over a block-pinned pool |
//! | [`maker_equity`] | Contract-exact maker settle preview over a block-pinned snapshot |
//!
//! Two things that look like they belong here do not. Storage-slot
//! derivation for the deployed contract layouts lives in the crate-internal
//! `storage` module beside `contracts`. And the Solidity-compatible integer
//! primitives every port is built on live in the crate-internal
//! `fixed_point` module at the root, below [`crate::units`], which needs
//! them too.

#![doc = "\n\nThe design of this module: [`src/math/DESIGN.md`](https://github.com/StrobeLabs/perpcity-rust-sdk/blob/main/src/math/DESIGN.md)."]

use alloy::primitives::B256;
use serde::{Deserialize, Serialize};

pub mod capacity;
pub mod liquidity;
pub mod maker_equity;
pub mod position;
pub mod pricing;
pub mod range;
pub mod swap;
pub mod tick;

/// The block a market snapshot's state was read at.
///
/// Shared by [`swap::PoolSnapshot`] and
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
