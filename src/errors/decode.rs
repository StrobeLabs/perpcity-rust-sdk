//! Contract error selector decoding.
//!
//! Maps 4-byte error selectors from PerpCity contracts (and Solady
//! SafeTransferLib) to human-readable names. Ported from the-beaconator's
//! `ContractErrorDecoder`.

/// Decode a hex-encoded revert data string into an error name.
///
/// `hex_data` must include the `0x` prefix and be at least 10 characters
/// (4-byte selector). Returns `(error_name, selector)`.
///
/// # Examples
///
/// ```
/// use perpcity_sdk::errors::decode::decode_revert_data;
///
/// let (name, sel) = decode_revert_data("0xbcffc83f").unwrap();
/// assert_eq!(name, "InvalidMarginRatio");
/// assert_eq!(sel, "0xbcffc83f");
/// ```
pub fn decode_revert_data(hex_data: &str) -> Option<(String, String)> {
    if hex_data.len() < 10 {
        return None;
    }

    let selector = &hex_data[0..10];
    let name = match selector {
        "0x10074548" => "ZeroLiquidity",
        "0x96bafbfd" => "ZeroNotional",
        "0xd6acf910" => "TicksOutOfBounds",
        "0x3a29e65e" => "InvalidMargin",
        "0x8acc6d7f" => "InvalidMarginDelta",
        "0x48f5c3ed" => "InvalidCaller",
        "0xc7d26d72" => "PositionLocked",
        "0x6f0f5899" => "ZeroDelta",
        "0xbcffc83f" => "InvalidMarginRatio",
        "0x2872ed04" => "FeesNotRegistered",
        "0x3eea589d" => "MarginRatiosNotRegistered",
        "0xd9f0aeaf" => "LockupPeriodNotRegistered",
        "0x5140209c" => "SqrtPriceImpactLimitNotRegistered",
        "0xfc5bee12" => "FeeTooLarge",
        "0xc3f6bb4e" => "MakerNotAllowed",
        "0x7884e2a9" => "BeaconNotRegistered",
        "0x232ad152" => "PerpDoesNotExist",
        "0x1d8648bc" => "StartingSqrtPriceTooLow",
        "0x0947cb52" => "StartingSqrtPriceTooHigh",
        "0x67cf2eaa" => "CouldNotFullyFill",
        "0x24775e06" => "SafeCastOverflow",
        "0x7939f424" => "TransferFromFailed",
        _ => return Some(("UnknownContractError".into(), selector.into())),
    };

    Some((name.into(), selector.into()))
}

/// Try to extract revert data from an Alloy error string.
///
/// Scans the error message for hex data following `"data: \""` (the format
/// Alloy uses for RPC error code 3 responses). Returns
/// `(error_name, selector, full_revert_data)`.
pub fn try_extract_revert(error: &str) -> Option<(String, String, Option<String>)> {
    // Alloy format: `execution reverted, data: "0xbcffc83f"`
    // or: `execution reverted, data: "0x7939f424"`
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

    #[test]
    fn decode_known_selectors() {
        let cases = [
            ("0xbcffc83f", "InvalidMarginRatio"),
            ("0x7939f424", "TransferFromFailed"),
            ("0x10074548", "ZeroLiquidity"),
            ("0x67cf2eaa", "CouldNotFullyFill"),
            ("0x3a29e65e", "InvalidMargin"),
        ];
        for (hex, expected_name) in cases {
            let (name, sel) = decode_revert_data(hex).unwrap();
            assert_eq!(name, expected_name);
            assert_eq!(sel, hex);
        }
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
        let error = r#"server returned an error response: error code 3: execution reverted, data: "0xbcffc83f""#;
        let (name, selector, data) = try_extract_revert(error).unwrap();
        assert_eq!(name, "InvalidMarginRatio");
        assert_eq!(selector, "0xbcffc83f");
        assert!(data.is_none()); // no extra params beyond selector
    }

    #[test]
    fn extract_revert_with_params() {
        let error = r#"execution reverted, data: "0x24775e060000000000000000000000000000000000000000000000000000000000000042""#;
        let (name, selector, data) = try_extract_revert(error).unwrap();
        assert_eq!(name, "SafeCastOverflow");
        assert_eq!(selector, "0x24775e06");
        assert!(data.is_some());
    }

    #[test]
    fn extract_no_revert_data() {
        let error = "server returned an error response: error code -32003: insufficient funds";
        assert!(try_extract_revert(error).is_none());
    }
}
