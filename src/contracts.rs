//! On-chain contract bindings generated via Alloy's `sol!` macro.
//!
//! Structs, events, errors, and function selectors are derived from
//! `perpcity-contracts/src/` (branch: unification-and-cleanup).
//!
//! Architecture: `PerpFactory` creates `Perp` contracts. Each market is
//! its own `Perp` contract (ERC721 for position NFTs). SDK interacts
//! with individual `Perp` contracts for trading.

use alloy::sol;

sol! {
    // ═══════════════════════════════════════════════════════════════════
    //  Uniswap V4 types used by PerpCity
    // ═══════════════════════════════════════════════════════════════════

    /// Identifies a Uniswap V4 pool.
    struct PoolKey {
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Shared structs (from SharedStructs.sol)
    // ═══════════════════════════════════════════════════════════════════

    /// Maker-specific funding tracking.
    struct MakerFunding {
        int256 belowX96;
        int256 withinX96;
        int256 divSqrtPriceWithinX96;
    }

    /// Long/short capacity.
    struct Capacity {
        uint128 long;
        uint128 short;
    }

    /// AMM price + index price pair (also used for EMAs).
    struct PricePair {
        uint128 ammPrice;
        uint128 index;
    }

    /// Current market snapshot after accrual.
    struct Snapshot {
        int24 tick;
        uint160 sqrtAmmPrice;
        PricePair spots;
        PricePair emas;
        uint256 markPrice;
    }

    /// Funding and utilization rates.
    struct Rates {
        int88 fundingPerDay;
        uint64 longUtilFeePerDay;
        uint64 shortUtilFeePerDay;
        uint40 lastTouch;
    }

    /// Cumulative funding and fee trackers.
    struct Cumulatives {
        int256 fundingX96;
        int256 fundingDivSqrtPX96;
        uint256 longUtilPaymentsX96;
        uint256 shortUtilPaymentsX96;
        uint256 longUtilEarningsX96;
        uint256 shortUtilEarningsX96;
    }

    /// Long/short open interest.
    struct OpenInterest {
        uint128 long;
        uint128 short;
    }

    /// Insurance + fee fund balances.
    struct FeeFund {
        uint80 insurance;
        uint80 creatorFees;
        uint80 protocolFees;
    }

    /// Bad debt + total margin tracking.
    struct SolvencyState {
        uint128 badDebt;
        uint128 totalMargin;
    }

    /// Tick-level funding info.
    struct TickInfo {
        int256 cumlFundingOppX96;
        int256 cumlFundingDivSqrtPOppX96;
    }

    /// Module addresses for a Perp market.
    struct Modules {
        address beacon;
        address fees;
        address funding;
        address marginRatios;
        address priceImpact;
        address pricing;
    }

    // ── Parameter structs ───────────────────────────────────────────

    struct OpenMakerParams {
        address holder;
        uint128 margin;
        int24 tickLower;
        int24 tickUpper;
        uint128 liquidity;
        uint256 maxAmt0In;
        uint256 maxAmt1In;
    }

    struct AdjustMakerParams {
        uint256 posId;
        int128 marginDelta;
        int128 liquidityDelta;
        uint256 amt0Limit;
        uint256 amt1Limit;
    }

    struct OpenTakerParams {
        address holder;
        uint128 margin;
        int256 perpDelta;
        uint256 amt1Limit;
    }

    struct AdjustTakerParams {
        uint256 posId;
        int128 marginDelta;
        int256 perpDelta;
        uint256 amt1Limit;
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Module interfaces
    // ═══════════════════════════════════════════════════════════════════

    /// Fee module — returns trading fees.
    #[sol(rpc)]
    interface IFees {
        function fees() external view returns (uint24 cFee, uint24 insFee, uint24 lpFee);
        function utilFees(uint24 longUtilization, uint24 shortUtilization)
            external view returns (uint96 longFee, uint96 shortFee);
        function liqFee() external view returns (uint24);
    }

    /// Margin ratio module — returns init/liquidation/backstop ratios.
    #[sol(rpc)]
    interface IMarginRatios {
        function makerMarginRatios() external view returns (uint24 init, uint24 liq, uint24 backstop);
        function takerMarginRatios() external view returns (uint24 init, uint24 liq, uint24 backstop);
    }

    /// Pricing module — determines fair/mark price from AMM + index + EMAs.
    #[sol(rpc)]
    interface IPricing {
        function fairPrice(uint256 ammPrice, uint256 index, uint256 emaAmmPrice, uint256 emaIndex)
            external view returns (uint256);
    }

    /// Funding module — returns funding payment rate.
    #[sol(rpc)]
    interface IFunding {
        function funding(uint256 ammPrice, uint256 index, uint256 emaAmmPrice, uint256 emaIndex)
            external view returns (int96);
    }

    /// Price impact module — returns sqrt price bounds per transaction.
    #[sol(rpc)]
    interface IPriceImpact {
        function sqrtPriceBounds(uint256 ammPrice, uint256 index, uint256 emaAmmPrice, uint256 emaIndex)
            external view returns (uint256 sqrtMin, uint256 sqrtMax);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Perp — individual perpetual market contract
    // ═══════════════════════════════════════════════════════════════════

    /// The Perp contract interface. Each market is its own Perp contract.
    /// Inherits ERC721 (position NFTs).
    #[sol(rpc)]
    interface Perp {
        // ── Events (from Events.sol) ────────────────────────────────

        event MakerOpened();
        event MakerClosed();
        event MakerAdjusted();
        event MakerLiquidated();
        event TakerOpened();
        event TakerClosed();
        event TakerAdjusted();
        event TakerLiquidated();
        event MakerBackstopped();
        event TakerBackstopped();
        event MakerConvertedToTaker();

        // ── Errors (from Errors.sol) ────────────────────────────────

        error ZeroDelta();
        error MinAmtUnmet();
        error MarginTooLow();
        error NoSystemFunds();
        error ZeroLiquidity();
        error MaxAmtExceeded();
        error NegativeEquity();
        error NegativeMargin();
        error NotPoolManager();
        error NotLiquidatable();
        error NonMakerPosition();
        error NonTakerPosition();
        error TicksOutOfBounds();
        error MarginRatioTooLow();
        error PriceImpactTooHigh();
        error UnauthorizedCaller();
        error PositionValueTooLow();
        error PositionDoesNotExist();
        error LongUtilizationExceeded();
        error ShortUtilizationExceeded();
        error InsufficientLiquidityToFill();

        // ── Position management ─────────────────────────────────────

        /// Open a maker (LP) position.
        function openMaker(OpenMakerParams calldata params)
            external returns (uint256 posId);

        /// Adjust a maker position (margin, liquidity, or both).
        /// Burns the position NFT if fully closed.
        function adjustMaker(AdjustMakerParams calldata params) external;

        /// Liquidate an unhealthy maker position.
        function liquidateMaker(uint256 posId, address liquidationFeeRecipient) external;

        /// Backstop a maker position approaching liquidation.
        function backstopMaker(uint256 posId, uint128 marginIn, address positionRecipient) external;

        /// Open a taker (long/short) position.
        /// `perpDelta` > 0 = long, < 0 = short.
        function openTaker(OpenTakerParams calldata params)
            external returns (uint256 posId);

        /// Adjust a taker position (margin, size, or both). Close by passing
        /// opposing `perpDelta`. Burns the position NFT if fully closed.
        function adjustTaker(AdjustTakerParams calldata params) external;

        /// Liquidate an unhealthy taker position.
        function liquidateTaker(uint256 posId, address liquidationFeeRecipient) external;

        /// Backstop a taker position approaching liquidation.
        function backstopTaker(uint256 posId, uint128 marginIn, address positionRecipient) external;

        // ── State maintenance ───────────────────────────────────────

        /// Accrue funding and update rates without any position changes.
        function touch() external;

        /// Donate USDC to the insurance fund.
        function donate(uint128 amount) external;

        // ── Fee collection ──────────────────────────────────────────

        function collectCreatorFees(address recipient) external;
        function collectProtocolFees(address recipient) external;
        function syncProtocolFee() external;

        // ── View functions ──────────────────────────────────────────

        function poolKey() external view returns (PoolKey memory);

        /// Position data. `delta` is a packed BalanceDelta (int128 amount0, int128 amount1).
        function positions(uint256 posId) external view returns (
            int256 delta,
            uint128 margin,
            uint24 liqMarginRatio,
            uint24 backstopMarginRatio,
            int256 lastCumlFundingX96
        );

        function makerDetails(uint256 posId) external view returns (
            int24 tickLower,
            int24 tickUpper,
            uint128 liquidity,
            uint256 lastLongUtilEarningsX96,
            uint256 lastShortUtilEarningsX96,
            Capacity capacity_,
            MakerFunding lastCumlFunding
        );

        function takerDetails(uint256 posId) external view returns (
            uint256 lastLongUtilPaymentsX96,
            uint256 lastShortUtilPaymentsX96
        );

        function nextPosId() external view returns (uint256);

        function feeFund() external view returns (
            uint80 insurance, uint80 creatorFees, uint80 protocolFees
        );

        function solvencyState() external view returns (
            uint128 badDebt, uint128 totalMargin
        );

        function openInterest() external view returns (uint128 long, uint128 short);

        function capacity() external view returns (uint128 long, uint128 short);

        function rates() external view returns (
            int88 fundingPerDay,
            uint64 longUtilFeePerDay,
            uint64 shortUtilFeePerDay,
            uint40 lastTouch
        );

        // ── ERC721 ─────────────────────────────────────────────────

        function name() external view returns (string memory);
        function symbol() external view returns (string memory);
        function tokenURI(uint256 tokenId) external view returns (string memory);
        function ownerOf(uint256 tokenId) external view returns (address);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  PerpFactory — creates Perp contracts
    // ═══════════════════════════════════════════════════════════════════

    #[sol(rpc)]
    interface PerpFactory {
        event PerpCreated(address perp);

        error StartingPriceTooLow();
        error StartingPriceTooHigh();
        error EmaWindowTooLow();

        /// Create a new perpetual market. Returns the Perp contract address.
        function createPerp(
            address owner,
            bytes32 name,
            bytes32 symbol,
            bytes32 tokenUri,
            Modules modules,
            uint24 emaWindow
        ) external returns (address perp);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Beacon — oracle index contract
    // ═══════════════════════════════════════════════════════════════════

    /// Beacon interface — emits `IndexUpdated` when the oracle index changes.
    #[sol(rpc)]
    interface IBeacon {
        event IndexUpdated(uint256 index);
        function index() external view returns (uint256);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  ERC20 (USDC) — minimal interface for approve + balanceOf
    // ═══════════════════════════════════════════════════════════════════

    #[sol(rpc)]
    interface IERC20 {
        function approve(address spender, uint256 amount)
            external returns (bool);
        function allowance(address owner, address spender)
            external view returns (uint256);
        function balanceOf(address account)
            external view returns (uint256);
        function transfer(address to, uint256 amount)
            external returns (bool);
        function transferFrom(address from, address to, uint256 amount)
            external returns (bool);

        event Transfer(address indexed from, address indexed to, uint256 value);
        event Approval(address indexed owner, address indexed spender, uint256 value);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Multicall3 — batch multiple contract reads into a single eth_call
    // ═══════════════════════════════════════════════════════════════════

    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }

        struct Result {
            bool success;
            bytes returnData;
        }

        function aggregate3(Call3[] calldata calls)
            external payable returns (Result[] memory returnData);

        function getEthBalance(address addr)
            external view returns (uint256 balance);
    }
}
