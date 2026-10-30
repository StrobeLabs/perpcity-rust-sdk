//! Staged interfaces for contracts commit 5141c2f. No live deployment is implied.

use alloy::sol;

sol! {
    struct OperationsRates {
        int88 fundingPerDay;
        uint64 longUtilFeePerDay;
        uint64 shortUtilFeePerDay;
        uint40 lastTouch;
        uint256 lastMark;
    }

    struct OperationsModules {
        address beacon;
        address fees;
        address funding;
        address marginRatios;
        address priceImpact;
        address pricing;
    }

    struct OperationsPosition {
        int256 delta;
        uint128 margin;
        int256 lastCumlFundingX96;
    }

    struct OperationsTaker {
        uint256 lastUtilPaymentsX96;
        uint256 lastReserveUtilPaymentsX96;
    }

    struct OperationsFunding {
        int256 belowX96;
        int256 withinInventoryX192;
    }

    struct OperationsMaker {
        int24 tickLower;
        int24 tickUpper;
        uint128 liquidity;
        uint256 lastLpFeeGrowthInsideX128;
        OperationsFunding lastCumlFunding;
        int256 lastFundingCorrectionX128;
    }

    #[sol(rpc)]
    interface OperationsPerp {
        function owner() external view returns (address);
        function governor() external view returns (address);
        function rates() external view returns (OperationsRates memory);
        function modules() external view returns (OperationsModules memory);
        function tvlCap() external view returns (uint256);
        function accountedTvl() external view returns (uint256);
        function withdrawalController() external view returns (address);
        function positionOwner(uint256 id) external view returns (address);
        function positions(uint256 id) external view returns (OperationsPosition memory);
        function makerDetails(uint256 id) external view returns (OperationsMaker memory);
        function takerDetails(uint256 id) external view returns (OperationsTaker memory);
        function touch() external;
        function backstopMaker(uint256 id, uint128 margin, address recipient, uint64 deadline) external;
        function backstopTaker(uint256 id, uint128 margin, address recipient, uint64 deadline) external;
        function setTvlCap(uint256 cap) external;
    }
    #[sol(rpc)]
    interface OperationsFees {
        function owner() external view returns (address);
        function targetUtilization() external view returns (uint256);
        function slopeBeforeTarget() external view returns (uint256);
        function slopeAfterTarget() external view returns (uint256);
        function setUtilFeeCurve(uint256 target, uint256 before, uint256 after) external;
    }
    #[sol(rpc)]
    interface OperationsController {
        function perp() external view returns (address);
        function challengeWindow() external view returns (uint48);
        function releasesPaused() external view returns (bool);
        function rateLimit() external view returns (uint128 capacity, uint128 available, uint128 refillPerSecond, uint48 lastUpdated);
        function totalReserved() external view returns (uint256);
        function nextClaimId() external view returns (uint256);
        function claims(uint256 id) external view returns (uint128 originalAmount, uint128 currentAmount, address recipient, uint48 createdAt, uint8 status, bool paused);
        function pauseReleases() external;
        function pauseClaim(uint256 claim) external;
        function lowerRateLimit(uint128 capacity, uint128 refill) external;
        function increaseChallengeWindow(uint48 seconds_) external;
        function reduceClaim(uint256 claim, uint128 newAmount, bytes32 evidence) external;
        function unpauseReleases() external;
        function unpauseClaim(uint256 claim) external;
    }
}
