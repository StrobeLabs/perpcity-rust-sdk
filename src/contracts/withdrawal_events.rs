//! Staged controller event ABI from contracts 5141c2f902d839591f694422fbe3f34745e9fb35.
//!
//! Use this reader only for reviewed deployments of this controller version.
//! These bindings do not establish support on deployed beta markets.

use alloy::sol;

sol! {
    interface WithdrawalEvents {
        event ChallengeWindowSet(uint48 challengeWindow);
        event ClaimCreated(uint256 indexed claimId, address indexed recipient, uint128 amount);
        event ClaimPaused(uint256 indexed claimId);
        event ClaimReduced(uint256 indexed claimId, uint128 oldAmount, uint128 newAmount, bytes32 evidenceHash, address guardian);
        event ClaimReleased(uint256 indexed claimId, address indexed recipient, uint128 amount);
        event ClaimUnpaused(uint256 indexed claimId);
        event ControllerInitialized(address indexed perp, uint128 capacity, uint128 refillPerSecond, uint48 challengeWindow);
        event InstantPayout(address indexed recipient, uint128 amount, uint128 availableAfter);
        event RateLimitSet(uint128 capacity, uint128 refillPerSecond);
        event ReleasesPaused();
        event ReleasesUnpaused();
    }
}
