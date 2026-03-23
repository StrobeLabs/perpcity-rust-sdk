//! On-chain contract bindings generated via Alloy's `sol!` macro.
//!
//! Structs, events, errors, and function selectors are derived directly from
//! `perpcity-contracts/src/interfaces/IPerpManager.sol` and related interfaces.
//! The `sol!` macro produces ABI-compatible Rust types automatically — no
//! manual encoding or decoding is needed.

use alloy::sol;

sol! {
    // ═══════════════════════════════════════════════════════════════════
    //  Uniswap V4 types used by PerpCity
    // ═══════════════════════════════════════════════════════════════════

    /// Identifies a Uniswap V4 pool (and PerpCity perp) by its constituent
    /// parameters.
    struct PoolKey {
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }

    /// Swap configuration for quoting through the Uniswap V4 pool.
    struct SwapConfig {
        PoolKey poolKey;
        bool isExactIn;
        bool zeroForOne;
        uint256 amountSpecified;
        uint160 sqrtPriceLimitX96;
        uint128 unspecifiedAmountLimit;
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Module interfaces
    // ═══════════════════════════════════════════════════════════════════

    /// Fee module — returns trading fees for a perp.
    #[sol(rpc)]
    interface IFees {
        function fees(PerpManager.PerpConfig calldata perp)
            external
            returns (uint24 cFee, uint24 insFee, uint24 lpFee);

        function utilizationFee(PoolKey calldata key)
            external
            returns (uint24 fee);

        function liquidationFee(PerpManager.PerpConfig calldata perp)
            external
            returns (uint24 fee);
    }

    /// Margin ratio module — returns min/max/liquidation margin ratios.
    #[sol(rpc)]
    interface IMarginRatios {
        struct MarginRatios {
            uint24 min;
            uint24 max;
            uint24 liq;
        }

        function marginRatios(PerpManager.PerpConfig calldata perp, bool isMaker)
            external
            returns (MarginRatios memory);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  PerpManager — the core protocol contract
    // ═══════════════════════════════════════════════════════════════════

    /// The PerpManager contract interface. Contains all structs, events,
    /// errors, and function signatures from IPerpManager.sol and
    /// PerpManager.sol.
    #[sol(rpc)]
    interface PerpManager {
        // ── Structs ──────────────────────────────────────────────

        struct PerpConfig {
            PoolKey key;
            address creator;
            address vault;
            address beacon;
            address fees;
            address marginRatios;
            address lockupPeriod;
            address sqrtPriceImpactLimit;
        }

        struct Position {
            bytes32 perpId;
            uint256 margin;
            int256 entryPerpDelta;
            int256 entryUsdDelta;
            int256 entryCumlFundingX96;
            uint256 entryCumlBadDebtX96;
            uint256 entryCumlUtilizationX96;
            IMarginRatios.MarginRatios marginRatios;
            MakerDetails makerDetails;
        }

        struct MakerDetails {
            uint32 unlockTimestamp;
            int24 tickLower;
            int24 tickUpper;
            uint128 liquidity;
            int256 entryCumlFundingBelowX96;
            int256 entryCumlFundingWithinX96;
            int256 entryCumlFundingDivSqrtPWithinX96;
        }

        struct CreatePerpParams {
            address beacon;
            address fees;
            address marginRatios;
            address lockupPeriod;
            address sqrtPriceImpactLimit;
            uint160 startingSqrtPriceX96;
        }

        struct OpenMakerPositionParams {
            address holder;
            uint128 margin;
            uint120 liquidity;
            int24 tickLower;
            int24 tickUpper;
            uint128 maxAmt0In;
            uint128 maxAmt1In;
        }

        struct OpenTakerPositionParams {
            address holder;
            bool isLong;
            uint128 margin;
            uint24 marginRatio;
            uint128 unspecifiedAmountLimit;
        }

        struct AdjustNotionalParams {
            uint256 posId;
            int256 usdDelta;
            uint128 perpLimit;
        }

        struct AdjustMarginParams {
            uint256 posId;
            int256 marginDelta;
        }

        struct ClosePositionParams {
            uint256 posId;
            uint128 minAmt0Out;
            uint128 minAmt1Out;
            uint128 maxAmt1In;
        }

        // ── Events ───────────────────────────────────────────────

        event PerpCreated(
            bytes32 perpId,
            address beacon,
            uint256 sqrtPriceX96,
            uint256 indexPriceX96
        );

        event PositionOpened(
            bytes32 perpId,
            uint256 sqrtPriceX96,
            uint256 longOI,
            uint256 shortOI,
            uint256 posId,
            bool isMaker,
            int256 perpDelta,
            int256 usdDelta,
            int24 tickLower,
            int24 tickUpper
        );

        event NotionalAdjusted(
            bytes32 perpId,
            uint256 sqrtPriceX96,
            uint256 longOI,
            uint256 shortOI,
            uint256 posId,
            int256 newPerpDelta,
            // Settlement details
            int256 swapPerpDelta,
            int256 swapUsdDelta,
            int256 funding,
            uint256 utilizationFee,
            uint256 adl,
            uint256 tradingFees
        );

        event MarginAdjusted(
            bytes32 perpId,
            uint256 posId,
            uint256 newMargin
        );

        event PositionClosed(
            bytes32 perpId,
            uint256 sqrtPriceX96,
            uint256 longOI,
            uint256 shortOI,
            uint256 posId,
            bool wasMaker,
            bool wasLiquidated,
            bool wasPartialClose,
            int256 exitPerpDelta,
            int256 exitUsdDelta,
            int24 tickLower,
            int24 tickUpper,
            // Settlement details
            int256 netUsdDelta,
            int256 funding,
            uint256 utilizationFee,
            uint256 adl,
            uint256 liquidationFee,
            int256 netMargin
        );

        event FeesModuleRegistered(address feesModule);
        event MarginRatiosModuleRegistered(address marginRatiosModule);
        event LockupPeriodModuleRegistered(address lockupPeriodModule);
        event SqrtPriceImpactLimitModuleRegistered(address sqrtPriceImpactLimitModule);

        // ── Errors ───────────────────────────────────────────────

        error ZeroLiquidity();
        error ZeroNotional();
        error TicksOutOfBounds();
        error InvalidMargin();
        error InvalidMarginDelta();
        error InvalidCaller();
        error PositionLocked();
        error ZeroDelta();
        error InvalidMarginRatio();
        error FeesNotRegistered();
        error MarginRatiosNotRegistered();
        error LockupPeriodNotRegistered();
        error SqrtPriceImpactLimitNotRegistered();
        error FeeTooLarge();
        error MakerNotAllowed();
        error BeaconNotRegistered();
        error PerpDoesNotExist();
        error StartingSqrtPriceTooLow();
        error StartingSqrtPriceTooHigh();
        error CouldNotFullyFill();
        error MarkTooFarFromIndex();

        // ── Perp functions ───────────────────────────────────────

        /// Creates a new perpetual market.
        function createPerp(CreatePerpParams calldata params)
            external
            returns (bytes32 perpId);

        /// Opens a maker (LP) position in a perp.
        function openMakerPos(bytes32 perpId, OpenMakerPositionParams calldata params)
            external
            returns (uint256 posId);

        /// Opens a taker (long/short) position in a perp.
        function openTakerPos(bytes32 perpId, OpenTakerPositionParams calldata params)
            external
            returns (uint256 posId);

        /// Adjusts the notional size of a taker position.
        function adjustNotional(AdjustNotionalParams calldata params) external;

        /// Adds or removes margin from an open position.
        function adjustMargin(AdjustMarginParams calldata params) external;

        /// Closes an open position (taker or maker).
        function closePosition(ClosePositionParams calldata params) external;

        /// Increases the oracle cardinality cap for a perp.
        function increaseCardinalityCap(bytes32 perpId, uint16 newCardinalityCap) external;

        // ── Module registration (owner only) ─────────────────────

        function registerFeesModule(address feesModule) external;
        function registerMarginRatiosModule(address marginRatiosModule) external;
        function registerLockupPeriodModule(address lockupPeriodModule) external;
        function registerSqrtPriceImpactLimitModule(address sqrtPriceImpactLimitModule) external;

        // ── Module registration queries ──────────────────────────

        function isFeesRegistered(address feesModule)
            external view returns (bool);
        function isMarginRatiosRegistered(address marginRatiosModule)
            external view returns (bool);
        function isLockupPeriodRegistered(address lockupPeriodModule)
            external view returns (bool);
        function isSqrtPriceImpactLimitRegistered(address sqrtPriceImpactLimitModule)
            external view returns (bool);

        // ── Protocol fee functions (owner only) ──────────────────

        function setProtocolFee(uint24 newProtocolFee) external;
        function collectProtocolFees(address recipient) external;

        // ── View functions ───────────────────────────────────────

        /// Returns the perp configuration for a given pool ID.
        function cfgs(bytes32 perpId) external view returns (PerpConfig memory);

        /// Returns a position by its NFT token ID.
        function positions(uint256 posId) external view returns (Position memory);

        /// Returns the next position ID to be minted.
        function nextPosId() external view returns (uint256);

        /// Returns the current protocol fee.
        function protocolFee() external view returns (uint24);

        /// Returns the oracle cardinality cap for a perp.
        function cardinalityCap(bytes32 perpId) external view returns (uint16);

        /// Returns the time-weighted average sqrtPrice, scaled by 2^96.
        function timeWeightedAvgSqrtPriceX96(bytes32 perpId, uint32 lookbackWindow)
            external view returns (uint256 twAvg);

        /// Returns funding rate per second, scaled by 2^96.
        function fundingPerSecondX96(bytes32 perpId) external view returns (int256);

        /// Returns utilization fee per second, scaled by 2^96.
        function utilFeePerSecX96(bytes32 perpId) external view returns (uint256);

        /// Returns the insurance fund balance for a perp.
        function insurance(bytes32 perpId) external view returns (uint128);

        /// Returns taker long and short open interest.
        function takerOpenInterest(bytes32 perpId)
            external view returns (uint128 longOI, uint128 shortOI);

        // ── Quote (simulation) functions ─────────────────────────

        /// Simulates opening a maker position.
        function quoteOpenMakerPosition(bytes32 perpId, OpenMakerPositionParams calldata params)
            external
            returns (bytes memory unexpectedReason, int256 perpDelta, int256 usdDelta);

        /// Simulates opening a taker position.
        function quoteOpenTakerPosition(bytes32 perpId, OpenTakerPositionParams calldata params)
            external
            returns (bytes memory unexpectedReason, int256 perpDelta, int256 usdDelta);

        /// Simulates closing a position — returns PnL, funding, and liquidation status.
        function quoteClosePosition(uint256 posId)
            external
            returns (
                bytes memory unexpectedReason,
                int256 pnl,
                int256 funding,
                int256 netMargin,
                bool wasLiquidated,
                uint256 notional
            );

        /// Simulates a swap in a perp's Uniswap V4 pool.
        function quoteSwap(
            bytes32 perpId,
            bool zeroForOne,
            bool isExactIn,
            uint256 amount,
            uint160 sqrtPriceLimitX96
        )
            external
            returns (bytes memory unexpectedReason, int256 perpDelta, int256 usdDelta);

        /// Simulates two sequential swaps.
        function quoteTwoSwaps(bytes32 perpId, SwapConfig memory first, SwapConfig memory second)
            external
            returns (
                bytes memory unexpectedReason,
                int256 perpDelta1,
                int256 usdDelta1,
                int256 perpDelta2,
                int256 usdDelta2
            );

        // ── ERC721 metadata ──────────────────────────────────────

        function name() external pure returns (string memory);
        function symbol() external pure returns (string memory);
        function tokenURI(uint256 tokenId) external pure returns (string memory);
        function ownerOf(uint256 tokenId) external view returns (address);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Beacon — oracle index contract
    // ═══════════════════════════════════════════════════════════════════

    /// Beacon interface — emits `IndexUpdated` when the oracle index changes.
    /// Each perp has its own beacon (from `PerpConfig.beacon`).
    interface IBeacon {
        event IndexUpdated(uint256 index);

        function index() external view returns (uint256);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  ERC20 (USDC) — minimal interface for approve + balanceOf
    // ═══════════════════════════════════════════════════════════════════

    /// Minimal ERC20 interface for USDC interactions.
    #[sol(rpc)]
    interface IERC20 {
        function approve(address spender, uint256 amount)
            external
            returns (bool);

        function allowance(address owner, address spender)
            external view returns (uint256);

        function balanceOf(address account)
            external view returns (uint256);

        function transfer(address to, uint256 amount)
            external
            returns (bool);

        function transferFrom(address from, address to, uint256 amount)
            external
            returns (bool);

        event Transfer(address indexed from, address indexed to, uint256 value);
        event Approval(address indexed owner, address indexed spender, uint256 value);
    }
}
