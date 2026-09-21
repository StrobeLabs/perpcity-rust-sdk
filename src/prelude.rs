//! Convenience re-export of the SDK's everyday public surface.
//!
//! `use perpcity_sdk::prelude::*;` pulls in the same items already
//! available individually at the crate root (see `lib.rs`'s own
//! `#[doc(inline)] pub use` blocks) — this module just bundles them behind
//! one import instead of naming each one: the client and transport
//! (`PerpClient`, `HftTransport`, `TransportConfig`, `TxBuilder`), the
//! well-known chain ids/addresses (`ARBITRUM_CHAIN_ID`,
//! `ARBITRUM_SEPOLIA_USDC`, ...), errors (`PerpCityError`, `Result`,
//! `ContractError`, `TransactionError`, `ValidationError`), gas/urgency
//! (`GasLimits`, `Urgency`), feeds (`MarketFeed`, `MarketEvent`,
//! `decode_log`, ...), the client-facing params/result types
//! (`OpenTakerParams`, `OpenResult`, ...), the maker-equity types
//! (`MakerEquityBreakdown`, `MakerState`, ...), the market's margin-ratio
//! thresholds (`MarginRatios`, `MarginRatioTriple`), liquidity sizing
//! (`estimate_liquidity`, `liquidity_for_target_ratio`,
//! `amounts_for_liquidity`), taker capacity (`Side`, `Capacity`,
//! `MarketCapacity`, `band_capacity`, `liquidity_for_capacity`), the
//! deployed fair price (`FairPrice`, `fair_price`, `fair_price_x96`), and
//! tick/price conversion (`price_to_tick`, `tick_to_price`, ...).
//!
//! It re-exports exactly that set, nothing more: lower-level ABI/
//! contract-interface types (`contracts::*`) and the fine-grained
//! `math::swap`/`convert` helpers are not included, since they're reached
//! for far less often than everything above.

#[doc(no_inline)]
pub use crate::{
    ARBITRUM_CHAIN_ID, ARBITRUM_POOL_MANAGER, ARBITRUM_SEPOLIA_CHAIN_ID,
    ARBITRUM_SEPOLIA_PERP_FACTORY, ARBITRUM_SEPOLIA_POOL_MANAGER, ARBITRUM_SEPOLIA_USDC,
    ARBITRUM_USDC, AccrualInputs, AccruedMakerSnapshot, AdjustMakerParams, AdjustMakerResult,
    AdjustTakerParams, AdjustTakerResult, BlockContext, BlockHeaderFeed, Bounds, Capacity,
    ContractError, Deployments, ExactAdjustTakerParams, ExactOpenTakerParams, FairPrice, Fees,
    GasLimits, HftTransport, LiveTakerMarket, LiveTakerMarketPublisher, MAX_MAKER_EQUITY_BATCH,
    MakerEquityBreakdown, MakerEquityKind, MakerEquityOutcome, MakerMarketSnapshot, MakerState,
    MarginRatioTriple, MarginRatios, MarketCapacity, MarketEvent, MarketFeed, OpenInterest,
    OpenMakerParams, OpenResult, OpenTakerParams, PerpCityError, PerpClient, PerpData,
    PerpSnapshot, PriceImpactPoint, Result, Side, TickFunding, TransactionError, TransportConfig,
    TxBuilder, Urgency, ValidationError, align_tick_down, align_tick_up, amounts_for_liquidity,
    band_capacity, decode_log, estimate_liquidity, fair_price, fair_price_x96,
    get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, liquidity_for_capacity,
    liquidity_for_target_ratio, price_to_tick, tick_to_price,
};
