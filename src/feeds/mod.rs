//! Live data feeds over WebSocket.
//!
//! | Feed | Source | Purpose |
//! |------|--------|---------|
//! | [`MarketFeed`] | Contract event logs | Trading events (positions, index updates) |
//! | [`BlockHeaderFeed`] | `newHeads` subscription | Block headers (base fee for gas pricing) |
//!
//! [`MarketFeed`] streams the present tense of the market's event
//! vocabulary, which lives in [`crate::events`] — the same
//! [`MarketEvent`] values [`crate::history`] replays from the past.

#![doc = "\n\nThe design of this module: [`src/feeds/DESIGN.md`](https://github.com/StrobeLabs/perpcity-rust-sdk/blob/main/src/feeds/DESIGN.md)."]

pub mod block;
pub mod market;
pub mod taker;

/// The event vocabulary, at its former path.
///
/// It moved to [`crate::events`] when history began replaying the same
/// events feeds stream: the vocabulary belongs to neither tense. This
/// re-export keeps existing imports working.
pub mod events {
    pub use crate::events::*;
}

pub use crate::events::{MarketEvent, decode_log};
pub use block::BlockHeaderFeed;
pub use market::MarketFeed;
pub use taker::{LiveTakerMarket, LiveTakerMarketPublisher};
