//! Convenience re-export of the SDK's everyday public surface.
//!
//! `use perpcity_sdk::prelude::*;` pulls in the same items already
//! available individually at the crate root (see `lib.rs`'s own
//! `#[doc(inline)] pub use` blocks) — this module just bundles them behind
//! one import instead of naming each one: the client and transport
//! (`PerpClient`, `HftTransport`, `TransportConfig`, `TxBuilder`), errors
//! (`PerpCityError`, `Result`, `ContractError`, `TransactionError`,
//! `ValidationError`), gas/urgency (`GasLimits`, `Urgency`), feeds
//! (`MarketFeed`, `MarketEvent`, `decode_log`, ...), the client-facing
//! params/result types (`OpenTakerParams`, `OpenResult`, ...), the
//! maker-equity types (`MakerEquityBreakdown`, `MakerState`, ...), and
//! liquidity sizing (`estimate_liquidity`, `liquidity_for_target_ratio`).
//!
//! It re-exports exactly that set, nothing more: lower-level ABI/
//! contract-interface types (`contracts::*`) and the fine-grained math
//! helpers (`math::tick`, `math::swap`, `convert`) are not included, since
//! they're reached for far less often than everything above.

#[doc(no_inline)]
pub use crate::{
    AccrualInputs, AccruedMakerSnapshot, AdjustMakerParams, AdjustMakerResult, AdjustTakerParams,
    AdjustTakerResult, BlockContext, BlockHeaderFeed, Bounds, ContractError, Deployments,
    ExactAdjustTakerParams, ExactOpenTakerParams, Fees, GasLimits, HftTransport, LiveTakerMarket,
    LiveTakerMarketPublisher, MAX_MAKER_EQUITY_BATCH, MakerEquityBreakdown, MakerEquityKind,
    MakerEquityOutcome, MakerMarketSnapshot, MakerState, MarketEvent, MarketFeed, OpenInterest,
    OpenMakerParams, OpenResult, OpenTakerParams, PerpCityError, PerpClient, PerpData,
    PerpSnapshot, PriceImpactPoint, Result, TickFunding, TransactionError, TransportConfig,
    TxBuilder, Urgency, ValidationError, decode_log, estimate_liquidity,
    liquidity_for_target_ratio,
};
