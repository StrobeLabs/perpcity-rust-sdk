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
//! [`PerpFactory`], [`WithdrawalController`] and [`IBeacon`]; a module
//! interface is the `v1.0.0` `I*` interface plus the module's own views.
//! Each interface also has its JSON ABI (`abi::contract()`). The
//! `abi_lock` tests hold every selector, topic and struct string, and
//! every item's names, return types, `indexed` flags and mutability,
//! against `forge inspect` at the tag.
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
    #![sol(abi)]

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
        event LpFeeConfigSet(LpFeeConfig config);
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
        function makerRatios() external view returns (uint24 init, uint24 liq, uint24 backstop);
        function takerRatios() external view returns (uint24 init, uint24 liq, uint24 backstop);

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
    //  WithdrawalController
    // ═══════════════════════════════════════════════════════════════════

    /// A claim's lifecycle (`ClaimStatus`).
    enum ClaimStatus {
        /// No claim has this id.
        None,
        /// Escrowed, not yet released.
        Pending,
        /// Released in full.
        Completed,
        /// Reduced to zero by the guardian.
        Cancelled,
    }

    /// A market's payout escrow and rate limit: a clone of the factory's
    /// `WITHDRAWAL_CONTROLLER_IMPL`, bound to one market at creation and
    /// read from its `withdrawalController()`. A payout that fits the
    /// bucket is instant (`InstantPayout`); any other is a claim
    /// (`ClaimCreated`) that anyone may `release` once its challenge window
    /// has passed. The market's owner is the guardian, its governor loosens.
    #[sol(rpc)]
    interface WithdrawalController {
        event ChallengeWindowSet(uint48 challengeWindow);
        event ClaimCreated(uint256 indexed claimId, address indexed recipient, uint128 amount);
        event ClaimPaused(uint256 indexed claimId);
        event ClaimReduced(
            uint256 indexed claimId,
            uint128 oldAmount,
            uint128 newAmount,
            bytes32 evidenceHash,
            address guardian
        );
        event ClaimReleased(uint256 indexed claimId, address indexed recipient, uint128 amount);
        event ClaimUnpaused(uint256 indexed claimId);
        event ControllerInitialized(
            address indexed perp,
            uint128 capacity,
            uint128 refillPerSecond,
            uint48 challengeWindow
        );
        event InstantPayout(address indexed recipient, uint128 amount, uint128 availableAfter);
        event RateLimitSet(uint128 capacity, uint128 refillPerSecond);
        event ReleasesPaused();
        event ReleasesUnpaused();

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
        error WindowNotIncreased();
        error ZeroAddress();
        error ZeroWindow();

        // ── Views ───────────────────────────────────────────────────

        /// The claim delay, in seconds.
        function challengeWindow() external view returns (uint48);
        /// A claim by id; ids count up from one and are never reused.
        function claims(uint256 claimId) external view returns (
            uint128 originalAmount,
            uint128 currentAmount,
            address recipient,
            uint48 createdAt,
            ClaimStatus status,
            bool paused
        );
        function nextClaimId() external view returns (uint256);
        function perp() external view returns (address);
        /// The instant-payout bucket.
        function rateLimit() external view returns (
            uint128 capacity,
            uint128 available,
            uint128 refillPerSecond,
            uint48 lastUpdated
        );
        function releasesPaused() external view returns (bool);
        /// The USDC held for pending claims.
        function totalReserved() external view returns (uint256);

        // ── Market, release, guardian and governor actions ──────────

        function initialize(address perp, WithdrawalConfig memory config) external;
        function requestPayout(address recipient, uint128 amount) external returns (bool instant);
        function release(uint256 claimId) external;
        function increaseChallengeWindow(uint48 newWindow) external;
        function lowerRateLimit(uint128 newCapacity, uint128 newRefillPerSecond) external;
        function pauseClaim(uint256 claimId) external;
        function pauseReleases() external;
        function reduceClaim(uint256 claimId, uint128 newAmount, bytes32 evidenceHash) external;
        function setChallengeWindow(uint48 newWindow) external;
        function setRateLimit(uint128 newCapacity, uint128 newRefillPerSecond) external;
        function unpauseClaim(uint256 claimId) external;
        function unpauseReleases() external;
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

#[cfg(test)]
mod abi_lock;
