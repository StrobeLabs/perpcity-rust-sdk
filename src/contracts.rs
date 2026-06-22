//! On-chain contract bindings generated via Alloy's `sol!` macro.
//!
//! Structs, events, errors, and function selectors are reconciled against the
//! frozen, audited `perpcity-contracts/src/` (`Perp.sol`, `PerpFactory.sol`,
//! `libraries/Structs.sol`, `libraries/Events.sol`, `libraries/Errors.sol`,
//! `interfaces/modules/*`).
//!
//! Architecture: `PerpFactory` creates `Perp` contracts. There is no
//! `PerpManager` — each market is its own `Perp` contract (ERC721 for position
//! NFTs), identified by its contract address. Positions are keyed by `posId`
//! (the ERC721 token id) within that `Perp`. The SDK interacts with an
//! individual `Perp` contract for all trading and reads.
//!
//! Units (from `Structs.sol`): prices scaled by 2^96; margin/USD/fees in USDC
//! decimals (6); fee & margin-ratio params scaled by 1e6; funding & utilization
//! rates scaled by 1e18 per day. `BalanceDelta` packs `int128 amount0` (perp)
//! and `int128 amount1` (USD) into an `int256`; positive = asset, negative = debt.

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
    //  Shared structs (from libraries/Structs.sol)
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
        uint256 lpFeeGrowthGlobalX128;
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

    /// Tick-level funding + LP-fee growth info.
    struct TickInfo {
        int256 cumlFundingOppX96;
        int256 cumlFundingDivSqrtPOppX96;
        uint256 lpFeeGrowthOutsideX128;
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

    /// Base position state shared by makers and takers.
    /// `delta` is a packed `BalanceDelta` (`int128 amount0`, `int128 amount1`).
    struct Position {
        int256 delta;
        uint128 margin;
        uint24 initMarginRatio;
        uint24 liqMarginRatio;
        uint24 backstopMarginRatio;
        int256 lastCumlFundingX96;
    }

    /// Maker-specific state for an active liquidity range.
    struct Maker {
        int24 tickLower;
        int24 tickUpper;
        uint128 liquidity;
        uint256 lastLpFeeGrowthInsideX128;
        uint256 lastLongUtilEarningsX96;
        uint256 lastShortUtilEarningsX96;
        Capacity capacity;
        MakerFunding lastCumlFunding;
    }

    /// Taker-specific checkpoints for utilization fees.
    struct Taker {
        uint256 lastLongUtilPaymentsX96;
        uint256 lastShortUtilPaymentsX96;
    }

    /// Result of a taker swap plus fees charged on the swap's USD volume.
    /// `delta` is a packed `BalanceDelta`.
    struct SwapResult {
        int256 delta;
        uint256 ammPrice;
        int256 totalFeeAmt;
        uint256 lpFeeAmt;
        uint256 protocolFeeAmt;
        uint256 creatorFeeAmt;
        uint256 insuranceFeeAmt;
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
    //  Module interfaces (from interfaces/modules/*)
    // ═══════════════════════════════════════════════════════════════════

    /// Fee module — returns trading fees.
    #[sol(rpc)]
    interface IFees {
        function fees() external view returns (uint24 cFee, uint24 insFee, uint24 lpFee);
        function utilFees(uint256 longUtilization, uint256 shortUtilization)
            external view returns (uint64 longFee, uint64 shortFee);
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

    /// Funding module — returns funding paid per day per unit of USD exposure.
    #[sol(rpc)]
    interface IFunding {
        function funding(PricePair spots, PricePair emas) external view returns (int88);
    }

    /// Price impact module — returns sqrt price bounds per transaction.
    #[sol(rpc)]
    interface IPriceImpact {
        function sqrtPriceBounds(uint256 ammPrice, uint256 index, uint256 emaAmmPrice, uint256 emaIndex)
            external view returns (uint256 sqrtMin, uint256 sqrtMax);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Perp — individual perpetual market contract (no PerpManager)
    // ═══════════════════════════════════════════════════════════════════

    /// The Perp contract interface. Each market is its own Perp contract.
    /// Inherits ERC721 (position NFTs).
    #[sol(rpc)]
    interface Perp {
        // ── Events (from libraries/Events.sol) ──────────────────────

        event MakerOpened(uint256 posId);
        event MakerAdjusted(uint256 posId, int256 funding, uint256 longUtilFees, uint256 shortUtilFees, uint256 lpFees);
        event MakerConverted(uint256 posId, int256 funding, uint256 longUtilFees, uint256 shortUtilFees, uint256 lpFees);
        event MakerClosed(uint256 posId, int256 funding, uint256 longUtilFees, uint256 shortUtilFees, uint256 lpFees);
        event MakerLiquidated(uint256 indexed posId, uint128 liquidityAmount, uint256 liqFee);
        event MakerBackstopped(
            uint256 posId,
            uint128 marginIn,
            address posRecipient,
            int256 funding,
            uint256 longUtilFees,
            uint256 shortUtilFees,
            uint256 lpFees
        );
        event TakerOpened(uint256 posId, SwapResult sr);
        event TakerAdjusted(uint256 posId, SwapResult sr, int256 funding, uint256 utilFees);
        event TakerClosed(uint256 posId, SwapResult sr, int256 funding, uint256 utilFees);
        event TakerLiquidated(uint256 indexed posId, uint128 perpAmount, uint256 liqFee);
        event TakerBackstopped(uint256 posId, uint128 marginIn, address posRecipient, int256 funding, uint256 utilFees);

        // ── Errors (from libraries/Errors.sol) ──────────────────────

        error Abdicated();
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
        error DataNotTimelocked();
        error HealthNotImproved();
        error MarginRatioTooLow();
        error DataAlreadyPending();
        error PriceImpactTooHigh();
        error TimelockNotExpired();
        error UnauthorizedCaller();
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

        /// Liquidate an unhealthy maker position (`liquidityAmount` of the range).
        function liquidateMaker(uint256 posId, address liquidationFeeRecipient, uint128 liquidityAmount) external;

        /// Backstop a maker position approaching liquidation.
        function backstopMaker(uint256 posId, uint128 marginIn, address positionRecipient) external;

        /// Open a taker (long/short) position.
        /// `perpDelta` > 0 = long, < 0 = short.
        function openTaker(OpenTakerParams calldata params)
            external returns (uint256 posId);

        /// Adjust a taker position (margin, size, or both). Close by passing
        /// opposing `perpDelta`. Burns the position NFT if fully closed.
        function adjustTaker(AdjustTakerParams calldata params) external;

        /// Liquidate an unhealthy taker position (`perpAmount` of exposure).
        function liquidateTaker(uint256 posId, address liquidationFeeRecipient, uint128 perpAmount) external;

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

        /// Live Uniswap V4 pool state (slot0 + liquidity). `ammPrice` is scaled by 2^96.
        function poolState() external view returns (
            int24 tick, uint160 sqrtPrice, uint256 ammPrice, uint128 liquidity
        );

        /// Module addresses (beacon, fees, funding, marginRatios, priceImpact, pricing).
        function modules() external view returns (Modules memory);

        /// Base position state.
        function positions(uint256 posId) external view returns (Position memory);

        /// Maker-specific state for a position.
        function makerDetails(uint256 posId) external view returns (Maker memory);

        /// Taker-specific state for a position.
        function takerDetails(uint256 posId) external view returns (Taker memory);

        function nextPosId() external view returns (uint256);

        function feeFund() external view returns (FeeFund memory);

        function solvencyState() external view returns (SolvencyState memory);

        function openInterest() external view returns (OpenInterest memory);

        function capacity() external view returns (Capacity memory);

        function rates() external view returns (Rates memory);

        function cumulatives() external view returns (Cumulatives memory);

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
        /// Emitted when a new perp market is created. `poolId` is a Uniswap V4 `PoolId` (bytes32).
        event PerpCreated(
            address perp,
            bytes32 poolId,
            Modules modules,
            uint256 initialIndex,
            uint24 emaWindow,
            uint256 protocolFee,
            uint160 sqrtPriceX96,
            int24 tick,
            address owner,
            string name,
            string symbol,
            string tokenUri
        );

        error NotPoolManager();
        error StartingPriceTooLow();
        error StartingPriceTooHigh();
        error EmaWindowTooLow();

        /// Create a new perpetual market. Returns the Perp contract address.
        function createPerp(
            address owner,
            string memory name,
            string memory symbol,
            string memory tokenUri,
            Modules memory modules,
            uint24 emaWindow,
            bytes32 salt
        ) external returns (address perp);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Beacon — oracle index contract
    // ═══════════════════════════════════════════════════════════════════

    /// Beacon interface — emits `IndexUpdated` when the oracle index changes.
    /// Note: `index()` is state-mutating (not a pure view) per the beacons lib.
    #[sol(rpc)]
    interface IBeacon {
        event IndexUpdated(uint256 index);
        function index() external returns (uint256);
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
