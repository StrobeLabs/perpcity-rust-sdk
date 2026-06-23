//! Contract error selector decoding.
//!
//! Maps 4-byte error selectors from the PerpCity contracts to human-readable
//! names. The selector values are taken from the generated `sol!` bindings
//! (`Perp` / `PerpFactory` error types), so they cannot drift from the frozen
//! `Errors.sol`. The standard Solidity `Error(string)` and `Panic(uint256)`
//! selectors are included as well.

use alloy::sol_types::SolError;

use crate::contracts::{Perp, PerpFactory};

/// `Error(string)` — the standard Solidity revert-string selector.
const ERROR_STRING_SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];
/// `Panic(uint256)` — the standard Solidity panic selector.
const PANIC_SELECTOR: [u8; 4] = [0x4e, 0x48, 0x7b, 0x71];

/// Look up the name for a 4-byte error selector.
///
/// Driven off the generated binding selectors so the mapping stays in sync
/// with the frozen contracts.
fn name_for_selector(selector: [u8; 4]) -> Option<&'static str> {
    // Perp errors (libraries/Errors.sol).
    let table: &[([u8; 4], &str)] = &[
        (Perp::Abdicated::SELECTOR, "Abdicated"),
        (Perp::ZeroDelta::SELECTOR, "ZeroDelta"),
        (Perp::MinAmtUnmet::SELECTOR, "MinAmtUnmet"),
        (Perp::MarginTooLow::SELECTOR, "MarginTooLow"),
        (Perp::NoSystemFunds::SELECTOR, "NoSystemFunds"),
        (Perp::ZeroLiquidity::SELECTOR, "ZeroLiquidity"),
        (Perp::MaxAmtExceeded::SELECTOR, "MaxAmtExceeded"),
        (Perp::NegativeEquity::SELECTOR, "NegativeEquity"),
        (Perp::NegativeMargin::SELECTOR, "NegativeMargin"),
        (Perp::NotPoolManager::SELECTOR, "NotPoolManager"),
        (Perp::NotLiquidatable::SELECTOR, "NotLiquidatable"),
        (Perp::NonMakerPosition::SELECTOR, "NonMakerPosition"),
        (Perp::NonTakerPosition::SELECTOR, "NonTakerPosition"),
        (Perp::TicksOutOfBounds::SELECTOR, "TicksOutOfBounds"),
        (Perp::DataNotTimelocked::SELECTOR, "DataNotTimelocked"),
        (Perp::MarginRatioTooLow::SELECTOR, "MarginRatioTooLow"),
        (Perp::DataAlreadyPending::SELECTOR, "DataAlreadyPending"),
        (Perp::PriceImpactTooHigh::SELECTOR, "PriceImpactTooHigh"),
        (Perp::TimelockNotExpired::SELECTOR, "TimelockNotExpired"),
        (Perp::UnauthorizedCaller::SELECTOR, "UnauthorizedCaller"),
        (Perp::PositionDoesNotExist::SELECTOR, "PositionDoesNotExist"),
        (
            Perp::LongUtilizationExceeded::SELECTOR,
            "LongUtilizationExceeded",
        ),
        (
            Perp::ShortUtilizationExceeded::SELECTOR,
            "ShortUtilizationExceeded",
        ),
        (
            Perp::InsufficientLiquidityToFill::SELECTOR,
            "InsufficientLiquidityToFill",
        ),
        // PerpFactory errors (IPerpFactory). NotPoolManager is shared with Perp.
        (
            PerpFactory::StartingPriceTooLow::SELECTOR,
            "StartingPriceTooLow",
        ),
        (
            PerpFactory::StartingPriceTooHigh::SELECTOR,
            "StartingPriceTooHigh",
        ),
        (PerpFactory::EmaWindowTooLow::SELECTOR, "EmaWindowTooLow"),
        // Solady SafeTransferLib (used throughout the contracts for USDC moves).
        ([0x79, 0x39, 0xf4, 0x24], "TransferFromFailed"),
        ([0x90, 0xb8, 0xec, 0x18], "TransferFailed"),
        ([0x3e, 0x3f, 0x8f, 0x73], "ApproveFailed"),
        ([0xb1, 0x2d, 0x13, 0xeb], "ETHTransferFailed"),
        // Standard Solidity errors.
        (ERROR_STRING_SELECTOR, "Error"),
        (PANIC_SELECTOR, "Panic"),
    ];

    table
        .iter()
        .find(|(s, _)| *s == selector)
        .map(|(_, name)| *name)
}

/// Decode a hex-encoded revert data string into an error name.
///
/// `hex_data` must include the `0x` prefix and be at least 10 characters
/// (4-byte selector). Returns `(error_name, selector)`. Unrecognized
/// selectors decode to `"UnknownContractError"`.
///
/// # Examples
///
/// ```
/// use perpcity_sdk::errors::decode::decode_revert_data;
///
/// // Standard Solidity `Error(string)` selector.
/// let (name, sel) = decode_revert_data("0x08c379a0").unwrap();
/// assert_eq!(name, "Error");
/// assert_eq!(sel, "0x08c379a0");
/// ```
pub fn decode_revert_data(hex_data: &str) -> Option<(String, String)> {
    if hex_data.len() < 10 {
        return None;
    }

    let selector_str = &hex_data[0..10];

    // Parse the 8 hex digits after `0x` into a 4-byte selector.
    let mut selector = [0u8; 4];
    for (i, byte) in selector.iter_mut().enumerate() {
        let start = 2 + i * 2;
        *byte = u8::from_str_radix(&hex_data[start..start + 2], 16).ok()?;
    }

    let name = name_for_selector(selector).unwrap_or("UnknownContractError");
    Some((name.into(), selector_str.into()))
}

/// Try to extract revert data from an Alloy error string.
///
/// Scans the error message for hex data following `"data: \""` (the format
/// Alloy uses for RPC error code 3 responses). Returns
/// `(error_name, selector, full_revert_data)`.
pub fn try_extract_revert(error: &str) -> Option<(String, String, Option<String>)> {
    // Alloy format: `execution reverted, data: "0x...."`
    let data = if let Some(idx) = error.find("data: \"0x") {
        let start = idx + "data: \"".len();
        let end = error[start..].find('"').map(|i| start + i)?;
        &error[start..end]
    } else if let Some(idx) = error.find("data: 0x") {
        // Some RPC providers omit the quotes
        let start = idx + "data: ".len();
        let end = error[start..]
            .find(|c: char| !c.is_ascii_hexdigit() && c != 'x')
            .map(|i| start + i)
            .unwrap_or(error.len());
        &error[start..end]
    } else {
        return None;
    };

    if data.len() < 10 {
        return None;
    }

    let (name, selector) = decode_revert_data(data)?;
    let full_data = if data.len() > 10 {
        Some(data.to_string())
    } else {
        None
    };

    Some((name, selector, full_data))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Format a 4-byte selector as a `0x`-prefixed hex string.
    fn sel_hex(selector: [u8; 4]) -> String {
        format!("0x{}", alloy::primitives::hex::encode(selector))
    }

    #[test]
    fn decode_binding_selectors() {
        // Driven off the generated bindings so this can't drift from Errors.sol.
        let (name, _) = decode_revert_data(&sel_hex(Perp::ZeroDelta::SELECTOR)).unwrap();
        assert_eq!(name, "ZeroDelta");

        let (name, _) = decode_revert_data(&sel_hex(Perp::PositionDoesNotExist::SELECTOR)).unwrap();
        assert_eq!(name, "PositionDoesNotExist");

        let (name, _) =
            decode_revert_data(&sel_hex(PerpFactory::StartingPriceTooLow::SELECTOR)).unwrap();
        assert_eq!(name, "StartingPriceTooLow");
    }

    #[test]
    fn decode_standard_solidity_errors() {
        assert_eq!(decode_revert_data("0x08c379a0").unwrap().0, "Error");
        assert_eq!(decode_revert_data("0x4e487b71").unwrap().0, "Panic");
    }

    #[test]
    fn decode_unknown_selector() {
        let (name, sel) = decode_revert_data("0xdeadbeef").unwrap();
        assert_eq!(name, "UnknownContractError");
        assert_eq!(sel, "0xdeadbeef");
    }

    #[test]
    fn decode_too_short() {
        assert!(decode_revert_data("0xbeef").is_none());
        assert!(decode_revert_data("").is_none());
    }

    #[test]
    fn extract_revert_from_alloy_error() {
        let sel = sel_hex(Perp::ZeroDelta::SELECTOR);
        let error = format!(
            r#"server returned an error response: error code 3: execution reverted, data: "{sel}""#
        );
        let (name, selector, data) = try_extract_revert(&error).unwrap();
        assert_eq!(name, "ZeroDelta");
        assert_eq!(selector, sel);
        assert!(data.is_none()); // no extra params beyond selector
    }

    #[test]
    fn extract_revert_with_params() {
        // A selector followed by 32 bytes of ABI-encoded params.
        let sel = sel_hex(Perp::MarginTooLow::SELECTOR);
        let params = "0".repeat(64);
        let error = format!(r#"execution reverted, data: "{sel}{params}""#);
        let (name, selector, data) = try_extract_revert(&error).unwrap();
        assert_eq!(name, "MarginTooLow");
        assert_eq!(selector, sel);
        assert!(data.is_some());
    }

    #[test]
    fn extract_no_revert_data() {
        let error = "server returned an error response: error code -32003: insufficient funds";
        assert!(try_extract_revert(error).is_none());
    }
}
