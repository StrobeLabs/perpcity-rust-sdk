//! # PerpCity Rust SDK
//!
//! A Rust SDK for the [PerpCity](https://perpcity.com) perpetual futures
//! protocol on Base L2.
//!
//! ## Module overview
//!
//! | Module | Purpose |
//! |---|---|
//! | [`constants`] | Protocol constants mirrored from on-chain `Constants.sol` |
//! | [`contracts`] | ABI bindings via Alloy `sol!` — structs, events, errors, functions |
//! | [`convert`] | Conversions between client f64 values and on-chain representations |
//! | [`errors`] | SDK-wide error types using `thiserror` |
//! | [`feeds`] | Live data feeds over WebSocket: market events, block headers, event decoding |
//! | [`hft`] | HFT infrastructure: nonce, gas, pipeline, state cache, latency, positions |
//! | [`math`] | Pure math: tick ↔ price, liquidity estimation, position calculations |
//! | [`transport`] | Multi-endpoint RPC transport with health-aware routing |
//! | [`types`] | Client-facing types with human-readable f64 fields |
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use perpcity_sdk::{
//!     PerpClient, HftTransport, TransportConfig, Urgency,
//!     Deployments, OpenTakerParams, OpenMakerParams,
//!     PerpCityError, Result,
//! };
//! use alloy::signers::local::PrivateKeySigner;
//! ```

#![deny(unreachable_pub)]
#![warn(missing_debug_implementations, missing_docs, rust_2018_idioms)]

pub mod client;
pub mod constants;
pub mod contracts;
pub mod convert;
pub mod errors;
pub mod feeds;
pub mod hft;
pub mod math;
pub mod transport;
pub mod types;

#[doc(inline)]
pub use client::PerpClient;

#[doc(inline)]
pub use contracts::{
    IBeacon, IERC20, IFees, IMarginRatios, IMulticall3, PerpManager, PoolKey, SwapConfig,
};

#[doc(inline)]
pub use feeds::{BlockHeaderFeed, MarketEvent, MarketFeed, decode_log};

#[doc(inline)]
pub use errors::{ContractError, PerpCityError, Result, TransactionError, ValidationError};

#[doc(inline)]
pub use hft::gas::{GasLimits, Urgency};

#[doc(inline)]
pub use transport::{config::TransportConfig, provider::HftTransport};

#[doc(inline)]
pub use types::{
    AdjustMarginParams, AdjustMarginResult, AdjustNotionalParams, AdjustNotionalResult, Bounds,
    CloseParams, CloseResult, Deployments, Fees, LiveDetails, OpenInterest, OpenMakerParams,
    OpenMakerQuote, OpenResult, OpenTakerParams, OpenTakerQuote, PerpData, PerpSnapshot, SwapQuote,
};

#[doc(inline)]
pub use math::tick::{
    align_tick_down, align_tick_up, get_sqrt_ratio_at_tick, price_to_tick, tick_to_price,
};
