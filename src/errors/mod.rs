//! Error types for the PerpCity SDK.
//!
//! Errors are organized by module boundary:
//!
//! - [`TransactionError`] — transaction lifecycle (simulation, signing,
//!   broadcasting, receipt polling, gas resolution)
//! - [`ValidationError`] — input validation (prices, margins, ticks,
//!   leverage, arithmetic overflow)
//! - [`ContractError`] — on-chain protocol state (perps, positions,
//!   modules, events, quotes)
//!
//! The top-level [`PerpCityError`] composes all three via `#[from]`
//! conversions, so module-internal code can return specific error types
//! with `?` and callers receive a unified enum.

pub mod contract;
pub mod decode;
pub mod transaction;
pub mod validation;

pub use contract::ContractError;
pub use transaction::TransactionError;
pub use validation::ValidationError;

use thiserror::Error;

/// Central error type for the PerpCity SDK.
///
/// Composed from per-module error types. Use `#[from]` conversions to
/// return module-specific errors with `?`:
///
/// ```rust,ignore
/// // Inside client/transactions.rs:
/// Err(TransactionError::GasUnavailable { reason: "..." }.into())
/// // Automatically converts to PerpCityError::Transaction(...)
/// ```
///
/// Callers can pattern-match on the variant to determine which layer
/// failed and decide how to handle it.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum PerpCityError {
    /// Transaction lifecycle error (simulation, signing, gas, pipeline).
    #[error(transparent)]
    Transaction(#[from] TransactionError),

    /// Input validation error (prices, margins, ticks, leverage).
    #[error(transparent)]
    Validation(#[from] ValidationError),

    /// On-chain protocol state error (perps, positions, events, quotes).
    #[error(transparent)]
    Contract(#[from] ContractError),

    /// Alloy RPC / transport error.
    #[error(transparent)]
    Rpc(#[from] alloy::transports::TransportError),

    /// Alloy contract ABI error.
    #[error(transparent)]
    Abi(#[from] alloy::contract::Error),

    /// JSON serialization / deserialization error.
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

impl PerpCityError {
    /// Returns `true` if the error indicates a pre-flight simulation
    /// detected a contract revert (no gas was burned).
    pub fn is_simulation_revert(&self) -> bool {
        matches!(
            self,
            Self::Transaction(TransactionError::SimulationReverted { .. })
        )
    }

    /// Returns `true` if the error is likely transient and worth retrying
    /// (RPC errors, gas unavailable, etc.).
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Rpc(_)
                | Self::Transaction(TransactionError::GasUnavailable { .. })
                | Self::Transaction(TransactionError::ReceiptTimeout { .. })
        )
    }
}

/// Convenience alias used throughout the SDK.
pub type Result<T> = std::result::Result<T, PerpCityError>;
