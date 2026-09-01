//! # PerpCity Rust SDK
//!
//! A Rust SDK for the [PerpCity](https://perpcity.com) perpetual futures
//! protocol on Arbitrum (mainnet and Arbitrum Sepolia testnet).
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
//! | [`math`] | Pure math: tick ↔ price, liquidity, positions, EMAs, taker swap simulation, maker settle previews |
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
//!
//! [`PerpClient`] accepts any `alloy` transaction signer (`TxSigner`), not just
//! `PrivateKeySigner` — e.g. AWS KMS via `alloy::signers::aws::AwsSigner` with
//! this crate's `aws` feature enabled (see `examples/aws_kms_signer.rs`).

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
pub use client::{
    ARBITRUM_CHAIN_ID, ARBITRUM_POOL_MANAGER, ARBITRUM_SEPOLIA_CHAIN_ID,
    ARBITRUM_SEPOLIA_PERP_FACTORY, ARBITRUM_SEPOLIA_POOL_MANAGER, ARBITRUM_SEPOLIA_USDC,
    ARBITRUM_USDC, MAX_MAKER_EQUITY_BATCH, MakerEquityKind, MakerEquityOutcome, PerpClient,
    TxBuilder,
};

#[doc(inline)]
pub use contracts::{
    AdjustMakerParams as ContractAdjustMakerParams, AdjustTakerParams as ContractAdjustTakerParams,
    IBeacon, IERC20, IFees, IFunding, IMarginRatios, IMulticall3, IPoolManagerState, IPriceImpact,
    IPricing, Modules, OpenMakerParams as ContractOpenMakerParams,
    OpenTakerParams as ContractOpenTakerParams, Perp, PerpDeployedEvents, PerpFactory, PoolKey,
};

#[doc(inline)]
pub use feeds::{
    BlockHeaderFeed, LiveTakerMarket, LiveTakerMarketPublisher, MarketEvent, MarketFeed, decode_log,
};

#[doc(inline)]
pub use errors::{ContractError, PerpCityError, Result, TransactionError, ValidationError};

#[doc(inline)]
pub use hft::gas::{GasLimits, Urgency};

#[doc(inline)]
pub use transport::{config::TransportConfig, provider::HftTransport};

#[doc(inline)]
pub use types::{
    AdjustMakerParams, AdjustMakerResult, AdjustTakerParams, AdjustTakerResult, Bounds,
    Deployments, ExactAdjustTakerParams, ExactOpenTakerParams, Fees, OpenInterest, OpenMakerParams,
    OpenResult, OpenTakerParams, PerpData, PerpSnapshot, PriceImpactPoint,
};

#[doc(inline)]
pub use math::BlockContext;

#[doc(inline)]
pub use math::maker_equity::{
    AccrualInputs, AccruedMakerSnapshot, MakerEquityBreakdown, MakerMarketSnapshot, MakerState,
    TickFunding,
};

#[doc(inline)]
pub use math::tick::{
    align_tick_down, align_tick_up, get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, price_to_tick,
    tick_to_price,
};

#[doc(inline)]
pub use math::swap::{
    QuoteConstraints, QuoteLimit, TakerMarketSnapshot, TakerQuote, TickLiquidity,
};
