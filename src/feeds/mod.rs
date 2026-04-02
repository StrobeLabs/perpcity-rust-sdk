//! Live data feeds over WebSocket.
//!
//! | Feed | Source | Purpose |
//! |------|--------|---------|
//! | [`MarketFeed`] | Contract event logs | Trading events (positions, index updates) |
//! | [`BlockHeaderFeed`] | `newHeads` subscription | Block headers (base fee for gas pricing) |
//!
//! The [`events`] submodule provides the [`MarketEvent`] type and
//! [`decode_log`] function used by [`MarketFeed`] to decode raw logs.

pub mod block;
pub mod events;
pub mod market;

pub use block::BlockHeaderFeed;
pub use events::{MarketEvent, decode_log};
pub use market::MarketFeed;
