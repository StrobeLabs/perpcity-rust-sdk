//! Contract error selector decoding.
//!
//! Maps 4-byte error selectors from the PerpCity contracts to human-readable
//! names, and says what a typed revert means for a retry ([`RevertClass`]).
//! The selector values are taken from the generated `sol!` bindings
//! (`Perp`, `PerpV022`, `PerpFactory` and the `v1` error types), so they
//! cannot drift from the contracts' `Errors.sol`. The standard Solidity
//! `Error(string)` and `Panic(uint256)` selectors are included as well.

use alloy::sol_types::SolError;

use crate::contracts::{Perp, PerpFactory, PerpV022, v1};

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
        (
            PerpFactory::InvalidPerpImplementation::SELECTOR,
            "InvalidPerpImplementation",
        ),
        (PerpFactory::NotProtocolOwner::SELECTOR, "NotProtocolOwner"),
        // v0.2.2-upgradeable: Errors.sol additions, the guard hook, the proxy.
        (PerpV022::NoSurplus::SELECTOR, "NoSurplus"),
        (PerpV022::ZeroAddress::SELECTOR, "ZeroAddress"),
        (
            PerpV022::UnauthorizedPoolAction::SELECTOR,
            "UnauthorizedPoolAction",
        ),
        (
            PerpV022::ERC1967InvalidImplementation::SELECTOR,
            "ERC1967InvalidImplementation",
        ),
        (PerpV022::ERC1967NonPayable::SELECTOR, "ERC1967NonPayable"),
        (
            PerpV022::UUPSUnauthorizedCallContext::SELECTOR,
            "UUPSUnauthorizedCallContext",
        ),
        (
            PerpV022::UUPSUnsupportedProxiableUUID::SELECTOR,
            "UUPSUnsupportedProxiableUUID",
        ),
        (
            PerpV022::InvalidInitialization::SELECTOR,
            "InvalidInitialization",
        ),
        // Solady ERC721, inherited by both builds.
        (Perp::TokenDoesNotExist::SELECTOR, "TokenDoesNotExist"),
        (Perp::NotOwnerNorApproved::SELECTOR, "NotOwnerNorApproved"),
        (
            Perp::TransferToZeroAddress::SELECTOR,
            "TransferToZeroAddress",
        ),
        (
            Perp::TransferFromIncorrectOwner::SELECTOR,
            "TransferFromIncorrectOwner",
        ),
        (
            Perp::TransferToNonERC721ReceiverImplementer::SELECTOR,
            "TransferToNonERC721ReceiverImplementer",
        ),
        // v1.0.0: the 30 errors only in its Errors.sol (`EmaWindowTooLow`,
        // before a factory error, is the row above); the rest are above.
        (
            v1::Perp::AggregateSwapFeesTooHigh::SELECTOR,
            "AggregateSwapFeesTooHigh",
        ),
        (v1::Perp::AlreadyInitialized::SELECTOR, "AlreadyInitialized"),
        (v1::Perp::AmountNotDecreased::SELECTOR, "AmountNotDecreased"),
        (
            v1::Perp::BeaconIndexUnreadable::SELECTOR,
            "BeaconIndexUnreadable",
        ),
        (v1::Perp::ClaimIsPaused::SELECTOR, "ClaimIsPaused"),
        (v1::Perp::ClaimNotPaused::SELECTOR, "ClaimNotPaused"),
        (v1::Perp::ClaimNotPending::SELECTOR, "ClaimNotPending"),
        (v1::Perp::ClaimNotReady::SELECTOR, "ClaimNotReady"),
        (v1::Perp::DeadlineExpired::SELECTOR, "DeadlineExpired"),
        (
            v1::Perp::DonationsRestricted::SELECTOR,
            "DonationsRestricted",
        ),
        (v1::Perp::HealthNotImproved::SELECTOR, "HealthNotImproved"),
        (v1::Perp::IdenticalTicks::SELECTOR, "IdenticalTicks"),
        (
            v1::Perp::InvalidCallbackAction::SELECTOR,
            "InvalidCallbackAction",
        ),
        (
            v1::Perp::LongCloseCapacityExceeded::SELECTOR,
            "LongCloseCapacityExceeded",
        ),
        (v1::Perp::MarginTooHigh::SELECTOR, "MarginTooHigh"),
        (
            v1::Perp::MaxTotalFeeExceeded::SELECTOR,
            "MaxTotalFeeExceeded",
        ),
        (v1::Perp::NotGovernor::SELECTOR, "NotGovernor"),
        (v1::Perp::NotGuardian::SELECTOR, "NotGuardian"),
        (v1::Perp::NotPerp::SELECTOR, "NotPerp"),
        (v1::Perp::PriceOutsideRange::SELECTOR, "PriceOutsideRange"),
        (v1::Perp::ReduceOnlyViolated::SELECTOR, "ReduceOnlyViolated"),
        (v1::Perp::ReleasesArePaused::SELECTOR, "ReleasesArePaused"),
        (
            v1::Perp::ShortCloseCapacityExceeded::SELECTOR,
            "ShortCloseCapacityExceeded",
        ),
        (v1::Perp::StaleBeaconIndex::SELECTOR, "StaleBeaconIndex"),
        (v1::Perp::TvlCapExceeded::SELECTOR, "TvlCapExceeded"),
        (v1::Perp::WindowNotIncreased::SELECTOR, "WindowNotIncreased"),
        (v1::Perp::ZeroAmount::SELECTOR, "ZeroAmount"),
        (v1::Perp::ZeroCapacity::SELECTOR, "ZeroCapacity"),
        (v1::Perp::ZeroWindow::SELECTOR, "ZeroWindow"),
        // v1.0.0: the proxy and ownership bases, and the factory.
        (v1::Perp::AddressEmptyCode::SELECTOR, "AddressEmptyCode"),
        (v1::Perp::FailedCall::SELECTOR, "FailedCall"),
        (v1::Perp::NotInitializing::SELECTOR, "NotInitializing"),
        (
            v1::Perp::NewOwnerIsZeroAddress::SELECTOR,
            "NewOwnerIsZeroAddress",
        ),
        (v1::Perp::NoHandoverRequest::SELECTOR, "NoHandoverRequest"),
        (v1::Perp::Unauthorized::SELECTOR, "Unauthorized"),
        (v1::PerpFactory::NotAContract::SELECTOR, "NotAContract"),
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

/// What a typed revert says about trying the call again.
///
/// Five `v1.0.0` errors are not an [`Answer`](Self::Answer): none of them
/// exists in an earlier build, so every revert of a legacy or `v0.2.2`
/// market stays an answer. The class comes from the selector alone; what
/// a caller does with it also depends on the call that reverted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RevertClass {
    /// The contract's answer to this call on this state: the same call
    /// gets it again. Not transient.
    Answer,
    /// A later attempt can pass with no change to the caller's intent, but
    /// not with the same calldata or not at once. Transient.
    ///
    /// - `DeadlineExpired`: the call ran after its `deadline`. The same
    ///   calldata always fails again: a retry builds the call again with a
    ///   new deadline.
    /// - `StaleBeaconIndex`: the beacon's last update is older than the
    ///   market's `latency()`, or is dated after the block. The call can
    ///   pass after an update whose time the market accepts.
    Transient,
    /// A beacon gives no usable index: `BeaconIndexUnreadable` (its
    /// `index()` call failed or returned short data) or `PriceOutsideRange`
    /// (the index is outside `[2^96 / 1e6, 2^96 * 1e6]`). Not transient.
    ///
    /// From a trade, a liquidation or a `touch`, the beacon is the market's
    /// own: every call that reads the index fails the same way until the
    /// beacon is fixed, whatever the position or the size, so hold the
    /// market, not the position. It is not a price-impact refusal: a
    /// smaller trade gets the same answer. From `setBeacon`, `createPerp`
    /// or `readBeaconIndex`, the beacon is the one the call names, and the
    /// fault is that beacon's: the market's own beacon is not proved bad.
    BeaconFault,
    /// `TvlCapExceeded`: the USDC this call brings in (margin, or a
    /// donation) would take the market's accounted TVL past its `tvlCap()`.
    /// A smaller amount can still fit; this one fails until the governor
    /// raises the cap or the TVL falls (the owner can only lower the cap).
    /// Hold the add, not the position. Not transient.
    Hold,
}

impl RevertClass {
    /// Whether a retry of the call can help without a change to the
    /// market, its beacon or its owner's cap; only [`Self::Transient`].
    pub fn is_transient(self) -> bool {
        matches!(self, Self::Transient)
    }
}

/// The [`RevertClass`] of a 4-byte error selector. A selector that is not
/// one of the five `v1.0.0` errors named in [`RevertClass`], known or not,
/// is an [`RevertClass::Answer`].
///
/// ```
/// use alloy::sol_types::SolError;
/// use perpcity_sdk::contracts::v1::Perp;
/// use perpcity_sdk::errors::decode::{RevertClass, revert_class};
///
/// assert_eq!(revert_class(Perp::StaleBeaconIndex::SELECTOR), RevertClass::Transient);
/// assert_eq!(revert_class(Perp::NotLiquidatable::SELECTOR), RevertClass::Answer);
/// ```
pub fn revert_class(selector: [u8; 4]) -> RevertClass {
    match selector {
        v1::Perp::StaleBeaconIndex::SELECTOR | v1::Perp::DeadlineExpired::SELECTOR => {
            RevertClass::Transient
        }
        v1::Perp::BeaconIndexUnreadable::SELECTOR | v1::Perp::PriceOutsideRange::SELECTOR => {
            RevertClass::BeaconFault
        }
        v1::Perp::TvlCapExceeded::SELECTOR => RevertClass::Hold,
        _ => RevertClass::Answer,
    }
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
    let (name, _) = decode_revert(hex_data)?;
    Some((name, hex_data[0..10].into()))
}

/// [`decode_revert_data`] with the selector kept raw, for callers that
/// match on it typed rather than print it.
pub(crate) fn decode_revert(hex_data: &str) -> Option<(String, [u8; 4])> {
    let selector = parse_selector(hex_data)?;
    let name = name_for_selector(selector)
        .map(String::from)
        // Keep the selector visible: distinct unknown errors must stay
        // distinguishable instead of collapsing into one opaque name.
        .unwrap_or_else(|| format!("UnknownContractError({})", &hex_data[0..10]));
    Some((name, selector))
}

/// Parse the 8 hex digits after `0x` into the raw 4-byte selector.
pub(crate) fn parse_selector(hex_data: &str) -> Option<[u8; 4]> {
    if hex_data.len() < 10 || !hex_data.starts_with("0x") {
        return None;
    }
    let mut selector = [0u8; 4];
    for (i, byte) in selector.iter_mut().enumerate() {
        let start = 2 + i * 2;
        *byte = u8::from_str_radix(&hex_data[start..start + 2], 16).ok()?;
    }
    Some(selector)
}

/// Try to extract revert data from an Alloy error string.
///
/// Scans the error message for hex data following `"data: \""` (the format
/// Alloy uses for RPC error code 3 responses). Returns
/// `(error_name, raw_selector, full_revert_data)`.
pub fn try_extract_revert(error: &str) -> Option<(String, [u8; 4], Option<String>)> {
    // Alloy format: `execution reverted, data: "0x...."`
    let data = if let Some(idx) = error.find("data: \"0x") {
        let start = idx + "data: \"".len();
        let end = error[start..].find('"').map(|i| start + i)?;
        &error[start..end]
    } else {
        let idx = error.find("data: 0x")?;
        // Some RPC providers omit the quotes
        let start = idx + "data: ".len();
        let end = error[start..]
            .find(|c: char| !c.is_ascii_hexdigit() && c != 'x')
            .map(|i| start + i)
            .unwrap_or(error.len());
        &error[start..end]
    };

    if data.len() < 10 {
        return None;
    }

    let (name, selector) = decode_revert(data)?;
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

        let (name, _) = decode_revert_data("0xb7cc5070").unwrap();
        assert_eq!(name, "UnauthorizedPoolAction");
        let (name, _) = decode_revert_data("0xc0ef17d3").unwrap();
        assert_eq!(name, "NoSurplus");
        let (name, _) = decode_revert_data("0xa457695f").unwrap();
        assert_eq!(name, "InvalidPerpImplementation");
        let (name, _) = decode_revert_data("0xceea21b6").unwrap();
        assert_eq!(name, "TokenDoesNotExist");
    }

    #[test]
    fn decode_standard_solidity_errors() {
        assert_eq!(decode_revert_data("0x08c379a0").unwrap().0, "Error");
        assert_eq!(decode_revert_data("0x4e487b71").unwrap().0, "Panic");
    }

    #[test]
    fn decode_unknown_selector() {
        let (name, sel) = decode_revert_data("0xdeadbeef").unwrap();
        assert_eq!(name, "UnknownContractError(0xdeadbeef)");
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
        assert_eq!(selector, Perp::ZeroDelta::SELECTOR);
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
        assert_eq!(selector, Perp::MarginTooLow::SELECTOR);
        assert!(data.is_some());
    }

    /// The 30 errors that only `v1.0.0`'s `Errors.sol` has.
    const V1_ONLY: [&str; 30] = [
        "AggregateSwapFeesTooHigh",
        "AlreadyInitialized",
        "AmountNotDecreased",
        "BeaconIndexUnreadable",
        "ClaimIsPaused",
        "ClaimNotPaused",
        "ClaimNotPending",
        "ClaimNotReady",
        "DeadlineExpired",
        "DonationsRestricted",
        "EmaWindowTooLow",
        "HealthNotImproved",
        "IdenticalTicks",
        "InvalidCallbackAction",
        "LongCloseCapacityExceeded",
        "MarginTooHigh",
        "MaxTotalFeeExceeded",
        "NotGovernor",
        "NotGuardian",
        "NotPerp",
        "PriceOutsideRange",
        "ReduceOnlyViolated",
        "ReleasesArePaused",
        "ShortCloseCapacityExceeded",
        "StaleBeaconIndex",
        "TvlCapExceeded",
        "WindowNotIncreased",
        "ZeroAmount",
        "ZeroCapacity",
        "ZeroWindow",
    ];

    /// The errors of the `v1.0.0` proxy and ownership bases that no earlier
    /// binding has.
    const V1_BASES: [&str; 6] = [
        "AddressEmptyCode",
        "FailedCall",
        "NotInitializing",
        "NewOwnerIsZeroAddress",
        "NoHandoverRequest",
        "Unauthorized",
    ];

    /// Every selector of an earlier build's binding, with its name.
    fn earlier_errors() -> Vec<([u8; 4], &'static str)> {
        let mut all = Vec::new();
        for (selectors, names) in [
            (Perp::PerpErrors::SELECTORS, Perp::PerpErrors::VARIANT_NAMES),
            (
                PerpV022::PerpV022Errors::SELECTORS,
                PerpV022::PerpV022Errors::VARIANT_NAMES,
            ),
            (
                PerpFactory::PerpFactoryErrors::SELECTORS,
                PerpFactory::PerpFactoryErrors::VARIANT_NAMES,
            ),
        ] {
            all.extend(selectors.iter().copied().zip(names.iter().copied()));
        }
        all
    }

    #[test]
    fn every_v1_error_decodes_to_its_binding_name() {
        let bound = v1::Perp::PerpErrors::SELECTORS
            .iter()
            .chain(v1::PerpFactory::PerpFactoryErrors::SELECTORS)
            .chain(v1::WithdrawalController::WithdrawalControllerErrors::SELECTORS);
        for selector in bound {
            let name = v1::Perp::PerpErrors::name_by_selector(*selector)
                .or_else(|| v1::PerpFactory::PerpFactoryErrors::name_by_selector(*selector))
                .or_else(|| {
                    v1::WithdrawalController::WithdrawalControllerErrors::name_by_selector(
                        *selector,
                    )
                })
                .unwrap();
            assert_eq!(
                name_for_selector(*selector),
                Some(name),
                "{}",
                sel_hex(*selector)
            );
        }
    }

    /// The v1-only market errors are the 30 of row 62 plus the six of the
    /// bases: every other `v1.0.0` market error has an earlier market
    /// binding with its selector. (`EmaWindowTooLow` was a factory error
    /// before `v1.0.0` moved it into the market's `Errors.sol`.)
    #[test]
    fn v1_only_errors_are_the_thirty_and_the_bases() {
        let earlier: Vec<[u8; 4]> = Perp::PerpErrors::SELECTORS
            .iter()
            .chain(PerpV022::PerpV022Errors::SELECTORS)
            .copied()
            .collect();
        let mut only: Vec<&str> = v1::Perp::PerpErrors::SELECTORS
            .iter()
            .zip(v1::Perp::PerpErrors::VARIANT_NAMES)
            .filter(|(s, _)| !earlier.contains(s))
            .map(|(_, n)| *n)
            .collect();
        only.sort_unstable();
        let mut expected: Vec<&str> = V1_ONLY.iter().chain(&V1_BASES).copied().collect();
        expected.sort_unstable();
        assert_eq!(only, expected);
        for name in V1_ONLY.iter().chain(&V1_BASES) {
            let selector = v1::Perp::PerpErrors::SELECTORS[v1::Perp::PerpErrors::VARIANT_NAMES
                .iter()
                .position(|n| n == name)
                .unwrap()];
            assert_eq!(decode_revert_data(&sel_hex(selector)).unwrap().0, *name);
        }
    }

    /// Every selector of an earlier build decodes to the name it had and is
    /// an answer: the v1 rows change nothing for a legacy or v0.2.2 market.
    #[test]
    fn earlier_errors_keep_their_names_and_are_answers() {
        for (selector, name) in earlier_errors() {
            assert_eq!(
                name_for_selector(selector),
                Some(name),
                "{}",
                sel_hex(selector)
            );
            assert_eq!(revert_class(selector), RevertClass::Answer, "{name}");
        }
    }

    #[test]
    fn five_v1_errors_are_not_answers() {
        let classes = [
            (v1::Perp::StaleBeaconIndex::SELECTOR, RevertClass::Transient),
            (v1::Perp::DeadlineExpired::SELECTOR, RevertClass::Transient),
            (
                v1::Perp::BeaconIndexUnreadable::SELECTOR,
                RevertClass::BeaconFault,
            ),
            (
                v1::Perp::PriceOutsideRange::SELECTOR,
                RevertClass::BeaconFault,
            ),
            (v1::Perp::TvlCapExceeded::SELECTOR, RevertClass::Hold),
        ];
        for (selector, class) in classes {
            assert_eq!(revert_class(selector), class, "{}", sel_hex(selector));
        }
        let others = v1::Perp::PerpErrors::SELECTORS
            .iter()
            .chain(v1::PerpFactory::PerpFactoryErrors::SELECTORS)
            .chain(v1::WithdrawalController::WithdrawalControllerErrors::SELECTORS)
            .filter(|s| !classes.iter().any(|(c, _)| c == *s));
        for selector in others {
            assert_eq!(
                revert_class(*selector),
                RevertClass::Answer,
                "{}",
                sel_hex(*selector)
            );
        }
        assert_eq!(revert_class([0xde, 0xad, 0xbe, 0xef]), RevertClass::Answer);
        assert_eq!(revert_class(ERROR_STRING_SELECTOR), RevertClass::Answer);
        assert!(RevertClass::Transient.is_transient());
        assert!(!RevertClass::BeaconFault.is_transient());
        assert!(!RevertClass::Hold.is_transient());
        assert!(!RevertClass::Answer.is_transient());
    }

    #[test]
    fn extract_no_revert_data() {
        let error = "server returned an error response: error code -32003: insufficient funds";
        assert!(try_extract_revert(error).is_none());
    }
}
