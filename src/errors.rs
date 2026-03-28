//! Error types for the PerpCity SDK.
//!
//! Follows the axiomtrade-rs pattern: a single top-level enum with
//! domain-specific variants plus `#[from]` conversions for library errors.

use alloy::primitives::{B256, U256};
use thiserror::Error;

/// Central error type for the PerpCity SDK.
#[derive(Error, Debug)]
#[non_exhaustive]
#[allow(missing_docs)]
pub enum PerpCityError {
    // ── Validation errors ────────────────────────────────────────────
    /// Price must be positive and within protocol bounds.
    #[error("invalid price: {reason}")]
    InvalidPrice { reason: String },

    /// Margin does not meet the minimum opening requirement.
    #[error("invalid margin: {reason}")]
    InvalidMargin { reason: String },

    /// Leverage is outside the allowed range for this perp.
    #[error("invalid leverage: {reason}")]
    InvalidLeverage { reason: String },

    /// Tick range violates protocol bounds or spacing.
    #[error("invalid tick range: lower={lower}, upper={upper}")]
    InvalidTickRange { lower: i32, upper: i32 },

    /// Margin ratio is outside the allowed [min, max] window.
    #[error("invalid margin ratio: {value} (must be in [{min}, {max}])")]
    InvalidMarginRatio { value: u32, min: u32, max: u32 },

    /// A configuration value is invalid or missing.
    #[error("invalid configuration: {reason}")]
    InvalidConfig { reason: String },

    // ── Arithmetic errors ────────────────────────────────────────────
    /// An arithmetic operation overflowed.
    #[error("arithmetic overflow: {context}")]
    Overflow { context: String },

    // ── Transaction / on-chain errors ────────────────────────────────
    /// A sent transaction reverted on-chain.
    #[error("transaction reverted: {reason}")]
    TxReverted { reason: String },

    /// An expected event was not found in the transaction receipt.
    #[error("event not found in receipt: {event_name}")]
    EventNotFound { event_name: String },

    /// Could not estimate or fetch gas price from the network.
    #[error("gas price unavailable: {reason}")]
    GasPriceUnavailable { reason: String },

    /// Too many unconfirmed transactions in flight.
    #[error("too many in-flight transactions: {count} (max {max})")]
    TooManyInFlight { count: usize, max: usize },

    // ── Contract / protocol errors ───────────────────────────────────
    /// The perp does not exist on-chain.
    #[error("perp does not exist: {perp_id}")]
    PerpNotFound { perp_id: B256 },

    /// The position does not exist on-chain.
    #[error("position does not exist: id={pos_id}")]
    PositionNotFound { pos_id: U256 },

    /// A required module is not registered.
    #[error("module not registered: {module}")]
    ModuleNotRegistered { module: String },

    // ── Transparent library error conversions ────────────────────────
    /// Alloy contract call or ABI error.
    #[error(transparent)]
    AlloyContract(#[from] alloy::contract::Error),

    /// Alloy transport (RPC) error.
    #[error(transparent)]
    AlloyTransport(#[from] alloy::transports::TransportError),

    /// JSON serialization / deserialization error.
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

/// Convenience alias used throughout the SDK.
pub type Result<T> = std::result::Result<T, PerpCityError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_error_converts() {
        let json_err = serde_json::from_str::<String>("not valid json").unwrap_err();
        let err: PerpCityError = json_err.into();
        assert!(matches!(err, PerpCityError::Serde(_)));
    }

    #[test]
    fn error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PerpCityError>();
    }
}
