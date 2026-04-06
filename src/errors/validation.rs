//! Input validation errors.
//!
//! Shared across `math`, `convert`, `client/trades`, and `client/queries`
//! — anywhere user-supplied values are checked before hitting the chain.

use thiserror::Error;

/// Errors from validating user-supplied parameters.
#[derive(Error, Debug)]
pub enum ValidationError {
    /// Price is not positive, not finite, or outside protocol bounds.
    #[error("invalid price: {reason}")]
    InvalidPrice {
        /// What was wrong with the price.
        reason: String,
    },

    /// Margin does not meet the minimum opening requirement.
    #[error("invalid margin: {reason}")]
    InvalidMargin {
        /// What was wrong with the margin.
        reason: String,
    },

    /// Leverage is outside the allowed range.
    #[error("invalid leverage: {reason}")]
    InvalidLeverage {
        /// What was wrong with the leverage.
        reason: String,
    },

    /// Tick range violates protocol bounds or spacing.
    #[error("invalid tick range: lower={lower}, upper={upper}")]
    InvalidTickRange {
        /// Lower tick.
        lower: i32,
        /// Upper tick.
        upper: i32,
    },

    /// Margin ratio is outside the allowed window.
    #[error("invalid margin ratio: {value} (must be in [{min}, {max}])")]
    InvalidMarginRatio {
        /// The supplied margin ratio.
        value: u32,
        /// Minimum allowed.
        min: u32,
        /// Maximum allowed.
        max: u32,
    },

    /// An arithmetic operation overflowed.
    #[error("overflow: {context}")]
    Overflow {
        /// Description of the overflow.
        context: String,
    },

    /// ABI decoding of on-chain return data failed.
    #[error("decode failed: {context}")]
    DecodeFailed {
        /// What failed to decode.
        context: String,
    },

    /// A configuration value is invalid or missing.
    #[error("invalid config: {reason}")]
    InvalidConfig {
        /// What was wrong.
        reason: String,
    },
}
