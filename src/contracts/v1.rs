//! The bindings of the audited build, contracts tag `v1.0.0` (`0df4208`),
//! with the beacons at `f762da1`.
//!
//! A `v1.0.0` market shares no struct shape, trade selector or position
//! event with the two earlier builds, so it has its own module: every name
//! here is the `v1.0.0` source's name, and a caller picks this module by
//! the market's era. The factories, hooks and implementations are deployed
//! on Arbitrum One and Arbitrum Sepolia (one address per chain, a different
//! contract on each).
//!
//! Each interface is the deployed contract's ABI, complete for [`Perp`],
//! [`PerpFactory`] and [`IBeacon`]; a module interface is the `v1.0.0`
//! `I*` interface plus the module's own views. The `abi_lock` tests hold
//! every selector, topic and struct against `forge inspect` at the tag.
//!
//! Units as in the earlier builds, with these additions: `X192` and `X128`
//! fields are scaled by `2^192` and `2^128`, `X64` capacities by `2^64`,
//! `deadline` is a unix timestamp in seconds, and a `BalanceDelta` field
//! (`delta`) is the packed `int256`.

// `sol!`-generated RPC methods mirror the Solidity arity (`createPerp`
// takes 7 params, `initialize` 10).
#![allow(clippy::too_many_arguments)]

use alloy::sol;

sol! {
    // ═══════════════════════════════════════════════════════════════════
    //  Uniswap V4
    // ═══════════════════════════════════════════════════════════════════

    /// Identifies a Uniswap V4 pool. A `v1.0.0` pool's `hooks` is the
    /// chain's `v1.0.0` `PerpGuardHook`.
    struct PoolKey {
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Structs (libraries/Structs.sol)
    // ═══════════════════════════════════════════════════════════════════

    /// The six rule modules a market delegates to.
    struct Modules {
        address beacon;
        address fees;
        address funding;
        address marginRatios;
        address priceImpact;
        address pricing;
    }

    /// The withdrawal controller's rate limit, set at creation.
    struct WithdrawalConfig {
        uint128 capacity;
        uint128 refillPerSecond;
        uint48 challengeWindow;
    }

    /// A position's shared row. Its owner is `positionOwner`, not a field.
    struct Position {
        int256 delta;
        uint128 margin;
        int256 lastCumlFundingX96;
    }

    /// A maker's band and its checkpoints.
    struct Maker {
        int24 tickLower;
        int24 tickUpper;
        uint128 liquidity;
        uint256 lastLpFeeGrowthInsideX128;
        MakerFunding lastCumlFunding;
        int256 lastFundingCorrectionX128;
    }

    /// A taker's utilization checkpoints.
    struct Taker {
        uint256 lastUtilPaymentsX96;
        uint256 lastReserveUtilPaymentsX96;
    }

    /// Long/short virtual capacity.
    struct Capacity {
        uint128 long;
        uint128 short;
    }

    /// Insurance and fee fund balances.
    struct FeeFund {
        uint80 insurance;
        uint80 creatorFees;
        uint80 protocolFees;
    }

    /// The funding split: the base and inventory legs, and the maker
    /// liquidity the inventory leg is spread over.
    struct FundingState {
        int256 baseX96;
        int256 inventoryX96;
        uint256 makerLiquidity;
    }

    /// Long/short open interest.
    struct OpenInterest {
        uint128 long;
        uint128 short;
    }

    /// The close depth each side has in the pool, at `2^64`.
    struct PhysicalCapacity {
        uint256 longCloseX64;
        uint256 shortCloseX64;
    }

    /// Bad debt and total margin.
    struct SolvencyState {
        uint128 badDebt;
        uint128 totalMargin;
    }

    /// The utilization band inside which no utilization fee accrues.
    struct UtilizationBand {
        uint128 lower;
        uint128 upper;
    }

    /// The accounting trackers.
    struct Cumulatives {
        int256 fundingX96;
        int256 fundingDivSqrtPX192;
        uint256 lpFeeGrowthGlobalX128;
        uint256 longUtilPaymentsX96;
        uint256 shortUtilPaymentsX96;
        uint256 longReserveUtilPaymentsX96;
        uint256 shortReserveUtilPaymentsX96;
        int256 fundingCorrectionX128;
    }

    /// A maker's funding checkpoints.
    struct MakerFunding {
        int256 belowX96;
        int256 withinInventoryX192;
    }

    /// AMM price and index price, `uint128` each (also the EMA pair).
    struct PricePair {
        uint128 ammPrice;
        uint128 index;
    }

    /// Funding and utilization rates, the last touch and the mark at it.
    struct Rates {
        int88 fundingPerDay;
        uint64 longUtilFeePerDay;
        uint64 shortUtilFeePerDay;
        uint40 lastTouch;
        uint256 lastMark;
    }

    /// Per-tick funding and fee-growth words, read as storage slots.
    struct TickInfo {
        int256 cumlFundingOppX96;
        int256 cumlFundingDivSqrtPOppX192;
        uint256 lpFeeGrowthOutsideX128;
    }

    /// `previewPosition`'s answer: a position valued as a liquidation
    /// would value it now.
    struct PositionPreview {
        bool isMaker;
        uint256 markPrice;
        int256 settledMargin;
        uint256 pendingLpFees;
        int256 pendingFunding;
        uint256 pendingUtil;
        uint256 pendingReserveUtil;
        uint256 value;
        int256 unrealizedPnl;
        int256 equity;
        bool meetsInitialMargin;
        bool liquidatable;
        bool backstoppable;
        uint256 nominalLiqFee;
        uint256 payableLiqFee;
    }

    /// A swap's outcome with its fee legs; `feeFundFeeAmt` is new here.
    struct SwapResult {
        int256 delta;
        uint256 ammPrice;
        int256 totalFeeAmt;
        uint256 lpFeeAmt;
        uint256 protocolFeeAmt;
        uint256 creatorFeeAmt;
        uint256 insuranceFeeAmt;
        uint256 feeFundFeeAmt;
    }

    // ── Parameter structs ───────────────────────────────────────────

    struct AdjustMakerParams {
        uint256 posId;
        int128 marginDelta;
        int128 liquidityDelta;
        uint256 amt0Limit;
        uint256 amt1Limit;
        uint64 deadline;
    }

    struct MoveMakerParams {
        uint256 posId;
        int24 tickLower;
        int24 tickUpper;
        uint128 liquidity;
        uint256 removeAmt0Min;
        uint256 removeAmt1Min;
        uint256 addAmt0Max;
        uint256 addAmt1Max;
        int128 marginDelta;
        uint64 deadline;
    }

    struct AdjustTakerParams {
        uint256 posId;
        int128 marginDelta;
        int256 perpDelta;
        uint256 amt1Limit;
        uint256 maxTotalFee;
        bool reduceOnly;
        uint64 deadline;
    }

    struct OpenMakerParams {
        address holder;
        uint128 margin;
        int24 tickLower;
        int24 tickUpper;
        uint128 liquidity;
        uint256 maxAmt0In;
        uint256 maxAmt1In;
        uint64 deadline;
    }

    struct OpenTakerParams {
        address holder;
        uint128 margin;
        int256 perpDelta;
        uint256 amt1Limit;
        uint256 maxTotalFee;
        uint64 deadline;
    }

    // ── Module structs ──────────────────────────────────────────────

    /// The fee module's LP fee curve (`Fees.LpFeeConfig`).
    struct LpFeeConfig {
        uint24 startFee;
        uint24 targetFee;
        uint128 liquidityDecay;
        uint24 volatilityScaler;
        uint24 maxFeeMultiplier;
    }

    /// A margin-ratio triple (`MarginRatios.Ratios`).
    struct Ratios {
        uint24 init;
        uint24 liq;
        uint24 backstop;
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Module interfaces (interfaces/modules/*, plus each module's views)
    // ═══════════════════════════════════════════════════════════════════

    /// Fee module: the swap-fee split by liquidity and prices, the
    /// liquidation fee, the swap-fee cap and the utilization-fee curve.
    #[sol(rpc)]
    interface IFees {
        function fees(uint128 liquidity, PricePair calldata spots, PricePair calldata emas)
            external view returns (uint24 cFee, uint24 insFee, uint24 lpFee);
        function liqFee() external view returns (uint24);
        function maxSwapFee() external view returns (uint256 rateE6);
        function utilFees(uint256 longUtilization, uint256 shortUtilization)
            external view returns (uint64 longFee, uint64 shortFee);
        function cFee() external view returns (uint24);
        function insFee() external view returns (uint24);
        function lpFeeConfig() external view returns (
            uint24 startFee,
            uint24 targetFee,
            uint128 liquidityDecay,
            uint24 volatilityScaler,
            uint24 maxFeeMultiplier
        );
        function targetUtilization() external view returns (uint256);
        function slopeBeforeTarget() external view returns (uint256);
        function slopeAfterTarget() external view returns (uint256);

        event LiqFeeSet(uint24 liqFee);
        event LpFeeConfigSet(LpFeeConfig lpFeeConfig);
        event SwapFeesSet(uint24 cFee, uint24 insFee);
        event UtilFeeCurveSet(uint256 targetUtilization, uint256 slopeBeforeTarget, uint256 slopeAfterTarget);
    }

    /// Funding module: funding per day from the spots, the EMAs and the mark.
    #[sol(rpc)]
    interface IFunding {
        function funding(PricePair calldata spots, PricePair calldata emas, uint256 markPrice)
            external view returns (int88);
        function fundingRateMultiplier() external view returns (uint256);
        function maxFundingPerDayPercent() external view returns (uint256);

        event FundingRateMultiplierSet(uint256 fundingRateMultiplier);
        event MaxFundingPerDayPercentSet(uint256 maxFundingPerDayPercent);
    }

    /// Margin-ratio module: the live maker and taker triples, and the
    /// timelocked updates.
    #[sol(rpc)]
    interface IMarginRatios {
        function makerMarginRatios() external view returns (uint24 init, uint24 liq, uint24 backstop);
        function takerMarginRatios() external view returns (uint24 init, uint24 liq, uint24 backstop);
        function pendingMakerRatios() external view returns (Ratios memory ratios, uint48 executableAt);
        function pendingTakerRatios() external view returns (Ratios memory ratios, uint48 executableAt);
        function UPDATE_DELAY() external view returns (uint48);

        event MakerMarginRatiosScheduled(Ratios ratios, uint48 executableAt);
        event MakerMarginRatiosExecuted(Ratios ratios, uint48 executableAt);
        event MakerMarginRatiosCancelled(Ratios ratios, uint48 executableAt);
        event MakerMarginRatiosSet(Ratios ratios);
        event TakerMarginRatiosScheduled(Ratios ratios, uint48 executableAt);
        event TakerMarginRatiosExecuted(Ratios ratios, uint48 executableAt);
        event TakerMarginRatiosCancelled(Ratios ratios, uint48 executableAt);
        event TakerMarginRatiosSet(Ratios ratios);
    }

    /// Price-impact module: the sqrt-price bounds a swap may reach, now
    /// with the mark as a fifth argument.
    #[sol(rpc)]
    interface IPriceImpact {
        function sqrtPriceBounds(
            uint256 ammPrice,
            uint256 index,
            uint256 emaAmmPrice,
            uint256 emaIndex,
            uint256 markPrice
        ) external view returns (uint256 sqrtMin, uint256 sqrtMax);
        function priceImpactBound() external view returns (uint256);

        event PriceImpactBoundSet(uint256 priceImpactBound);
    }

    /// Pricing module: the fair (mark) price and its premium cap.
    #[sol(rpc)]
    interface IPricing {
        function fairPrice(uint256 ammPrice, uint256 index, uint256 emaAmmPrice, uint256 emaIndex)
            external view returns (uint256);
        function maxPremiumRatio() external view returns (uint256);

        event MaxPremiumRatioSet(uint256 maxPremiumRatio);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Perp
    // ═══════════════════════════════════════════════════════════════════

    /// A `v1.0.0` market: an ERC-1967 proxy over the factory's
    /// implementation. Positions are ids with an owner (`positionOwner`),
    /// not NFTs. Every trade takes a `deadline`; a taker trade also takes
    /// `maxTotalFee`, and an adjust `reduceOnly`.
    #[sol(rpc)]
    interface Perp {
        // ── Position events ─────────────────────────────────────────

        event MakerOpened(
            uint256 indexed posId,
            address indexed owner,
            int24 tickLower,
            int24 tickUpper,
            uint128 liquidity,
            int256 delta,
            int256 lastCumlFundingX96,
            MakerFunding lastCumlFunding
        );
        event MakerAdjusted(
            uint256 indexed posId,
            address indexed owner,
            int128 marginDelta,
            int128 liquidityDelta,
            int256 funding,
            uint256 lpFees,
            int256 delta,
            uint128 margin,
            uint128 liquidity,
            int256 lastCumlFundingX96,
            MakerFunding lastCumlFunding
        );
        event MakerConverted(
            uint256 indexed posId,
            address indexed owner,
            int128 marginDelta,
            int128 liquidityDelta,
            int256 funding,
            uint256 lpFees,
            int256 delta,
            uint128 margin,
            uint128 liquidity,
            int256 lastCumlFundingX96,
            MakerFunding lastCumlFunding
        );
        event MakerClosed(
            uint256 indexed posId,
            address indexed owner,
            int128 marginDelta,
            int128 liquidityDelta,
            int256 funding,
            uint256 lpFees,
            int256 delta,
            uint128 margin,
            uint128 liquidity,
            int256 lastCumlFundingX96,
            MakerFunding lastCumlFunding
        );
        event MakerMoved(
            uint256 indexed posId,
            address indexed owner,
            int24 oldTickLower,
            int24 oldTickUpper,
            int24 newTickLower,
            int24 newTickUpper,
            uint128 oldLiquidity,
            uint128 newLiquidity,
            int128 marginDelta,
            int256 funding,
            uint256 lpFees
        );
        event MakerBackstopped(
            uint256 indexed posId,
            uint128 marginIn,
            address posRecipient,
            int256 funding,
            uint256 lpFees,
            uint128 margin,
            int256 delta,
            uint128 liquidity,
            int256 lastCumlFundingX96,
            MakerFunding lastCumlFunding
        );
        event MakerLiquidated(uint256 indexed posId, address indexed feeRecipient, uint128 liquidityAmount, uint256 liqFee);
        event TakerOpened(
            uint256 indexed posId,
            address indexed owner,
            uint128 margin,
            SwapResult sr,
            int256 lastCumlFundingX96
        );
        event TakerAdjusted(
            uint256 indexed posId,
            address indexed owner,
            int128 marginDelta,
            SwapResult sr,
            int256 funding,
            uint256 utilFees,
            int256 delta,
            uint128 margin,
            int256 lastCumlFundingX96
        );
        event TakerClosed(
            uint256 indexed posId,
            address indexed owner,
            int128 marginDelta,
            SwapResult sr,
            int256 funding,
            uint256 utilFees,
            int256 delta,
            uint128 margin,
            int256 lastCumlFundingX96
        );
        event TakerBackstopped(
            uint256 indexed posId,
            uint128 marginIn,
            address posRecipient,
            int256 funding,
            uint256 utilFees,
            int256 delta,
            uint128 margin,
            int256 lastCumlFundingX96
        );
        event TakerLiquidated(uint256 indexed posId, address indexed feeRecipient, uint128 perpAmount, uint256 liqFee);
        /// A position's owner set (`oldOwner` zero), moved, or cleared
        /// (`newOwner` zero, on a full close).
        event PositionOwnerUpdated(uint256 indexed posId, address indexed oldOwner, address indexed newOwner);
        event QuoteFolded(uint256 indexed posId, int256 amount);
        event UtilizationReserveRealized(
            uint256 indexed posId,
            uint256 amount,
            uint256 badDebtRepaid,
            uint256 insuranceCredited
        );

        // ── Market-state events ─────────────────────────────────────

        event CapacityUpdated(Capacity cap);
        event PhysicalCapacityUpdated(PhysicalCapacity physicalCap);
        event OpenInterestUpdated(OpenInterest oi);
        event CumulativesAccrued(Cumulatives cumls);
        event RatesAndEmasRefreshed(Rates rates, uint128 index, PricePair emas);
        event UtilizationAccrued(
            uint128 longOi,
            uint128 shortOi,
            uint128 longCapacity,
            uint128 shortCapacity,
            uint256 longMakerReward,
            uint256 shortMakerReward,
            uint256 longReserveFee,
            uint256 shortReserveFee
        );
        event TicksCrossed(int24 startingTick, int24 endingTick, bool zeroForOne);
        /// Same topic as the earlier builds; the third word is `X192` here.
        event TickInitialized(int24 tick, int256 cumlFundingOppX96, int256 cumlFundingDivSqrtPOppX192);
        event TickDeleted(int24 tick);

        // ── Margin, solvency and fee events ─────────────────────────

        event MarginTransferred(int128 marginDelta, uint128 totalMargin);
        event SwapFeesRemovedFromMargin(uint256 feeAmount, uint128 totalMargin);
        event UtilizationReserveRemovedFromMargin(uint256 amount, uint128 totalMargin);
        event Donated(address donor, uint128 amount, uint128 badDebt, uint80 insurance);
        event BadDebtAccounted(uint256 badDebt, uint256 insuranceConsumed, uint256 insuranceAfter, uint128 badDebtAfter);
        event LossSocialized(uint256 originalAmount, uint256 feeCharged, uint128 badDebtAfter);
        event SurplusRecovered(address indexed recipient, uint256 amount);
        event CreatorFeesCollected(uint256 amount, address recipient);
        event ProtocolFeesCollected(uint256 amount, address recipient);
        event ProtocolFeeSynced(uint256 protocolFee);

        // ── Governance and configuration events ─────────────────────

        event SetBeacon(address indexed beacon);
        event SetFeesModule(address indexed fees);
        event SetFundingModule(address indexed funding);
        event SetMarginRatiosModule(address indexed marginRatios);
        event SetPriceImpactModule(address indexed priceImpact);
        event SetPricingModule(address indexed pricing);
        event SetEmaWindow(uint24 emaWindow);
        event SetLatency(uint32 latency);
        event SetTvlCap(uint256 tvlCap);
        event SetGovernor(address indexed governor);
        event SetWithdrawalController(address indexed controller);
        event PermissionlessDonationsEnabledSet(bool enabled);
        event BeaconRecoveryUsed(address indexed oldBeacon, address indexed newBeacon);
        event Abdicate(bytes4 indexed selector);

        // ── Proxy and ownership events (UUPS, Initializable, Solady Ownable) ─

        event Upgraded(address indexed implementation);
        event Initialized(uint64 version);
        event OwnershipTransferred(address indexed oldOwner, address indexed newOwner);
        event OwnershipHandoverRequested(address indexed pendingOwner);
        event OwnershipHandoverCanceled(address indexed pendingOwner);

        // ── Errors (libraries/Errors.sol, all of it) ────────────────

        error Abdicated();
        error AggregateSwapFeesTooHigh();
        error BeaconIndexUnreadable();
        error DeadlineExpired();
        error DonationsRestricted();
        error EmaWindowTooLow();
        error HealthNotImproved();
        error IdenticalTicks();
        error InsufficientLiquidityToFill();
        error InvalidCallbackAction();
        error LongCloseCapacityExceeded();
        error MarginRatioTooLow();
        error MarginTooHigh();
        error MarginTooLow();
        error MaxAmtExceeded();
        error MaxTotalFeeExceeded();
        error MinAmtUnmet();
        error NegativeEquity();
        error NegativeMargin();
        error NonMakerPosition();
        error NonTakerPosition();
        error NoSurplus();
        error NoSystemFunds();
        error NotLiquidatable();
        error NotPoolManager();
        error PositionDoesNotExist();
        error PriceImpactTooHigh();
        error PriceOutsideRange();
        error ReduceOnlyViolated();
        error ShortCloseCapacityExceeded();
        error StaleBeaconIndex();
        error TicksOutOfBounds();
        error UnauthorizedCaller();
        error ZeroAddress();
        error ZeroAmount();
        error ZeroCapacity();
        error ZeroDelta();
        error ZeroLiquidity();
        error AlreadyInitialized();
        error AmountNotDecreased();
        error ClaimIsPaused();
        error ClaimNotPaused();
        error ClaimNotPending();
        error ClaimNotReady();
        error NotGovernor();
        error NotGuardian();
        error NotPerp();
        error ReleasesArePaused();
        error TvlCapExceeded();
        error WindowNotIncreased();
        error ZeroWindow();

        // ── Errors of the proxy and ownership bases ─────────────────

        error AddressEmptyCode(address target);
        error ERC1967InvalidImplementation(address implementation);
        error ERC1967NonPayable();
        error FailedCall();
        error InvalidInitialization();
        error NotInitializing();
        error UUPSUnauthorizedCallContext();
        error UUPSUnsupportedProxiableUUID(bytes32 slot);
        error NewOwnerIsZeroAddress();
        error NoHandoverRequest();
        error Unauthorized();

        // ── Maker actions ───────────────────────────────────────────

        function openMaker(OpenMakerParams calldata params) external returns (uint256 posId);
        function adjustMaker(AdjustMakerParams calldata params) external;
        /// Move a maker to a new band in one transaction (`MakerMoved`).
        function moveMaker(MoveMakerParams calldata params) external;
        function liquidateMaker(uint256 posId, address liquidationFeeRecipient, uint128 liquidityAmount) external;
        function backstopMaker(uint256 posId, uint128 marginIn, address positionRecipient, uint64 deadline) external;

        // ── Taker actions ───────────────────────────────────────────

        function openTaker(OpenTakerParams calldata params) external returns (uint256 posId);
        function adjustTaker(AdjustTakerParams calldata params) external;
        function liquidateTaker(uint256 posId, address liquidationFeeRecipient, uint128 perpAmount) external;
        function backstopTaker(uint256 posId, uint128 marginIn, address positionRecipient, uint64 deadline) external;

        // ── Permissionless and fee actions ──────────────────────────

        function touch() external;
        function donate(uint128 amount) external;
        function collectProtocolFees(address recipient) external;
        function syncProtocolFee() external;

        // ── Owner and governor actions ──────────────────────────────

        function collectCreatorFees(address recipient) external;
        function lowerTvlCap(uint256 newTvlCap) external;
        function setEmaWindow(uint24 newEmaWindow) external;
        function setLatency(uint32 newLatency) external;
        function setPermissionlessDonationsEnabled(bool enabled) external;
        function abdicate(bytes4 selector) external;
        function setBeacon(address newBeacon) external;
        function setFeesModule(address newFees) external;
        function setFundingModule(address newFunding) external;
        function setGovernor(address newGovernor) external;
        function setMarginRatiosModule(address newMarginRatios) external;
        function setPriceImpactModule(address newPriceImpact) external;
        function setPricingModule(address newPricing) external;
        function setTvlCap(uint256 newTvlCap) external;
        function setWithdrawalController(address newController) external;
        function claimSurplus(address recipient) external returns (uint256 amount);

        // ── Views ───────────────────────────────────────────────────

        function abdicated(bytes4 selector) external view returns (bool);
        function accountedTvl() external view returns (uint256 tvl);
        function capacity() external view returns (Capacity memory);
        function cumulatives() external view returns (Cumulatives memory);
        function currency0() external view returns (address);
        function currency1() external view returns (address);
        function emaWindow() external view returns (uint24);
        function feeFund() external view returns (FeeFund memory);
        function fundingState() external view returns (FundingState memory);
        function governor() external view returns (address);
        function hooks() external view returns (address);
        /// The beacon's staleness limit, in seconds.
        function latency() external view returns (uint32);
        function makerDetails(uint256 posId) external view returns (Maker memory);
        function modules() external view returns (Modules memory);
        function nextPosId() external view returns (uint256);
        function openInterest() external view returns (OpenInterest memory);
        function owner() external view returns (address);
        function permissionlessDonationsEnabled() external view returns (bool);
        function physicalCloseCapacity() external view returns (uint128 longClose, uint128 shortClose);
        function poolId() external view returns (bytes32);
        function poolKey() external view returns (PoolKey memory);
        function poolState() external view returns (int24 tick, uint160 sqrtPrice, uint256 ammPrice, uint128 liquidity);
        /// Zero for an id that is not open.
        function positionOwner(uint256 posId) external view returns (address);
        function positions(uint256 posId) external view returns (Position memory);
        /// Reverts `PositionDoesNotExist` for an id that is not open.
        function previewPosition(uint256 posId) external view returns (PositionPreview memory);
        function protocolFee() external view returns (uint256);
        function protocolFeeManager() external view returns (address);
        function rates() external view returns (Rates memory);
        function readBeaconIndex(address beacon) external view returns (uint128);
        function solvencyState() external view returns (SolvencyState memory);
        function takerDetails(uint256 posId) external view returns (Taker memory);
        function tvlCap() external view returns (uint256);
        function utilizationBand() external view returns (UtilizationBand memory);
        function withdrawalController() external view returns (address);

        // ── Proxy, ownership and callbacks ──────────────────────────

        function initialize(
            address _protocolFeeManager,
            uint256 _protocolFee,
            address _owner,
            address _governor,
            address _withdrawalController,
            Modules calldata _modules,
            PoolKey calldata _poolKey,
            uint24 _emaWindow,
            uint32 _latency,
            uint128 index
        ) external;
        function unlockCallback(bytes calldata data) external returns (bytes memory encodedDelta);
        function UPGRADE_INTERFACE_VERSION() external view returns (string memory);
        function proxiableUUID() external view returns (bytes32);
        function upgradeToAndCall(address newImplementation, bytes calldata data) external payable;
        function transferOwnership(address newOwner) external payable;
        function renounceOwnership() external payable;
        function requestOwnershipHandover() external payable;
        function cancelOwnershipHandover() external payable;
        function completeOwnershipHandover(address pendingOwner) external payable;
        function ownershipHandoverExpiresAt(address pendingOwner) external view returns (uint256 result);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  PerpFactory
    // ═══════════════════════════════════════════════════════════════════

    /// The `v1.0.0` factory: creates a market proxy, its pool on the
    /// chain's `PerpGuardHook`, and its withdrawal controller.
    #[sol(rpc)]
    interface PerpFactory {
        /// A market created. No name, symbol or URI: positions are not NFTs.
        event PerpCreated(
            address perp,
            bytes32 poolId,
            Modules modules,
            uint256 initialIndex,
            uint24 emaWindow,
            uint32 latency,
            uint256 protocolFee,
            uint160 sqrtPriceX96,
            int24 tick,
            address owner
        );
        event PerpImplementationSet(address indexed previousImplementation, address indexed implementation);

        error BeaconIndexUnreadable();
        error EmaWindowTooLow();
        error InvalidPerpImplementation();
        error NotAContract();
        error NotPoolManager();
        error NotProtocolOwner();
        error PriceOutsideRange();
        error StaleBeaconIndex();

        function createPerp(
            address owner,
            address governor,
            Modules calldata modules,
            uint24 emaWindow,
            uint32 latency,
            bytes32 salt,
            WithdrawalConfig calldata withdrawalConfig
        ) external returns (address perp);
        function perpImplementation() external view returns (address);
        function setPerpImplementation(address implementation) external;
        function unlockCallback(bytes calldata data) external returns (bytes memory);
        function ACCOUNTING_TOKEN_IMPL() external view returns (address);
        function PERP_GUARD_HOOK() external view returns (address);
        function PROTOCOL_FEE_MANAGER() external view returns (address);
        function WITHDRAWAL_CONTROLLER_IMPL() external view returns (address);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Beacon (beacons f762da1)
    // ═══════════════════════════════════════════════════════════════════

    /// The index a `v1.0.0` market reads. `index()` now returns the time
    /// of the last update beside the value; a market refuses an index
    /// older than its `latency()`.
    #[sol(rpc)]
    interface IBeacon {
        event IndexUpdated(uint256 index);
        function index() external view returns (uint256 index, uint256 updatedAt);
        function twAvg(uint32 secondsAgo) external view returns (uint256);
        function increaseCardinalityCap(uint16 newCap) external;
    }
}

/// The `v1.0.0` lock. The tables are `forge inspect` output at contracts
/// tag `v1.0.0` (`0df4208`, solc 0.8.30) and beacons `f762da1`, copied by
/// a script, not by hand. Each binding is compared with its table both
/// ways, so a binding the source lacks and a source item the binding lacks
/// both fail. A module binding is the `I*` interface whole plus the views
/// and setter events named in its table.
#[cfg(test)]
mod abi_lock {
    use std::collections::BTreeMap;

    use alloy::primitives::hex;
    use alloy::sol_types::SolStruct;

    use super::*;

    /// A binding's `(signature, selector or topic)` pairs, by signature.
    fn bound<S: AsRef<[u8]>>(signatures: &[&str], selectors: &[S]) -> BTreeMap<String, String> {
        assert_eq!(signatures.len(), selectors.len());
        signatures
            .iter()
            .zip(selectors)
            .map(|(s, sel)| (s.to_string(), hex::encode(sel)))
            .collect()
    }

    fn table(rows: &[(&str, &str)]) -> BTreeMap<String, String> {
        let map: BTreeMap<String, String> = rows
            .iter()
            .map(|(s, h)| (s.to_string(), h.to_string()))
            .collect();
        assert_eq!(map.len(), rows.len(), "a duplicate row");
        map
    }

    // forge inspect Perp methodIdentifiers
    const PERP_FUNCTIONS: &[(&str, &str)] = &[
        ("UPGRADE_INTERFACE_VERSION()", "ad3cb1cc"),
        ("abdicate(bytes4)", "b2e32848"),
        ("abdicated(bytes4)", "e470b8bc"),
        ("accountedTvl()", "3e7eea36"),
        (
            "adjustMaker((uint256,int128,int128,uint256,uint256,uint64))",
            "7bd08a1a",
        ),
        (
            "adjustTaker((uint256,int128,int256,uint256,uint256,bool,uint64))",
            "4c124b86",
        ),
        ("backstopMaker(uint256,uint128,address,uint64)", "2965081d"),
        ("backstopTaker(uint256,uint128,address,uint64)", "19eda872"),
        ("cancelOwnershipHandover()", "54d1f13d"),
        ("capacity()", "5cfc1a51"),
        ("claimSurplus(address)", "b77dfe03"),
        ("collectCreatorFees(address)", "7d31dcc0"),
        ("collectProtocolFees(address)", "2a54db01"),
        ("completeOwnershipHandover(address)", "f04e283e"),
        ("cumulatives()", "ddaf429d"),
        ("currency0()", "79f1232b"),
        ("currency1()", "10d737b8"),
        ("donate(uint128)", "2f07ad80"),
        ("emaWindow()", "e51493bc"),
        ("feeFund()", "e394732b"),
        ("fundingState()", "b25cdccf"),
        ("governor()", "0c340a24"),
        ("hooks()", "cd7033c4"),
        (
            "initialize(address,uint256,address,address,address,(address,address,address,address,address,address),(address,address,uint24,int24,address),uint24,uint32,uint128)",
            "3371dba7",
        ),
        ("latency()", "7c990b07"),
        ("liquidateMaker(uint256,address,uint128)", "14ca0f4c"),
        ("liquidateTaker(uint256,address,uint128)", "bfb4b1c7"),
        ("lowerTvlCap(uint256)", "884c6ac2"),
        ("makerDetails(uint256)", "3f02ad20"),
        ("modules()", "f7e80e98"),
        (
            "moveMaker((uint256,int24,int24,uint128,uint256,uint256,uint256,uint256,int128,uint64))",
            "e2471c97",
        ),
        ("nextPosId()", "cf95061a"),
        ("openInterest()", "fa5a2e62"),
        (
            "openMaker((address,uint128,int24,int24,uint128,uint256,uint256,uint64))",
            "dbf66cd6",
        ),
        (
            "openTaker((address,uint128,int256,uint256,uint256,uint64))",
            "1d920967",
        ),
        ("owner()", "8da5cb5b"),
        ("ownershipHandoverExpiresAt(address)", "fee81cf4"),
        ("permissionlessDonationsEnabled()", "f03570c8"),
        ("physicalCloseCapacity()", "b526e5ce"),
        ("poolId()", "3e0dc34e"),
        ("poolKey()", "182148ef"),
        ("poolState()", "641ad8a9"),
        ("positionOwner(uint256)", "b3060d36"),
        ("positions(uint256)", "99fbab88"),
        ("previewPosition(uint256)", "4dd7c2a9"),
        ("protocolFee()", "b0e21e8a"),
        ("protocolFeeManager()", "a70dbaeb"),
        ("proxiableUUID()", "52d1902d"),
        ("rates()", "43f48fbd"),
        ("readBeaconIndex(address)", "f382e606"),
        ("renounceOwnership()", "715018a6"),
        ("requestOwnershipHandover()", "25692962"),
        ("setBeacon(address)", "d42afb56"),
        ("setEmaWindow(uint24)", "1b542dd0"),
        ("setFeesModule(address)", "f7e39255"),
        ("setFundingModule(address)", "348519fe"),
        ("setGovernor(address)", "c42cf535"),
        ("setLatency(uint32)", "a8f3152b"),
        ("setMarginRatiosModule(address)", "717f1709"),
        ("setPermissionlessDonationsEnabled(bool)", "77cace9a"),
        ("setPriceImpactModule(address)", "a79cae0c"),
        ("setPricingModule(address)", "05d719f1"),
        ("setTvlCap(uint256)", "dd0290e2"),
        ("setWithdrawalController(address)", "b9967cc7"),
        ("solvencyState()", "7085ac95"),
        ("syncProtocolFee()", "46a79d33"),
        ("takerDetails(uint256)", "a681fa2d"),
        ("touch()", "a55526db"),
        ("transferOwnership(address)", "f2fde38b"),
        ("tvlCap()", "3f23fa1a"),
        ("unlockCallback(bytes)", "91dd7346"),
        ("upgradeToAndCall(address,bytes)", "4f1ef286"),
        ("utilizationBand()", "2d04d2f5"),
        ("withdrawalController()", "b7d93508"),
    ];

    // forge inspect Perp events, signatures from forge inspect Perp abi
    const PERP_EVENTS: &[(&str, &str)] = &[
        (
            "Abdicate(bytes4)",
            "a8480b135a28741e8c059528bf8f82de70278485fb86bfc816a183801f0a8570",
        ),
        (
            "BadDebtAccounted(uint256,uint256,uint256,uint128)",
            "dbcafd00395fff98625ed30d465cca9eede44a0dae950cac30d32fb9d92e0487",
        ),
        (
            "BeaconRecoveryUsed(address,address)",
            "4987751c1c07dbbb01e8e7ce4517ade302a475935245db8e6a97075e66399515",
        ),
        (
            "CapacityUpdated((uint128,uint128))",
            "fe42fdaaba337eb89d0a1ba85925ff87357139fe3dfacc940d0cf9566b548d40",
        ),
        (
            "CreatorFeesCollected(uint256,address)",
            "321d1b28d93d89bfda0b914e8665da62be76431dd78e6a1ab722de08e945b369",
        ),
        (
            "CumulativesAccrued((int256,int256,uint256,uint256,uint256,uint256,uint256,int256))",
            "2cd36955b37db3698acccccb5d9a0413aefdfabc9c00742d1fab35fbdb47a149",
        ),
        (
            "Donated(address,uint128,uint128,uint80)",
            "af6f0ffd55e91192427c83d24821702e6ed9b3dfacf969394281590a5ed00d92",
        ),
        (
            "Initialized(uint64)",
            "c7f505b2f371ae2175ee4913f4499e1f2633a7b5936321eed1cdaeb6115181d2",
        ),
        (
            "LossSocialized(uint256,uint256,uint128)",
            "7995a51d3105403dcb7085f1f69f37b83f31f3ccd8ae21abdff53109eab4c2c5",
        ),
        (
            "MakerAdjusted(uint256,address,int128,int128,int256,uint256,int256,uint128,uint128,int256,(int256,int256))",
            "867c59df8ef3ea10a16a572ab85d718d1972f7feabfe847d83d818e60929ca1e",
        ),
        (
            "MakerBackstopped(uint256,uint128,address,int256,uint256,uint128,int256,uint128,int256,(int256,int256))",
            "4b6aac8d4ce998ced2f6f6e6cffb9cef0aa49bdd783143813b6845e14ff871ba",
        ),
        (
            "MakerClosed(uint256,address,int128,int128,int256,uint256,int256,uint128,uint128,int256,(int256,int256))",
            "181a101fe3eb4e5adcaea5a4409bc933dd26341a7c7179252d17b863b99fa671",
        ),
        (
            "MakerConverted(uint256,address,int128,int128,int256,uint256,int256,uint128,uint128,int256,(int256,int256))",
            "5dfddc3ff94e76acf24a54b00d5a855ffd7704f0f7ff0eb2cb54f65df66bc8cc",
        ),
        (
            "MakerLiquidated(uint256,address,uint128,uint256)",
            "ab8324c9ed835b7bd8a9b0411a39e508a1df4fa1707f159db06897b3dfee7f44",
        ),
        (
            "MakerMoved(uint256,address,int24,int24,int24,int24,uint128,uint128,int128,int256,uint256)",
            "c475caf3ea500603e349cc0bb3f48dea0fe01b76bb5f65c0a35a62f34e59de7e",
        ),
        (
            "MakerOpened(uint256,address,int24,int24,uint128,int256,int256,(int256,int256))",
            "ea3ba68e0eb61c62cdf81a21ee41f306e2551414cbb0ed6cda3aad946e0eb738",
        ),
        (
            "MarginTransferred(int128,uint128)",
            "7a819589c92e56d0b6a9172b705c160f9799202dbfbfcb022d2af6136fd813ed",
        ),
        (
            "OpenInterestUpdated((uint128,uint128))",
            "2cfcd40a0a33efd1a2cc618e2df899730c84f9df55549afc46d02937b59bf3e6",
        ),
        (
            "OwnershipHandoverCanceled(address)",
            "fa7b8eab7da67f412cc9575ed43464468f9bfbae89d1675917346ca6d8fe3c92",
        ),
        (
            "OwnershipHandoverRequested(address)",
            "dbf36a107da19e49527a7176a1babf963b4b0ff8cde35ee35d6cd8f1f9ac7e1d",
        ),
        (
            "OwnershipTransferred(address,address)",
            "8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e0",
        ),
        (
            "PermissionlessDonationsEnabledSet(bool)",
            "480a59fc06c0cc04ac869c329644bcd76fbb3cc6d0e0541974fc8b9c78489297",
        ),
        (
            "PhysicalCapacityUpdated((uint256,uint256))",
            "d94f7512df329182d51ae5d209a5d368f4e5c6e4442ed632bc572ead1aa7b613",
        ),
        (
            "PositionOwnerUpdated(uint256,address,address)",
            "84d4f777b8104806c9eb3a78d71a668383a852c0d4eceb873c808f805a718169",
        ),
        (
            "ProtocolFeeSynced(uint256)",
            "4a818a8cd3b8e45429bbca951557a3b2d9a8e5a38fd2c2538e40c5cd60f3454e",
        ),
        (
            "ProtocolFeesCollected(uint256,address)",
            "ccba73dd28c18e918f543a3a2bd58894faedeac726d5950c97d3bf6b158bb0ed",
        ),
        (
            "QuoteFolded(uint256,int256)",
            "481ed1985f6be40e5d23a087237aadd15cd8aa22302ab90d719de1525db56182",
        ),
        (
            "RatesAndEmasRefreshed((int88,uint64,uint64,uint40,uint256),uint128,(uint128,uint128))",
            "24ade31ed405c75b2486b61b581337ddebf24e6601e85b988f30eaa1fb7ab9eb",
        ),
        (
            "SetBeacon(address)",
            "eda478a82221a6140112aa7f09cae9d34ba9ef5afe98665b4b91af27bfae4b80",
        ),
        (
            "SetEmaWindow(uint24)",
            "77df70b09176107b7d4b5118bb6fe5dc2255eeed298a1e5bea303f2256135714",
        ),
        (
            "SetFeesModule(address)",
            "52a6977e4b34f6211e8979d27735f11d62127fde19dac323e94a7b1b3dcc927d",
        ),
        (
            "SetFundingModule(address)",
            "3d9e293475ebd5f1c539a7b7b922bdd95152550f0b65820ee5b1b2dd1546d734",
        ),
        (
            "SetGovernor(address)",
            "bce074c8369e26e70e1ae2f14fc944da352cfe6f52e2de9572f0c9942a24b7fc",
        ),
        (
            "SetLatency(uint32)",
            "1300577283c76cdfc51069370aa55d8e0ddb6bfd89da782643c68be9edcd8777",
        ),
        (
            "SetMarginRatiosModule(address)",
            "9e2c08905af50bef2fe2ea75467e6a980ed96b0d42e3166ea2ddd7da61d0ff99",
        ),
        (
            "SetPriceImpactModule(address)",
            "75f628c85f0f4623da54cc34d09abc7c38b277a46a64acc1b8624aacce3bd4c8",
        ),
        (
            "SetPricingModule(address)",
            "a3c68ccb672060124d2ccfc83677f8c033e5f93ec4eff04e73206a48129d9c28",
        ),
        (
            "SetTvlCap(uint256)",
            "9b83075767c93f683dc38f28b4e8fd725867522b5bbead77b8c397ab04fe21f1",
        ),
        (
            "SetWithdrawalController(address)",
            "cebc60ef35c96db62c3510211a8c5de785b81124f11fa8cdfe442107a6627cfe",
        ),
        (
            "SurplusRecovered(address,uint256)",
            "8d062d570e15e665f57873e5745c84e024c3b6a4c94cfcda6af54a9ccd416af7",
        ),
        (
            "SwapFeesRemovedFromMargin(uint256,uint128)",
            "cdd65d1af3124184c38ab395bdbac20a4a943f702dec80a0e0549db1aff118a6",
        ),
        (
            "TakerAdjusted(uint256,address,int128,(int256,uint256,int256,uint256,uint256,uint256,uint256,uint256),int256,uint256,int256,uint128,int256)",
            "c7ca5680aa92db12f83b06a5dcfa8e623c7d7e1e97ca9d16d3a11e1be98f98f8",
        ),
        (
            "TakerBackstopped(uint256,uint128,address,int256,uint256,int256,uint128,int256)",
            "c6779121432107396b73135fce3baa93eff302c7986be1b76d17e7b653c3db84",
        ),
        (
            "TakerClosed(uint256,address,int128,(int256,uint256,int256,uint256,uint256,uint256,uint256,uint256),int256,uint256,int256,uint128,int256)",
            "f75081db50efa183348fcf2b16f56ae49f811be3f2009f0ed5ad4f5fb5f49bf2",
        ),
        (
            "TakerLiquidated(uint256,address,uint128,uint256)",
            "74e6ccfa6bab3e020c58093b8f8adbc3168136b94d24e8f27a6439c31c023ba6",
        ),
        (
            "TakerOpened(uint256,address,uint128,(int256,uint256,int256,uint256,uint256,uint256,uint256,uint256),int256)",
            "1fa3ed5129b3c75e604c7878b4be8f6b301c308ece67791254ec60ab82d77c12",
        ),
        (
            "TickDeleted(int24)",
            "39ef6cb8e0f1a6ae2b33f77866aa58dbcb643ae44498b9dafc036e4d5b8cf607",
        ),
        (
            "TickInitialized(int24,int256,int256)",
            "c5aab48c1b38d15329a34f12443844e49367404a3a1728de883b578333fb40c9",
        ),
        (
            "TicksCrossed(int24,int24,bool)",
            "7155fa98d64ad518fe22d41dc9573e6abc6df89d1b88a244c87336a95084e8ad",
        ),
        (
            "Upgraded(address)",
            "bc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b",
        ),
        (
            "UtilizationAccrued(uint128,uint128,uint128,uint128,uint256,uint256,uint256,uint256)",
            "0a026975f8f850073f167a5303654352c2bb60ce6a5c716d1746464d80b2d806",
        ),
        (
            "UtilizationReserveRealized(uint256,uint256,uint256,uint256)",
            "4464625b30cdfba29ec835541a73a29f7636e875844a1d976860a0cbcfc78ae9",
        ),
        (
            "UtilizationReserveRemovedFromMargin(uint256,uint128)",
            "deae0bb29b3ec6b6beea436cf899b41bc4143055ed2f06679501f3b502d41bf6",
        ),
    ];

    // forge inspect errors: src/libraries/Errors.sol:Errors, Perp, and the linked Perp*Logic libraries
    const PERP_ERRORS: &[(&str, &str)] = &[
        ("Abdicated()", "281df4aa"),
        ("AddressEmptyCode(address)", "9996b315"),
        ("AggregateSwapFeesTooHigh()", "d08a3568"),
        ("AlreadyInitialized()", "0dc149f0"),
        ("AmountNotDecreased()", "26a19fe0"),
        ("BeaconIndexUnreadable()", "6c776dda"),
        ("ClaimIsPaused()", "17e94742"),
        ("ClaimNotPaused()", "af67ab56"),
        ("ClaimNotPending()", "48f8d3b1"),
        ("ClaimNotReady()", "57155ae3"),
        ("DeadlineExpired()", "1ab7da6b"),
        ("DonationsRestricted()", "3532c744"),
        ("ERC1967InvalidImplementation(address)", "4c9c8ce3"),
        ("ERC1967NonPayable()", "b398979f"),
        ("EmaWindowTooLow()", "c657a809"),
        ("FailedCall()", "d6bda275"),
        ("HealthNotImproved()", "0e8c9a42"),
        ("IdenticalTicks()", "63c8ca5d"),
        ("InsufficientLiquidityToFill()", "ed126f97"),
        ("InvalidCallbackAction()", "99571bc0"),
        ("InvalidInitialization()", "f92ee8a9"),
        ("LongCloseCapacityExceeded()", "84d09fb1"),
        ("MarginRatioTooLow()", "b2c649db"),
        ("MarginTooHigh()", "00562c4f"),
        ("MarginTooLow()", "38f5e1a7"),
        ("MaxAmtExceeded()", "24f14ba6"),
        ("MaxTotalFeeExceeded()", "c89df80f"),
        ("MinAmtUnmet()", "0470009e"),
        ("NegativeEquity()", "fece0035"),
        ("NegativeMargin()", "e94943ae"),
        ("NewOwnerIsZeroAddress()", "7448fbae"),
        ("NoHandoverRequest()", "6f5e8818"),
        ("NoSurplus()", "c0ef17d3"),
        ("NoSystemFunds()", "5c64c19c"),
        ("NonMakerPosition()", "dbcefbf3"),
        ("NonTakerPosition()", "12d39e8a"),
        ("NotGovernor()", "ee3675d4"),
        ("NotGuardian()", "ef6d0f02"),
        ("NotInitializing()", "d7e6bcf8"),
        ("NotLiquidatable()", "ddeb79ba"),
        ("NotPerp()", "0a0a1dbe"),
        ("NotPoolManager()", "ae18210a"),
        ("PositionDoesNotExist()", "f7b3b391"),
        ("PriceImpactTooHigh()", "fb30d03a"),
        ("PriceOutsideRange()", "e10dffe6"),
        ("ReduceOnlyViolated()", "46e46f2e"),
        ("ReleasesArePaused()", "7a539c7c"),
        ("ShortCloseCapacityExceeded()", "deacb1fc"),
        ("StaleBeaconIndex()", "61200552"),
        ("TicksOutOfBounds()", "d6acf910"),
        ("TvlCapExceeded()", "b0e52638"),
        ("UUPSUnauthorizedCallContext()", "e07c8dba"),
        ("UUPSUnsupportedProxiableUUID(bytes32)", "aa1d49a4"),
        ("Unauthorized()", "82b42900"),
        ("UnauthorizedCaller()", "5c427cd9"),
        ("WindowNotIncreased()", "c705313f"),
        ("ZeroAddress()", "d92e233d"),
        ("ZeroAmount()", "1f2a2005"),
        ("ZeroCapacity()", "0bcb6784"),
        ("ZeroDelta()", "6f0f5899"),
        ("ZeroLiquidity()", "10074548"),
        ("ZeroWindow()", "4acbbb65"),
    ];

    // forge inspect PerpFactory methodIdentifiers
    const FACTORY_FUNCTIONS: &[(&str, &str)] = &[
        ("ACCOUNTING_TOKEN_IMPL()", "0b4e2e5c"),
        ("PERP_GUARD_HOOK()", "14b08bf2"),
        ("PROTOCOL_FEE_MANAGER()", "36c7c39f"),
        ("WITHDRAWAL_CONTROLLER_IMPL()", "e2f44868"),
        (
            "createPerp(address,address,(address,address,address,address,address,address),uint24,uint32,bytes32,(uint128,uint128,uint48))",
            "2fdabe72",
        ),
        ("perpImplementation()", "fa62e317"),
        ("setPerpImplementation(address)", "45b68aca"),
        ("unlockCallback(bytes)", "91dd7346"),
    ];

    // forge inspect PerpFactory events
    const FACTORY_EVENTS: &[(&str, &str)] = &[
        (
            "PerpCreated(address,bytes32,(address,address,address,address,address,address),uint256,uint24,uint32,uint256,uint160,int24,address)",
            "9e1124db88459a4e932f32dd28acc4bc6f1a1a2c4dde3f2778ef3dfd90189b6d",
        ),
        (
            "PerpImplementationSet(address,address)",
            "88033614aeb2b26190836fee8a7224216cc687bbb661d95460c89f82545ada00",
        ),
    ];

    // forge inspect PerpFactory errors
    const FACTORY_ERRORS: &[(&str, &str)] = &[
        ("BeaconIndexUnreadable()", "6c776dda"),
        ("EmaWindowTooLow()", "c657a809"),
        ("InvalidPerpImplementation()", "a457695f"),
        ("NotAContract()", "09ee12d5"),
        ("NotPoolManager()", "ae18210a"),
        ("NotProtocolOwner()", "fb6fc0b7"),
        ("PriceOutsideRange()", "e10dffe6"),
        ("StaleBeaconIndex()", "61200552"),
    ];

    // forge inspect Fees methodIdentifiers (IFees whole, plus the views bound)
    const FEES_FUNCTIONS: &[(&str, &str)] = &[
        ("cFee()", "4cfebaf1"),
        (
            "fees(uint128,(uint128,uint128),(uint128,uint128))",
            "709db758",
        ),
        ("insFee()", "ebc29d07"),
        ("liqFee()", "3fb0da64"),
        ("lpFeeConfig()", "49fca378"),
        ("maxSwapFee()", "66edecce"),
        ("slopeAfterTarget()", "5263bdd2"),
        ("slopeBeforeTarget()", "44e4fb9e"),
        ("targetUtilization()", "d22cedc0"),
        ("utilFees(uint256,uint256)", "87ba6600"),
    ];

    // forge inspect Fees events (the setters' events)
    const FEES_EVENTS: &[(&str, &str)] = &[
        (
            "LiqFeeSet(uint24)",
            "3dba64823257950bae05c50410806f44a95bd6ceae55591c53e06a84237f2f9e",
        ),
        (
            "LpFeeConfigSet((uint24,uint24,uint128,uint24,uint24))",
            "a955da548d71df42dee5faf0ec2b4f438a1f1d4d331d44cb8a020096faf134f6",
        ),
        (
            "SwapFeesSet(uint24,uint24)",
            "a52e279a9c4ed8ff7beccca9a9f7a38075d2c5cbaac8e5b5032d95e32504ebc5",
        ),
        (
            "UtilFeeCurveSet(uint256,uint256,uint256)",
            "09d8cee5cb693f57be59192af0feefd9034f141f5475794ba8de46ed38eb9f02",
        ),
    ];

    // forge inspect Funding methodIdentifiers (IFunding whole, plus the views bound)
    const FUNDING_FUNCTIONS: &[(&str, &str)] = &[
        (
            "funding((uint128,uint128),(uint128,uint128),uint256)",
            "0a0185c6",
        ),
        ("fundingRateMultiplier()", "d8aaaf80"),
        ("maxFundingPerDayPercent()", "33d4ed61"),
    ];

    // forge inspect Funding events (the setters' events)
    const FUNDING_EVENTS: &[(&str, &str)] = &[
        (
            "FundingRateMultiplierSet(uint256)",
            "86cb57039f715b6ea66d6e4bd305014d8402dedcd716db9369a04a3068fe239c",
        ),
        (
            "MaxFundingPerDayPercentSet(uint256)",
            "3b41aebddf52f3c121668d127cabc05c515ab0c36159496b19cd7969c6d6e154",
        ),
    ];

    // forge inspect MarginRatios methodIdentifiers (IMarginRatios whole, plus the views bound)
    const MARGINRATIOS_FUNCTIONS: &[(&str, &str)] = &[
        ("UPDATE_DELAY()", "ecf25e0f"),
        ("makerMarginRatios()", "b583a929"),
        ("pendingMakerRatios()", "0d383702"),
        ("pendingTakerRatios()", "2419263a"),
        ("takerMarginRatios()", "97019e5c"),
    ];

    // forge inspect MarginRatios events (the setters' events)
    const MARGINRATIOS_EVENTS: &[(&str, &str)] = &[
        (
            "MakerMarginRatiosCancelled((uint24,uint24,uint24),uint48)",
            "98f0c681e9f4b7723980d994f5528cf39edefd8c61877f8e43a613d821798f9d",
        ),
        (
            "MakerMarginRatiosExecuted((uint24,uint24,uint24),uint48)",
            "cbb9ceedfd6768a198700df28689535f37a1c4a22dac01a9d4b454c1e8ea149e",
        ),
        (
            "MakerMarginRatiosScheduled((uint24,uint24,uint24),uint48)",
            "76c8945629dc3da7ebdbfe928e56b589fe7571766c42482f38dd5bc54b7a4e21",
        ),
        (
            "MakerMarginRatiosSet((uint24,uint24,uint24))",
            "5dc3af58f0979a8f04be327a43d1f2350326390e22605a7e7d4ff60b416bf531",
        ),
        (
            "TakerMarginRatiosCancelled((uint24,uint24,uint24),uint48)",
            "d8811187a22b5faaf8f5f91ec78f69aee59696b68e2565c18c36a55ea3867209",
        ),
        (
            "TakerMarginRatiosExecuted((uint24,uint24,uint24),uint48)",
            "ed288199ed8b0f69fc077604ff57d21d5107e07a830aa9bd4ec3bcaf7b64395a",
        ),
        (
            "TakerMarginRatiosScheduled((uint24,uint24,uint24),uint48)",
            "8e47c6e4759a36ad0531c5efcebe9089be373438239e4603b1986d0a6ecc8038",
        ),
        (
            "TakerMarginRatiosSet((uint24,uint24,uint24))",
            "e225d751c24c9e85989d34945e94074a8ecff7130a2202818cfe0152af692b35",
        ),
    ];

    // forge inspect PriceImpact methodIdentifiers (IPriceImpact whole, plus the views bound)
    const PRICEIMPACT_FUNCTIONS: &[(&str, &str)] = &[
        ("priceImpactBound()", "306b99fb"),
        (
            "sqrtPriceBounds(uint256,uint256,uint256,uint256,uint256)",
            "2fbcd3b4",
        ),
    ];

    // forge inspect PriceImpact events (the setters' events)
    const PRICEIMPACT_EVENTS: &[(&str, &str)] = &[(
        "PriceImpactBoundSet(uint256)",
        "a473ea81100ed565c63e0d938a84c774f3cd13f2e138c422ae0122fdc93713ea",
    )];

    // forge inspect Pricing methodIdentifiers (IPricing whole, plus the views bound)
    const PRICING_FUNCTIONS: &[(&str, &str)] = &[
        ("fairPrice(uint256,uint256,uint256,uint256)", "0fa23b8a"),
        ("maxPremiumRatio()", "e111cd6c"),
    ];

    // forge inspect Pricing events (the setters' events)
    const PRICING_EVENTS: &[(&str, &str)] = &[(
        "MaxPremiumRatioSet(uint256)",
        "4596d451337db23f6b4d6bfd562101c1d86dda6689e3c30d47d4466c8d984916",
    )];

    // out/IBeacon.sol/IBeacon.json methodIdentifiers (beacons f762da1; forge inspect IBeacon is ambiguous)
    const BEACON_FUNCTIONS: &[(&str, &str)] = &[
        ("increaseCardinalityCap(uint16)", "9f2f9399"),
        ("index()", "2986c0e5"),
        ("twAvg(uint32)", "4686cef7"),
    ];

    // forge inspect IdentityBeacon events
    const BEACON_EVENTS: &[(&str, &str)] = &[(
        "IndexUpdated(uint256)",
        "acfc085c9be45d2b3f9e5c09a19d4a95749cc16939519c13e090de3a4cb192c6",
    )];

    // EIP-712 strings built from the struct components of forge inspect <C> abi
    // (Perp, PerpFactory, Fees, MarginRatios); TickInfo from forge inspect Perp storageLayout.
    const STRUCTS: &[(&str, &str)] = &[
        (
            "AdjustMakerParams",
            "AdjustMakerParams(uint256 posId,int128 marginDelta,int128 liquidityDelta,uint256 amt0Limit,uint256 amt1Limit,uint64 deadline)",
        ),
        (
            "AdjustTakerParams",
            "AdjustTakerParams(uint256 posId,int128 marginDelta,int256 perpDelta,uint256 amt1Limit,uint256 maxTotalFee,bool reduceOnly,uint64 deadline)",
        ),
        ("Capacity", "Capacity(uint128 long,uint128 short)"),
        (
            "Cumulatives",
            "Cumulatives(int256 fundingX96,int256 fundingDivSqrtPX192,uint256 lpFeeGrowthGlobalX128,uint256 longUtilPaymentsX96,uint256 shortUtilPaymentsX96,uint256 longReserveUtilPaymentsX96,uint256 shortReserveUtilPaymentsX96,int256 fundingCorrectionX128)",
        ),
        (
            "FeeFund",
            "FeeFund(uint80 insurance,uint80 creatorFees,uint80 protocolFees)",
        ),
        (
            "FundingState",
            "FundingState(int256 baseX96,int256 inventoryX96,uint256 makerLiquidity)",
        ),
        (
            "LpFeeConfig",
            "LpFeeConfig(uint24 startFee,uint24 targetFee,uint128 liquidityDecay,uint24 volatilityScaler,uint24 maxFeeMultiplier)",
        ),
        (
            "Maker",
            "Maker(int24 tickLower,int24 tickUpper,uint128 liquidity,uint256 lastLpFeeGrowthInsideX128,MakerFunding lastCumlFunding,int256 lastFundingCorrectionX128)MakerFunding(int256 belowX96,int256 withinInventoryX192)",
        ),
        (
            "MakerFunding",
            "MakerFunding(int256 belowX96,int256 withinInventoryX192)",
        ),
        (
            "Modules",
            "Modules(address beacon,address fees,address funding,address marginRatios,address priceImpact,address pricing)",
        ),
        (
            "MoveMakerParams",
            "MoveMakerParams(uint256 posId,int24 tickLower,int24 tickUpper,uint128 liquidity,uint256 removeAmt0Min,uint256 removeAmt1Min,uint256 addAmt0Max,uint256 addAmt1Max,int128 marginDelta,uint64 deadline)",
        ),
        ("OpenInterest", "OpenInterest(uint128 long,uint128 short)"),
        (
            "OpenMakerParams",
            "OpenMakerParams(address holder,uint128 margin,int24 tickLower,int24 tickUpper,uint128 liquidity,uint256 maxAmt0In,uint256 maxAmt1In,uint64 deadline)",
        ),
        (
            "OpenTakerParams",
            "OpenTakerParams(address holder,uint128 margin,int256 perpDelta,uint256 amt1Limit,uint256 maxTotalFee,uint64 deadline)",
        ),
        (
            "PhysicalCapacity",
            "PhysicalCapacity(uint256 longCloseX64,uint256 shortCloseX64)",
        ),
        (
            "PoolKey",
            "PoolKey(address currency0,address currency1,uint24 fee,int24 tickSpacing,address hooks)",
        ),
        (
            "Position",
            "Position(int256 delta,uint128 margin,int256 lastCumlFundingX96)",
        ),
        (
            "PositionPreview",
            "PositionPreview(bool isMaker,uint256 markPrice,int256 settledMargin,uint256 pendingLpFees,int256 pendingFunding,uint256 pendingUtil,uint256 pendingReserveUtil,uint256 value,int256 unrealizedPnl,int256 equity,bool meetsInitialMargin,bool liquidatable,bool backstoppable,uint256 nominalLiqFee,uint256 payableLiqFee)",
        ),
        ("PricePair", "PricePair(uint128 ammPrice,uint128 index)"),
        (
            "Rates",
            "Rates(int88 fundingPerDay,uint64 longUtilFeePerDay,uint64 shortUtilFeePerDay,uint40 lastTouch,uint256 lastMark)",
        ),
        ("Ratios", "Ratios(uint24 init,uint24 liq,uint24 backstop)"),
        (
            "SolvencyState",
            "SolvencyState(uint128 badDebt,uint128 totalMargin)",
        ),
        (
            "SwapResult",
            "SwapResult(int256 delta,uint256 ammPrice,int256 totalFeeAmt,uint256 lpFeeAmt,uint256 protocolFeeAmt,uint256 creatorFeeAmt,uint256 insuranceFeeAmt,uint256 feeFundFeeAmt)",
        ),
        (
            "Taker",
            "Taker(uint256 lastUtilPaymentsX96,uint256 lastReserveUtilPaymentsX96)",
        ),
        (
            "TickInfo",
            "TickInfo(int256 cumlFundingOppX96,int256 cumlFundingDivSqrtPOppX192,uint256 lpFeeGrowthOutsideX128)",
        ),
        (
            "UtilizationBand",
            "UtilizationBand(uint128 lower,uint128 upper)",
        ),
        (
            "WithdrawalConfig",
            "WithdrawalConfig(uint128 capacity,uint128 refillPerSecond,uint48 challengeWindow)",
        ),
    ];

    #[test]
    fn perp_matches_forge_inspect() {
        use Perp::{PerpCalls, PerpErrors, PerpEvents};
        assert_eq!(
            bound(PerpCalls::SIGNATURES, PerpCalls::SELECTORS),
            table(PERP_FUNCTIONS)
        );
        assert_eq!(
            bound(PerpEvents::SIGNATURES, PerpEvents::SELECTORS),
            table(PERP_EVENTS)
        );
        assert_eq!(
            bound(PerpErrors::SIGNATURES, PerpErrors::SELECTORS),
            table(PERP_ERRORS)
        );
    }

    #[test]
    fn factory_matches_forge_inspect() {
        use PerpFactory::{PerpFactoryCalls, PerpFactoryErrors, PerpFactoryEvents};
        assert_eq!(
            bound(PerpFactoryCalls::SIGNATURES, PerpFactoryCalls::SELECTORS),
            table(FACTORY_FUNCTIONS)
        );
        assert_eq!(
            bound(PerpFactoryEvents::SIGNATURES, PerpFactoryEvents::SELECTORS),
            table(FACTORY_EVENTS)
        );
        assert_eq!(
            bound(PerpFactoryErrors::SIGNATURES, PerpFactoryErrors::SELECTORS),
            table(FACTORY_ERRORS)
        );
    }

    #[test]
    fn modules_match_forge_inspect() {
        use IFees::{IFeesCalls, IFeesEvents};
        use IFunding::{IFundingCalls, IFundingEvents};
        use IMarginRatios::{IMarginRatiosCalls, IMarginRatiosEvents};
        use IPriceImpact::{IPriceImpactCalls, IPriceImpactEvents};
        use IPricing::{IPricingCalls, IPricingEvents};
        assert_eq!(
            bound(IFeesCalls::SIGNATURES, IFeesCalls::SELECTORS),
            table(FEES_FUNCTIONS)
        );
        assert_eq!(
            bound(IFeesEvents::SIGNATURES, IFeesEvents::SELECTORS),
            table(FEES_EVENTS)
        );
        assert_eq!(
            bound(IFundingCalls::SIGNATURES, IFundingCalls::SELECTORS),
            table(FUNDING_FUNCTIONS)
        );
        assert_eq!(
            bound(IFundingEvents::SIGNATURES, IFundingEvents::SELECTORS),
            table(FUNDING_EVENTS)
        );
        assert_eq!(
            bound(
                IMarginRatiosCalls::SIGNATURES,
                IMarginRatiosCalls::SELECTORS
            ),
            table(MARGINRATIOS_FUNCTIONS)
        );
        assert_eq!(
            bound(
                IMarginRatiosEvents::SIGNATURES,
                IMarginRatiosEvents::SELECTORS
            ),
            table(MARGINRATIOS_EVENTS)
        );
        assert_eq!(
            bound(IPriceImpactCalls::SIGNATURES, IPriceImpactCalls::SELECTORS),
            table(PRICEIMPACT_FUNCTIONS)
        );
        assert_eq!(
            bound(
                IPriceImpactEvents::SIGNATURES,
                IPriceImpactEvents::SELECTORS
            ),
            table(PRICEIMPACT_EVENTS)
        );
        assert_eq!(
            bound(IPricingCalls::SIGNATURES, IPricingCalls::SELECTORS),
            table(PRICING_FUNCTIONS)
        );
        assert_eq!(
            bound(IPricingEvents::SIGNATURES, IPricingEvents::SELECTORS),
            table(PRICING_EVENTS)
        );
    }

    #[test]
    fn beacon_matches_forge_inspect() {
        use IBeacon::{IBeaconCalls, IBeaconEvents};
        assert_eq!(
            bound(IBeaconCalls::SIGNATURES, IBeaconCalls::SELECTORS),
            table(BEACON_FUNCTIONS)
        );
        assert_eq!(
            bound(IBeaconEvents::SIGNATURES, IBeaconEvents::SELECTORS),
            table(BEACON_EVENTS)
        );
    }

    #[test]
    fn structs_match_forge_inspect() {
        let bound: BTreeMap<String, String> = [
            ("AdjustMakerParams", AdjustMakerParams::eip712_encode_type()),
            ("AdjustTakerParams", AdjustTakerParams::eip712_encode_type()),
            ("Capacity", Capacity::eip712_encode_type()),
            ("Cumulatives", Cumulatives::eip712_encode_type()),
            ("FeeFund", FeeFund::eip712_encode_type()),
            ("FundingState", FundingState::eip712_encode_type()),
            ("LpFeeConfig", LpFeeConfig::eip712_encode_type()),
            ("Maker", Maker::eip712_encode_type()),
            ("MakerFunding", MakerFunding::eip712_encode_type()),
            ("Modules", Modules::eip712_encode_type()),
            ("MoveMakerParams", MoveMakerParams::eip712_encode_type()),
            ("OpenInterest", OpenInterest::eip712_encode_type()),
            ("OpenMakerParams", OpenMakerParams::eip712_encode_type()),
            ("OpenTakerParams", OpenTakerParams::eip712_encode_type()),
            ("PhysicalCapacity", PhysicalCapacity::eip712_encode_type()),
            ("PoolKey", PoolKey::eip712_encode_type()),
            ("Position", Position::eip712_encode_type()),
            ("PositionPreview", PositionPreview::eip712_encode_type()),
            ("PricePair", PricePair::eip712_encode_type()),
            ("Rates", Rates::eip712_encode_type()),
            ("Ratios", Ratios::eip712_encode_type()),
            ("SolvencyState", SolvencyState::eip712_encode_type()),
            ("SwapResult", SwapResult::eip712_encode_type()),
            ("Taker", Taker::eip712_encode_type()),
            ("TickInfo", TickInfo::eip712_encode_type()),
            ("UtilizationBand", UtilizationBand::eip712_encode_type()),
            ("WithdrawalConfig", WithdrawalConfig::eip712_encode_type()),
        ]
        .into_iter()
        .map(|(n, s)| (n.to_string(), s.into_owned()))
        .collect();
        assert_eq!(bound, table(STRUCTS));
    }

    /// The three selectors the build plan names, by `cast sig` as well.
    #[test]
    fn named_selectors() {
        use alloy::sol_types::SolCall;
        assert_eq!(Perp::poolIdCall::SELECTOR, hex!("3e0dc34e"));
        assert_eq!(Perp::previewPositionCall::SELECTOR, hex!("4dd7c2a9"));
        assert_eq!(Perp::openTakerCall::SELECTOR, hex!("1d920967"));
    }
}
