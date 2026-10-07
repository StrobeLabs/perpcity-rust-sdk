//! # PerpCity Rust SDK
//!
//! A Rust SDK for the [PerpCity](https://perpcity.com) perpetual futures
//! protocol on Arbitrum (mainnet and Arbitrum Sepolia testnet).
//!
//! ## Module overview
//!
//! | Module | Purpose |
//! |---|---|
//! | [`client`] | The reads and the trades, and the parameters, results, configuration and state they are spoken in |
//! | [`constants`] | Protocol constants mirrored from on-chain `Constants.sol` |
//! | [`contracts`] | ABI bindings via Alloy `sol!` — structs, events, errors, functions |
//! | [`convert`] | Conversions between client f64 values and on-chain representations |
//! | [`errors`] | SDK-wide error types using `thiserror` |
//! | [`feeds`] | Live data feeds over WebSocket: market events, block headers, event decoding |
//! | [`hft`] | HFT infrastructure: nonce, gas, pipeline, state cache, latency, positions |
//! | [`history`] | Historical log reads over block ranges of any length; beacon index print series |
//! | [`math`] | Pure math: tick ↔ price, liquidity, positions, EMAs, the deployed fair price, taker swap simulation, maker settle previews |
//! | [`prelude`] | Everyday public surface, bundled for `use perpcity_sdk::prelude::*;` |
//! | [`transport`] | Multi-endpoint RPC transport with health-aware routing |
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use perpcity_sdk::prelude::*;
//! use alloy::signers::local::PrivateKeySigner;
//! ```
//!
//! Or import only what's needed — every [`prelude`] item is also available
//! individually at the crate root (`perpcity_sdk::PerpClient`, etc.).
//!
//! [`PerpClient`] accepts any `alloy` transaction signer (`TxSigner`), not just
//! `PrivateKeySigner` — e.g. AWS KMS via `alloy::signers::aws::AwsSigner` with
//! this crate's `aws` feature enabled (see `examples/aws_kms_signer.rs`).

#![deny(unreachable_pub)]
#![warn(missing_debug_implementations, missing_docs, rust_2018_idioms)]
#![doc = "\n\nThe crate's design lives beside its code as a graph of nodes, one per component, rooted at [`DESIGN.md`](https://github.com/StrobeLabs/perpcity-rust-sdk/blob/main/DESIGN.md); `cargo xtask design --open` draws its type graph."]

pub mod client;
pub mod constants;
pub mod contracts;
pub mod convert;
pub mod errors;
pub mod events;
pub mod feeds;
pub mod hft;
pub mod history;
pub mod math;
pub mod prelude;
pub(crate) mod storage;
pub mod transport;
pub mod units;

#[doc(inline)]
pub use client::{
    ARBITRUM_CHAIN_ID, ARBITRUM_POOL_MANAGER, ARBITRUM_SEPOLIA_CHAIN_ID,
    ARBITRUM_SEPOLIA_PERP_FACTORY, ARBITRUM_SEPOLIA_POOL_MANAGER, ARBITRUM_SEPOLIA_USDC,
    ARBITRUM_USDC, AdjustMakerParams, AdjustMakerResult, AdjustTakerParams, AdjustTakerResult,
    Bounds, ChainDeployments, ChainReader, ExactAdjustMakerParams, ExactAdjustTakerParams,
    ExactOpenMakerParams, ExactOpenTakerParams, Fees, MAX_ROW_BATCH, MakerEquityKind,
    MakerEquityOutcome, MarginRatioTriple, MarginRatios, MarketConfig, MarketRates, MarketReader,
    MarketSnapshot, OpenInterest, OpenMakerParams, OpenResult, OpenTakerParams, PerpClient,
    RowOutcome, SolvencyState, StateAt, TxBuilder,
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
pub use units::{
    Earnings, Factor, FeeGrowth, Funding, FundingPerSqrtPrice, FundingRate, LDelta, LUnits,
    PerSide, PerpAtoms, PerpDelta, Price, Ratio, Share, Side, SqrtPrice, UsdcAtoms, UsdcDelta,
    UtilizationRate,
};

#[doc(inline)]
pub use math::{BlockContext, LiquidationPrices};

#[doc(inline)]
pub use math::pricing::{Emas, Mark, PricePair, calculate_emas, fair_price, fair_price_f64};

#[doc(inline)]
pub use math::maker_equity::{
    AccrualInputs, AccruedMakerSnapshot, MakerEquityBreakdown, MakerMarketSnapshot, MakerState,
    TickFunding,
};

#[doc(inline)]
pub use math::taker::{TakerHealth, TakerMarketSnapshot, TakerState};

#[doc(inline)]
pub use math::tick::{
    align_tick_down, align_tick_up, get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, price_to_tick,
    tick_to_price,
};

#[doc(inline)]
pub use math::swap::{PoolSnapshot, QuoteConstraints, QuoteLimit, TakerQuote, TickLiquidity};

#[doc(inline)]
pub use math::capacity::{Capacity, MarketCapacity, band_capacity, liquidity_for_capacity};

#[doc(inline)]
pub use math::range::{MakerBand, TickRange};

#[doc(inline)]
pub use math::liquidity::{
    amounts_for_liquidity, estimate_liquidity, liquidity_change_delta, liquidity_for_target_ratio,
    margin_for_liquidity,
};
