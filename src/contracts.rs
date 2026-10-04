//! On-chain contract bindings generated via Alloy's `sol!` macro.
//!
//! Two contract builds are live on Arbitrum One and the bindings cover both:
//!
//! - **build `58b42b7`** (the markets created before 2026-10-05): every
//!   function and struct in [`Perp`], and the tailed close events in
//!   [`PerpDeployedEvents`] plus `Perp::TakerClosed`;
//! - **`v0.2.2-upgradeable`** (tag `198559a`; the markets on factory
//!   `0x90C8cb83C4257156bf3eE0C9bB8f164B20A04da1`): the same views and
//!   trade entry points, plus what [`PerpV022`] declares — the partial
//!   `liquidate*` selectors, the untailed `TakerClosed`, the hooked pool
//!   and the UUPS surface. It has no `emas()`; the stored pair is read from
//!   storage slot 11 on both builds.
//!
//! Struct shapes, trade and view selectors, `createPerp` and `PerpCreated`
//! are identical across the two. A market's build is read from its pool
//! key: a `58b42b7` pool has no hook, a `v0.2.2` pool carries the
//! `PerpGuardHook`. Neither build is the contracts repository's `main`,
//! whose later work (`Position.initMarginRatio`, `lpFeeGrowth*`,
//! `HealthNotImproved`, `previewPosition`) is on no chain.
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

// `sol!`-generated RPC methods mirror the Solidity arity; e.g.
// `PerpFactory::createPerp` legitimately takes 7 params, which trips clippy's
// `too_many_arguments` lint on generated code.
#![allow(clippy::too_many_arguments)]
#![doc = "\n\nThe design of this module: [`src/contracts/DESIGN.md`](https://github.com/StrobeLabs/perpcity-rust-sdk/blob/main/src/contracts/DESIGN.md)."]

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

    /// Base position state shared by makers and takers.
    /// `delta` is a packed `BalanceDelta` (`int128 amount0`, `int128 amount1`).
    struct Position {
        int256 delta;
        uint128 margin;
        uint24 liqMarginRatio;
        uint24 backstopMarginRatio;
        int256 lastCumlFundingX96;
    }

    /// Maker-specific state for an active liquidity range.
    struct Maker {
        int24 tickLower;
        int24 tickUpper;
        uint128 liquidity;
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

    /// Uniswap V4 PoolManager surface used by the SDK.
    ///
    /// `extsload` feeds the local quoter and the maker-equity reads from
    /// block-pinned storage. `ModifyLiquidity` is the pool's liquidity
    /// event: perp pools are vanilla V4 pools, so a maker open/adjust emits
    /// it from the PoolManager with `sender` = the Perp and `salt` = the
    /// position id.
    #[sol(rpc)]
    interface IPoolManagerState {
        /// Liquidity added to (`liquidityDelta > 0`) or removed from a pool's
        /// tick range.
        event ModifyLiquidity(
            bytes32 indexed id,
            address indexed sender,
            int24 tickLower,
            int24 tickUpper,
            int256 liquidityDelta,
            bytes32 salt
        );

        function extsload(bytes32 slot) external view returns (bytes32 value);
        function extsload(bytes32[] calldata slots) external view returns (bytes32[] memory values);
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
        event TakerClosed(uint256 posId, SwapResult sr, int256 funding, uint256 utilFees, uint256 liqFee, bool isLiquidation);
        event TakerLiquidated(uint256 indexed posId, uint128 perpAmount, uint256 liqFee);
        event TakerBackstopped(uint256 posId, uint128 marginIn, address posRecipient, int256 funding, uint256 utilFees);

        // Market-state events
        event CapacityUpdated(Capacity cap);
        event OpenInterestUpdated(OpenInterest oi);
        event CumulativesAccrued(Cumulatives cumls);
        event RatesAndEmasRefreshed(Rates rates, PricePair emas);
        event TicksCrossed(int24 startingTick, int24 endingTick, bool zeroForOne);
        event TickInitialized(int24 tick, int256 cumlFundingOppX96, int256 cumlFundingDivSqrtPOppX96);
        event TickDeleted(int24 tick);

        // Solvency / insurance events
        event Donated(address donor, uint128 amount, uint128 badDebt, uint80 insurance);
        event BadDebtAccounted(uint256 badDebt, uint256 insuranceAfter, uint128 badDebtAfter);
        event LossSocialized(uint256 originalAmount, uint256 feeCharged, uint128 badDebtAfter);
        event MarginTransferred(int128 marginDelta, uint128 totalMargin);

        // Governance: the six modules a market delegates to, each swapped
        // by one event. A fold that rebuilds the market from its events
        // needs them to know which rules were in force.
        event SetBeacon(address indexed beacon);
        event SetFeesModule(address indexed fees);
        event SetFundingModule(address indexed funding);
        event SetMarginRatiosModule(address indexed marginRatios);
        event SetPriceImpactModule(address indexed priceImpact);
        event SetPricingModule(address indexed pricing);

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

        /// Liquidate an unhealthy maker position, whole. Build `58b42b7`
        /// only: a `v0.2.2` market has no 2-arg selector and reverts empty;
        /// it takes [`PerpV022::liquidateMaker`] with the liquidity amount.
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

        /// Liquidate an unhealthy taker position, whole. Build `58b42b7`
        /// only; a `v0.2.2` market takes [`PerpV022::liquidateTaker`].
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

        function POOL_ID() external view returns (bytes32);

        function EMA_WINDOW() external view returns (uint256);

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

        /// The stored EMA pair as of `rates().lastTouch`. Build `58b42b7`
        /// only: `v0.2.2` dropped the view, so the SDK reads the pair from
        /// storage slot 11 on both builds and never calls this.
        function emas() external view returns (PricePair memory);

        // ── ERC721 ─────────────────────────────────────────────────

        /// Position NFT transfer: a mint has `from == address(0)`, a burn
        /// (full close, liquidation) `to == address(0)`.
        event Transfer(address indexed from, address indexed to, uint256 indexed tokenId);

        function name() external view returns (string memory);
        function symbol() external view returns (string memory);
        function tokenURI(uint256 tokenId) external view returns (string memory);
        function ownerOf(uint256 tokenId) external view returns (address);

        /// Move a position to another account. Positions carry their margin
        /// and their whole accrual history, so the new owner inherits the
        /// live position rather than a claim on it: the transfer settles
        /// nothing and touches no margin.
        ///
        /// `safeTransferFrom` over plain `transferFrom` because the SDK
        /// cannot know the destination is an EOA; the receiver check is the
        /// difference between a rejected transfer and a position stranded in
        /// a contract that has no code path to adjust it.
        function safeTransferFrom(address from, address to, uint256 tokenId) external;

        // ── Solady ERC721 errors ───────────────────────────────────
        //
        // Not from `libraries/Errors.sol` — the Perp inherits Solady's
        // ERC721, which reverts with its own set. Probed on the deployed
        // HORMUZ-TRAFFIC market (0x137E0048, Arbitrum One, 2026-09-08):
        // `safeTransferFrom` from a non-owner returns 0xa1148100 =
        // `TransferFromIncorrectOwner()`, while an undefined selector on the
        // same contract returns empty revert data. The selector exists.
        error TokenDoesNotExist();
        error NotOwnerNorApproved();
        error TransferToZeroAddress();
        error TransferFromIncorrectOwner();
        error TransferToNonERC721ReceiverImplementer();
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Build 58b42b7 maker close events
    // ═══════════════════════════════════════════════════════════════════

    /// Maker close/convert events as build `58b42b7` emits them.
    ///
    /// That build has no `MakerLiquidated` event; a maker liquidation emits
    /// `MakerConverted` (and the residual taker close emits `TakerClosed`)
    /// with a `liqFee` amount and `isLiquidation = true`. `v0.2.2` dropped
    /// both tail fields and moved liquidations to dedicated events, which
    /// changes the signature hash, so both shapes are declared for
    /// `decode_log`. (`Perp::TakerClosed` above is the `58b42b7` shape; the
    /// `v0.2.2` one is [`PerpV022::TakerClosed`].)
    interface PerpDeployedEvents {
        event MakerConverted(
            uint256 posId,
            int256 funding,
            uint256 longUtilFees,
            uint256 shortUtilFees,
            uint256 lpFees,
            uint256 liqFee,
            bool isLiquidation
        );
        event MakerClosed(
            uint256 posId,
            int256 funding,
            uint256 longUtilFees,
            uint256 shortUtilFees,
            uint256 lpFees,
            uint256 liqFee,
            bool isLiquidation
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    //  v0.2.2-upgradeable surface
    // ═══════════════════════════════════════════════════════════════════

    /// What a `v0.2.2-upgradeable` market (tag `198559a`) has that build
    /// `58b42b7` does not. Everything else on such a market is [`Perp`].
    ///
    /// The market is an ERC-1967 proxy behind the factory's
    /// `PERP_IMPLEMENTATION`; its pool carries the `PerpGuardHook`, which
    /// reverts `UnauthorizedPoolAction` on any PoolManager call whose sender
    /// is not the market itself. Liquidations take an amount and may be
    /// partial: a full one emits the untailed close event, then
    /// `TakerLiquidated` / `MakerLiquidated` with the fee.
    #[sol(rpc)]
    interface PerpV022 {
        /// The close event without the liquidation tails; the fee is in the
        /// `TakerLiquidated` log that follows a liquidation.
        event TakerClosed(uint256 posId, SwapResult sr, int256 funding, uint256 utilFees);
        /// `skim`: the owner swept the balance above the market's books.
        event SurplusRecovered(address indexed recipient, uint256 amount);

        /// Liquidate `liquidityAmount` of an unhealthy maker position; the
        /// whole position when it equals the position's liquidity. Zero
        /// reverts `ZeroLiquidity`, more than the position `MaxAmtExceeded`.
        function liquidateMaker(uint256 posId, address liquidationFeeRecipient, uint128 liquidityAmount) external;

        /// Liquidate `perpAmount` of an unhealthy taker position; the whole
        /// position when it equals the position's perp amount. Zero reverts
        /// `ZeroDelta`, more than the position `MaxAmtExceeded`.
        function liquidateTaker(uint256 posId, address liquidationFeeRecipient, uint128 perpAmount) external;

        /// The pool's hook, the `PerpGuardHook`.
        function HOOKS() external view returns (address);

        // `libraries/Errors.sol` additions.
        error NoSurplus();
        error ZeroAddress();
        // `PerpGuardHook`.
        error UnauthorizedPoolAction();
        // OpenZeppelin ERC-1967 / UUPS / Initializable, on the proxy.
        error ERC1967InvalidImplementation(address implementation);
        error ERC1967NonPayable();
        error UUPSUnauthorizedCallContext();
        error UUPSUnsupportedProxiableUUID(bytes32 slot);
        error InvalidInitialization();
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
        // `v0.2.2-upgradeable`: `setPerpImplementation`'s refusals.
        error InvalidPerpImplementation();
        error NotProtocolOwner();

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
    ///
    /// `index()` is a read: the deployed beacons answer it under
    /// `STATICCALL`, and it returns the value alone, with no update time.
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

/// ABI-lock tests: assert the generated bindings match the frozen contracts.
///
/// These guard against the class of bug that motivated the binding rewrite —
/// silently mis-shaped structs and events. Because a function selector is
/// computed from its *input* types only, return-struct drift (e.g. a view
/// losing a field) is NOT caught by a selector check; we lock struct field
/// shapes via EIP-712 type strings, function selectors via `SIGNATURE`, and
/// event `topic0` via the event `SIGNATURE`. Expected values are transcribed
/// from `perpcity-contracts/src/` (`Structs.sol` / `Events.sol`).
#[cfg(test)]
mod abi_lock {
    use super::*;
    use alloy::sol_types::{SolCall, SolEvent, SolStruct};

    /// Struct field shapes (names + types) — catches return-struct drift.
    #[test]
    fn struct_shapes_match_frozen_contracts() {
        // Flat structs (no nested struct fields): exact match.
        assert_eq!(
            Position::eip712_encode_type().as_ref(),
            "Position(int256 delta,uint128 margin,uint24 liqMarginRatio,uint24 backstopMarginRatio,int256 lastCumlFundingX96)"
        );
        assert_eq!(
            Taker::eip712_encode_type().as_ref(),
            "Taker(uint256 lastLongUtilPaymentsX96,uint256 lastShortUtilPaymentsX96)"
        );
        assert_eq!(
            Rates::eip712_encode_type().as_ref(),
            "Rates(int88 fundingPerDay,uint64 longUtilFeePerDay,uint64 shortUtilFeePerDay,uint40 lastTouch)"
        );
        assert_eq!(
            Cumulatives::eip712_encode_type().as_ref(),
            "Cumulatives(int256 fundingX96,int256 fundingDivSqrtPX96,uint256 longUtilPaymentsX96,uint256 shortUtilPaymentsX96,uint256 longUtilEarningsX96,uint256 shortUtilEarningsX96)"
        );
        assert_eq!(
            TickInfo::eip712_encode_type().as_ref(),
            "TickInfo(int256 cumlFundingOppX96,int256 cumlFundingDivSqrtPOppX96)"
        );
        assert_eq!(
            FeeFund::eip712_encode_type().as_ref(),
            "FeeFund(uint80 insurance,uint80 creatorFees,uint80 protocolFees)"
        );
        assert_eq!(
            SolvencyState::eip712_encode_type().as_ref(),
            "SolvencyState(uint128 badDebt,uint128 totalMargin)"
        );
        assert_eq!(
            OpenInterest::eip712_encode_type().as_ref(),
            "OpenInterest(uint128 long,uint128 short)"
        );
        assert_eq!(
            Capacity::eip712_encode_type().as_ref(),
            "Capacity(uint128 long,uint128 short)"
        );
        assert_eq!(
            PricePair::eip712_encode_type().as_ref(),
            "PricePair(uint128 ammPrice,uint128 index)"
        );
        assert_eq!(
            MakerFunding::eip712_encode_type().as_ref(),
            "MakerFunding(int256 belowX96,int256 withinX96,int256 divSqrtPriceWithinX96)"
        );
        assert_eq!(
            Modules::eip712_encode_type().as_ref(),
            "Modules(address beacon,address fees,address funding,address marginRatios,address priceImpact,address pricing)"
        );
        assert_eq!(
            SwapResult::eip712_encode_type().as_ref(),
            "SwapResult(int256 delta,uint256 ammPrice,int256 totalFeeAmt,uint256 lpFeeAmt,uint256 protocolFeeAmt,uint256 creatorFeeAmt,uint256 insuranceFeeAmt)"
        );

        // Param structs (the SDK builds these).
        assert_eq!(
            OpenTakerParams::eip712_encode_type().as_ref(),
            "OpenTakerParams(address holder,uint128 margin,int256 perpDelta,uint256 amt1Limit)"
        );
        assert_eq!(
            OpenMakerParams::eip712_encode_type().as_ref(),
            "OpenMakerParams(address holder,uint128 margin,int24 tickLower,int24 tickUpper,uint128 liquidity,uint256 maxAmt0In,uint256 maxAmt1In)"
        );
        assert_eq!(
            AdjustTakerParams::eip712_encode_type().as_ref(),
            "AdjustTakerParams(uint256 posId,int128 marginDelta,int256 perpDelta,uint256 amt1Limit)"
        );
        assert_eq!(
            AdjustMakerParams::eip712_encode_type().as_ref(),
            "AdjustMakerParams(uint256 posId,int128 marginDelta,int128 liquidityDelta,uint256 amt0Limit,uint256 amt1Limit)"
        );

        // Nested structs: EIP-712 appends referenced type definitions, so lock
        // the primary field list with a prefix check.
        assert!(Maker::eip712_encode_type().starts_with(
            "Maker(int24 tickLower,int24 tickUpper,uint128 liquidity,uint256 lastLongUtilEarningsX96,uint256 lastShortUtilEarningsX96,Capacity capacity,MakerFunding lastCumlFunding)"
        ));
    }

    /// Function selectors (input types) — catches arity/param drift.
    #[test]
    fn function_selectors_match_frozen_contracts() {
        assert_eq!(
            Perp::openTakerCall::SIGNATURE,
            "openTaker((address,uint128,int256,uint256))"
        );
        assert_eq!(
            Perp::adjustTakerCall::SIGNATURE,
            "adjustTaker((uint256,int128,int256,uint256))"
        );
        assert_eq!(
            Perp::openMakerCall::SIGNATURE,
            "openMaker((address,uint128,int24,int24,uint128,uint256,uint256))"
        );
        assert_eq!(
            Perp::adjustMakerCall::SIGNATURE,
            "adjustMaker((uint256,int128,int128,uint256,uint256))"
        );
        // Build 58b42b7: the 2-arg whole-position forms.
        assert_eq!(
            Perp::liquidateMakerCall::SIGNATURE,
            "liquidateMaker(uint256,address)"
        );
        assert_eq!(Perp::liquidateMakerCall::SELECTOR, [0xaa, 0xfa, 0xf6, 0x74]);
        assert_eq!(
            Perp::liquidateTakerCall::SIGNATURE,
            "liquidateTaker(uint256,address)"
        );
        assert_eq!(Perp::liquidateTakerCall::SELECTOR, [0xea, 0xc4, 0x19, 0x06]);
        // v0.2.2-upgradeable: the 3-arg forms, the only ones its
        // implementation (0x9b74b1ff217bdE6EB0e221Ca11034D67187EF9d3,
        // Arbitrum One) dispatches.
        assert_eq!(
            PerpV022::liquidateMakerCall::SIGNATURE,
            "liquidateMaker(uint256,address,uint128)"
        );
        assert_eq!(
            PerpV022::liquidateMakerCall::SELECTOR,
            [0x14, 0xca, 0x0f, 0x4c]
        );
        assert_eq!(
            PerpV022::liquidateTakerCall::SIGNATURE,
            "liquidateTaker(uint256,address,uint128)"
        );
        assert_eq!(
            PerpV022::liquidateTakerCall::SELECTOR,
            [0xbf, 0xb4, 0xb1, 0xc7]
        );
        assert_eq!(PerpV022::HOOKSCall::SELECTOR, [0xdc, 0xe1, 0x56, 0x1d]);
        assert_eq!(
            Perp::backstopMakerCall::SIGNATURE,
            "backstopMaker(uint256,uint128,address)"
        );
        assert_eq!(
            Perp::backstopTakerCall::SIGNATURE,
            "backstopTaker(uint256,uint128,address)"
        );
        assert_eq!(
            Perp::safeTransferFromCall::SIGNATURE,
            "safeTransferFrom(address,address,uint256)"
        );
        assert_eq!(Perp::positionsCall::SIGNATURE, "positions(uint256)");
        assert_eq!(Perp::makerDetailsCall::SIGNATURE, "makerDetails(uint256)");
        assert_eq!(Perp::nextPosIdCall::SIGNATURE, "nextPosId()");
        assert_eq!(Perp::solvencyStateCall::SIGNATURE, "solvencyState()");
        assert_eq!(Perp::poolStateCall::SIGNATURE, "poolState()");
        assert_eq!(Perp::modulesCall::SIGNATURE, "modules()");
        assert_eq!(Perp::ratesCall::SIGNATURE, "rates()");
        assert_eq!(Perp::cumulativesCall::SIGNATURE, "cumulatives()");
        // Verified 2026-09-07 by eth_call on HORMUZ-TRAFFIC
        // (0x137e00487dc079dad69ba149994320a8ff4c5b17, Arbitrum One):
        // selector 0x6ab80a34 returned (ammPrice 0x2e4e1d5d09c24b2a50779abaf3,
        // index 0x2dc0f47dc4c7764d34d2f6a88f).
        assert_eq!(Perp::emasCall::SIGNATURE, "emas()");
        assert_eq!(Perp::emasCall::SELECTOR, [0x6a, 0xb8, 0x0a, 0x34]);
        // Both live beacon bytecodes (Arbitrum One 0x1b37de2b…ef884 and
        // 0x0a33ea45…29990) answer 0x2986c0e5 under STATICCALL with one
        // 32-byte word, so the read is `view` and returns no update time.
        assert_eq!(IBeacon::indexCall::SIGNATURE, "index()");
        assert_eq!(IBeacon::indexCall::SELECTOR, [0x29, 0x86, 0xc0, 0xe5]);

        // `get_capacity` reads these two. eth_call on HORMUZ-TRAFFIC at
        // block 507526321: capacity() returned (60883605, 51603209) and
        // openInterest() (37772806, 42582564).
        assert_eq!(Perp::capacityCall::SIGNATURE, "capacity()");
        assert_eq!(Perp::capacityCall::SELECTOR, [0x5c, 0xfc, 0x1a, 0x51]);
        assert_eq!(Perp::openInterestCall::SIGNATURE, "openInterest()");
        assert_eq!(Perp::openInterestCall::SELECTOR, [0xfa, 0x5a, 0x2e, 0x62]);
    }

    /// The v0.2.2 error selectors, so a typed revert from a proxy market
    /// decodes by name.
    #[test]
    fn v022_error_selectors_match_the_tag() {
        use alloy::sol_types::SolError;

        assert_eq!(PerpV022::NoSurplus::SELECTOR, [0xc0, 0xef, 0x17, 0xd3]);
        assert_eq!(PerpV022::ZeroAddress::SELECTOR, [0xd9, 0x2e, 0x23, 0x3d]);
        assert_eq!(
            PerpV022::UnauthorizedPoolAction::SELECTOR,
            [0xb7, 0xcc, 0x50, 0x70]
        );
        assert_eq!(
            PerpV022::ERC1967InvalidImplementation::SELECTOR,
            [0x4c, 0x9c, 0x8c, 0xe3]
        );
        assert_eq!(
            PerpV022::ERC1967NonPayable::SELECTOR,
            [0xb3, 0x98, 0x97, 0x9f]
        );
        assert_eq!(
            PerpV022::UUPSUnauthorizedCallContext::SELECTOR,
            [0xe0, 0x7c, 0x8d, 0xba]
        );
        assert_eq!(
            PerpV022::UUPSUnsupportedProxiableUUID::SELECTOR,
            [0xaa, 0x1d, 0x49, 0xa4]
        );
        assert_eq!(
            PerpV022::InvalidInitialization::SELECTOR,
            [0xf9, 0x2e, 0xe8, 0xa9]
        );
        assert_eq!(Perp::TokenDoesNotExist::SELECTOR, [0xce, 0xea, 0x21, 0xb6]);
        assert_eq!(
            PerpFactory::InvalidPerpImplementation::SELECTOR,
            [0xa4, 0x57, 0x69, 0x5f]
        );
        assert_eq!(
            PerpFactory::NotProtocolOwner::SELECTOR,
            [0xfb, 0x6f, 0xc0, 0xb7]
        );
    }

    /// Event signatures (all params) — drives `topic0`; catches event drift.
    #[test]
    fn event_signatures_match_frozen_contracts() {
        const SWAP_RESULT: &str = "(int256,uint256,int256,uint256,uint256,uint256,uint256)";

        assert_eq!(Perp::MakerOpened::SIGNATURE, "MakerOpened(uint256)");
        assert_eq!(
            Perp::MakerAdjusted::SIGNATURE,
            "MakerAdjusted(uint256,int256,uint256,uint256,uint256)"
        );
        assert_eq!(
            Perp::MakerConverted::SIGNATURE,
            "MakerConverted(uint256,int256,uint256,uint256,uint256)"
        );
        assert_eq!(
            Perp::MakerClosed::SIGNATURE,
            "MakerClosed(uint256,int256,uint256,uint256,uint256)"
        );
        assert_eq!(
            Perp::MakerLiquidated::SIGNATURE,
            "MakerLiquidated(uint256,uint128,uint256)"
        );
        assert_eq!(
            Perp::MakerBackstopped::SIGNATURE,
            "MakerBackstopped(uint256,uint128,address,int256,uint256,uint256,uint256)"
        );
        assert_eq!(
            Perp::TakerOpened::SIGNATURE,
            format!("TakerOpened(uint256,{SWAP_RESULT})")
        );
        assert_eq!(
            Perp::TakerAdjusted::SIGNATURE,
            format!("TakerAdjusted(uint256,{SWAP_RESULT},int256,uint256)")
        );
        // Verified against the deployed contract: topic0 of this signature is
        // 0xc6d1565765c65beb63cd0a76e37c058e0908e6e24a609d0dc1a724106ae0576e,
        // matching the `TakerClosed` log emitted on Arbitrum Sepolia. The
        // deployed event unifies close + liquidation (`liqFee`, `isLiquidation`).
        assert_eq!(
            Perp::TakerClosed::SIGNATURE,
            format!("TakerClosed(uint256,{SWAP_RESULT},int256,uint256,uint256,bool)")
        );
        assert_eq!(
            Perp::TakerClosed::SIGNATURE_HASH,
            alloy::primitives::b256!(
                "c6d1565765c65beb63cd0a76e37c058e0908e6e24a609d0dc1a724106ae0576e"
            )
        );
        assert_eq!(
            Perp::TakerLiquidated::SIGNATURE,
            "TakerLiquidated(uint256,uint128,uint256)"
        );
        assert_eq!(
            Perp::TakerLiquidated::SIGNATURE_HASH,
            alloy::primitives::b256!(
                "2347417853c438a233b6d4d0630048d196587db0d521d33f5a4705721e1a91cf"
            )
        );
        assert_eq!(
            Perp::MakerLiquidated::SIGNATURE_HASH,
            alloy::primitives::b256!(
                "1ea1626f80876d431626ac0d48ac2bb9495fa178b8b7fab5fbb75fa9d430b554"
            )
        );
        // v0.2.2-upgradeable: the untailed taker close.
        assert_eq!(
            PerpV022::TakerClosed::SIGNATURE,
            format!("TakerClosed(uint256,{SWAP_RESULT},int256,uint256)")
        );
        assert_eq!(
            PerpV022::TakerClosed::SIGNATURE_HASH,
            alloy::primitives::b256!(
                "208f950e4dba30512aa9e643b25c9df8bdb616ee90bbff00f669a5d1d3d452f3"
            )
        );
        assert_eq!(
            PerpV022::SurplusRecovered::SIGNATURE,
            "SurplusRecovered(address,uint256)"
        );
        // Deployed-era maker closes (pre-#171): topic0 values transcribed
        // from logs emitted by the live Arbitrum perps.
        assert_eq!(
            PerpDeployedEvents::MakerConverted::SIGNATURE,
            "MakerConverted(uint256,int256,uint256,uint256,uint256,uint256,bool)"
        );
        assert_eq!(
            PerpDeployedEvents::MakerConverted::SIGNATURE_HASH,
            alloy::primitives::b256!(
                "8d8df09df1280157a012f3f883267724105b6d76650a4f9ff07413e4741711e8"
            )
        );
        assert_eq!(
            PerpDeployedEvents::MakerClosed::SIGNATURE,
            "MakerClosed(uint256,int256,uint256,uint256,uint256,uint256,bool)"
        );
        assert_eq!(
            PerpDeployedEvents::MakerClosed::SIGNATURE_HASH,
            alloy::primitives::b256!(
                "752da4d171cb6563c169a325eca86c5e7f5b62e9da232b1f737cafebc6830b7e"
            )
        );
        assert_eq!(
            Perp::TakerBackstopped::SIGNATURE,
            "TakerBackstopped(uint256,uint128,address,int256,uint256)"
        );
        assert_eq!(
            Perp::OpenInterestUpdated::SIGNATURE,
            "OpenInterestUpdated((uint128,uint128))"
        );
        assert_eq!(
            Perp::RatesAndEmasRefreshed::SIGNATURE,
            "RatesAndEmasRefreshed((int88,uint64,uint64,uint40),(uint128,uint128))"
        );
        assert_eq!(
            Perp::TicksCrossed::SIGNATURE,
            "TicksCrossed(int24,int24,bool)"
        );
        assert_eq!(IBeacon::IndexUpdated::SIGNATURE, "IndexUpdated(uint256)");
        // The topic0 of every `IndexUpdated` log from the live Arbitrum One
        // beacons (e.g. 0x0a33ea45fe9011029641ef63ce8e1c94a8a29990).
        assert_eq!(
            IBeacon::IndexUpdated::SIGNATURE_HASH,
            alloy::primitives::b256!(
                "acfc085c9be45d2b3f9e5c09a19d4a95749cc16939519c13e090de3a4cb192c6"
            )
        );
        // The topic0 the live PoolManager (0x360e68faccca8ca495c1b759fd9eee466db9fb32,
        // Arbitrum One) emits for every perp pool's liquidity change.
        assert_eq!(
            IPoolManagerState::ModifyLiquidity::SIGNATURE,
            "ModifyLiquidity(bytes32,address,int24,int24,int256,bytes32)"
        );
        assert_eq!(
            IPoolManagerState::ModifyLiquidity::SIGNATURE_HASH,
            alloy::primitives::b256!(
                "f208f4912782fd25c7f114ca3723a2d5dd6f3bcc3ac8db5af63baa85f711d5ec"
            )
        );
        // ERC721 Transfer shares its topic0 with ERC20 Transfer (the
        // signature omits `indexed`); only the topic count tells them apart.
        assert_eq!(
            Perp::Transfer::SIGNATURE,
            "Transfer(address,address,uint256)"
        );
        assert_eq!(
            Perp::Transfer::SIGNATURE_HASH,
            IERC20::Transfer::SIGNATURE_HASH
        );
        assert_eq!(
            Perp::Transfer::SIGNATURE_HASH,
            alloy::primitives::b256!(
                "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
            )
        );
        // The six module setters, `cast keccak` of each signature; the
        // same on both live builds.
        for (signature, hash) in [
            (
                Perp::SetBeacon::SIGNATURE,
                "eda478a82221a6140112aa7f09cae9d34ba9ef5afe98665b4b91af27bfae4b80",
            ),
            (
                Perp::SetFeesModule::SIGNATURE,
                "52a6977e4b34f6211e8979d27735f11d62127fde19dac323e94a7b1b3dcc927d",
            ),
            (
                Perp::SetFundingModule::SIGNATURE,
                "3d9e293475ebd5f1c539a7b7b922bdd95152550f0b65820ee5b1b2dd1546d734",
            ),
            (
                Perp::SetMarginRatiosModule::SIGNATURE,
                "9e2c08905af50bef2fe2ea75467e6a980ed96b0d42e3166ea2ddd7da61d0ff99",
            ),
            (
                Perp::SetPriceImpactModule::SIGNATURE,
                "75f628c85f0f4623da54cc34d09abc7c38b277a46a64acc1b8624aacce3bd4c8",
            ),
            (
                Perp::SetPricingModule::SIGNATURE,
                "a3c68ccb672060124d2ccfc83677f8c033e5f93ec4eff04e73206a48129d9c28",
            ),
        ] {
            assert_eq!(
                alloy::primitives::keccak256(signature.as_bytes()),
                hash.parse::<alloy::primitives::B256>().unwrap(),
                "{signature}"
            );
        }
    }
}
