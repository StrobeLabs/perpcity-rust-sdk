# Terminology

The words this crate uses, one meaning each. A name in the SDK is a claim
about how a Perp City market works, and the test of the crate is that
someone can read it and come away knowing how the markets function. That
only holds if every concept has one canonical word, the word is used for
nothing else, and the type that carries the concept is the one named
here.

Legion builds on this vocabulary. A term defined here is not redefined
there; a term Legion needs that is absent here is either added here, if
it is a fact about the chain, or is Legion's own, if it is a fact about
strategy.

How to read an entry: the canonical word, what it means, and the type or
read that carries it. Words we no longer use are listed at the end with
their replacements.

## The market

**Market.** One `Perp` contract: one perpetual future, its own pool,
positions, funding, fees and solvency. There is no market id; the
contract's address identifies it, and every event names the market by the
emitting address. A `MarketReader` reads one market.

**Perp.** The market's contract. Used for the contract itself and its
bindings; the market as a concept is "the market".

**Pool.** The Uniswap V4 pool the market trades on, owned by the
PoolManager and keyed by its `PoolKey`. Token 0 is the perp, token 1 is the
collateral. The pool has no fee of its own; the market's fees are
charged by the Perp on the swap's USD volume. `PoolSnapshot` is the pool
at one block.

**PoolManager.** Uniswap V4's singleton, where the pool's state, tick
bitmap and fee growth live. The SDK reads it by `extsload` at known slots.

**Collateral.** USDC, the token every margin, fee and settlement is
denominated in. `ChainDeployments::usdc`.

**Modules.** The five governance-set contracts a market delegates to, from
`modules()`: the beacon, fees, funding, margin ratios, price impact, and
pricing. An unregistered module is the zero address and reads as
`ContractError::ModuleNotRegistered`.

**Beacon.** The market's oracle. Its `index()` is the index; it emits
`IndexUpdated` when the index changes, and each such log is a *print*.

**Factory.** `PerpFactory`, which deploys markets and emits `PerpCreated`.
Deployment is not listing: which markets are live is off-chain curation,
not a chain fact.

**Position.** One NFT-numbered position on a market, maker or taker, keyed
by its position id (the ERC-721 token id). The chain reports a closed or
never-minted id as an empty struct; `StateAt::position` returns `None` for
those and `MarketReader::get_position` errs.

**Owner.** The wallet holding the position NFT. Trade events name a
position id and never a wallet; ownership is a fold of `Transfer` events,
`OwnershipLog`, and is a function of time.

## Prices

There are three prices and one relation between them. Naming any one of
them by another's name is the mistake this section exists to prevent.

**Pool price.** The AMM's spot: `poolState().ammPrice`, the price of the
perp in collateral at the pool's current tick. The contract calls it
`ammPrice`; in prose and in the SDK's f64 surface it is the pool price
(`PerpSnapshot::pool_price`, `MarketReader::get_pool_price`). It is the
price a trade moves and the price liquidity geometry is measured in. It
is not the mark.

**Index.** The beacon's price: `IBeacon::index()`, a print at a time. Raw
and unsmoothed; the contract smooths it into an EMA but the index is the
print.

**EMAs.** The contract's exponential moving averages of the pool price and
the index, a `PricePair`, stored at each touch and advanced by
`calculate_emas` over the time since `last_touch` with the market's
`EMA_WINDOW`. Between touches the stored pair is stale by construction;
the pair that matters is the advanced one.

**Mark.** The price the contract values positions at: the fair price of the
pool price, the index, and the two advanced EMAs, as the pricing module's
`fairPrice` computes it and `PerpLogic.accrue` stores it. Every health
check, `valPnl`, liquidation test and utilization accrual prices at the
mark. `Mark` is the four inputs at one block, `Mark::fair_price_x96` the
price; `StateAt::mark` reads it at a block, `MarketReader::get_mark` at
the lagged snapshot block.

**Fair price.** The function, not a price of its own: `fair_price_x96`
and its f64 twin `fair_price`. "The fair price of these inputs" is the
mark.

**Sqrt price.** A price as the pool stores it, `sqrt(price) × 2^96`, a
`U256` suffixed `_x96`. Ticks and sqrt prices convert both ways through
`math::tick`; `convert` moves between sqrt prices and f64 prices.

**Tick.** The pool's price grid: price `1.0001^tick`. Maker ranges are
aligned to the market's tick spacing, 30 on every Perp City pool, and
bounded by `MIN_TICK` and `MAX_TICK`.

**Basis.** The mark relative to the index. Legion's word, defined here so
its inputs are named: a basis is always of the mark, never of the pool
price, unless it says "pool basis".

## Positions and their two kinds

**Taker.** A directional position: a `Side`, long or short, opened by
swapping against the pool. Its exposure is its perp delta, signed, and
its cost basis its USD delta. `OpenTakerParams` opens one in human units,
`ExactOpenTakerParams` in wire units.

**Maker.** A liquidity position: collateral standing as concentrated
liquidity in a range of the pool. A maker earns the pool's LP fees,
utilization fees and funding on the side its capacity backs, and holds
inventory as the price moves through its range.

**Band.** A maker's geometry: a `TickRange` and the liquidity standing in
it, `MakerBand`. The one type for a band wherever it appears: read back
from chain (`StateAt::maker_band`), sized before it opens, or tracked after
it did. The maker math takes it whole.

**Range.** A tick interval `[lower, upper)`, valid by construction:
`TickRange`. Lower below upper, both inside the V4 domain, checked once
where the ticks enter and never again. A range's `contains` is half-open,
the way V4 activates liquidity.

**Liquidity.** The pool's own unit, V4's `L`, the quantity a band's
capacity and token amounts derive from. Not a USD amount and not a perp
amount; `estimate_liquidity` sizes it from collateral and
`band_amounts` says what tokens it holds at a price.

**Capacity.** The open interest a band can back: the perp amount its
liquidity spans above the pool price backs longs, below it backs shorts,
in perp atoms, as `PerpLogic.calcCapacity` computes it at the moment the
liquidity changes and never after. `Capacity` is one band's or the
market's running sum; `band_capacity` computes it, `liquidity_for_capacity`
inverts it.

**Open interest.** The perp amount takers hold on each side,
`OpenInterest`, in perp atoms on chain and perp tokens in the f64
surface.

**Headroom.** Capacity less open interest on a side: what can still be
opened before `LongUtilizationExceeded` or its short twin.
`MarketCapacity::headroom_atoms`.

**Utilization.** Open interest over capacity on a side, the number the
fees module takes to set the utilization fee. `MarketCapacity::utilization_e6`.

**Settle.** What the contract does to a maker when it is touched: credit
accrued funding, utilization earnings and LP fees to its margin, and
mark its inventory. `MakerEquityBreakdown` is a settle previewed off
chain; a `MakerSettle` in the event vocabulary is one the chain performed.

**Convert.** A maker whose liquidity reaches zero with inventory left
becomes a taker holding that inventory under the same position id:
`MakerConverted`.

**Liquidate.** Close a position below its liquidation margin ratio, paying
the liquidator a fee. On the deployed contracts a taker liquidation is a
`TakerClosed` with `is_liquidation` set and a maker's is a
`MakerConverted` likewise; `MakerLiquidated` and `TakerLiquidated` are
later contracts' events.

**Backstop.** A third party taking over a position below its backstop
ratio by adding margin and receiving the position, before it would need
liquidating: `TakerBackstopped` and `MakerBackstopped`.

## Economics

**Margin.** Collateral deposited against a position, in USDC. Not equity:
equity is margin plus everything accrued and the inventory marked.

**Margin ratio.** Equity over position value, as a fraction. The market's
`IMarginRatios` module sets three thresholds per kind, `MarginRatioTriple`:
`init` to open or grow, `liquidation` below which a position is
liquidatable, `backstop` below which it can be backstopped. Makers on the
deployed markets are fully collateralised, a ratio of 1.0.

**Leverage.** The inverse of a margin ratio, the taker-facing unit.
`Bounds` carries the market's minimum and maximum.

**Funding.** The payment between longs and shorts, `fundingPerDay`
positive when longs pay shorts, WAD-scaled per day on chain. Makers
receive funding on the side their capacity backs, tracked per tick.

**Utilization fee.** Per-side fee takers pay makers for the capacity they
use, `longUtilFeePerDay` and its short twin, set by the fees module from
utilization.

**LP fee.** The swap fee the Perp charges on a taker's USD volume and
donates to the pool's makers in range; the pool itself charges nothing.
One of four fee legs on every swap, with the creator, insurance and
protocol fees: `SwapInfo` carries all four exactly.

**Touch.** Any action on a market that runs `accrue`: rates, EMAs and
cumulatives advance to the block and `last_touch` moves.
`RatesAndEmasRefreshed` is the event of a touch.

**Cumulatives.** The market's running accumulators for funding and
utilization, `Cumulatives`, against which a position's last checkpoint is
differenced at settle.

**Solvency.** The contract's own books, `SolvencyState`: `total_margin`,
which moves only when real USDC enters or leaves, and `bad_debt`, the
insolvency it has recognised. What positions claim can exceed both.

**Fee fund.** The insurance balance and the creator's and protocol's
uncollected fees, `FeeFund`.

## Units

The chain's units are integers; the SDK's human surface is `f64`. A name
says which.

**Atoms.** A chain integer in its smallest unit. USDC atoms are
6-decimal (`SCALE_1E6`); perp atoms are 6-decimal too. A field or
parameter suffixed `_atoms` is one, and `Exact*` parameter types carry
them for callers that must not round-trip through `f64`.

**USDC, perp tokens.** The f64 human units, unsuffixed:
`margin: f64` is USDC, `long_oi: f64` is perp tokens.

**X96, X128.** Fixed point over `2^96` or `2^128`, `U256` or `I256`,
suffixed `_x96` or `_x128`. Prices and sqrt prices are X96; the pool's
fee growth is X128.

**WAD.** Fixed point over `10^18`, the funding rate's unit on chain.

**e6.** A ratio scaled by `10^6`, the margin-ratio and fee unit on chain;
`MarginRatioTriple::from_e6` takes it and the fraction is unsuffixed.

**Perp delta, USD delta.** A swap's two legs from V4's packed
`BalanceDelta`: perp signed by direction, USD signed by flow, both in
atoms on chain and unpacked by `unpack_balance_delta`.

## Reads and their tense

A read's block policy is a property of the type it hangs off. There are
two tenses and no third.

**Now.** Reads on `MarketReader` and `ChainReader`: the head at the moment
of the call, or the state cache within its TTL. Each read is independently
current and promises nothing about agreeing with any other read on the
block it came from. Their names start with `get_`.

**At a block.** Reads on `StateAt`: the handle resolves one header and
pins every read to that hash, so values read through one handle cannot
come from different blocks. The handle is the block. `MarketReader::state`
pins the lagged snapshot block, `state_at(number)` a block the caller
names. Reads on the handle have no prefix: `capacity`, `pool`, `mark`,
`solvency`.

**Snapshot.** A read whose result carries the block it was read at:
`PerpSnapshot` by number, `PoolSnapshot`, `MarketCapacity` and
`MakerMarketSnapshot` by `BlockContext`. A snapshot's fields all come from
that one block.

**Lagged snapshot block.** The head less `SNAPSHOT_BLOCK_LAG`, the block a
`state()` handle pins. Far enough back that every replica behind a
load-balanced endpoint has it, near enough to be under two seconds old.

**Pinned.** Read at a block hash, so a reorg cannot substitute another
block's state. Every read on a handle is pinned.

**Head.** The newest block the serving node has. A now-read at the head
carries its number and nothing else, since the header is not resolved.

**Block context.** A block's number, hash and timestamp, `BlockContext`:
what a snapshot carries and a pinned read derives its hash from.

**Cache layers.** The state cache's two TTLs: slow for governance-set
values (fees, bounds), fast for values that move every block (pool
price, funding, balances). A now-read may be served from either.

**Immutables.** A market's deployment-fixed values, `MarketImmutables`,
read once per market and never pinned, since they cannot change.

## Time and history

**Feed.** The present tense: `MarketFeed` streams the market's events over
a WebSocket as they are emitted.

**Tape.** The past tense: a market's whole event history replayed from log
scans, `TapeEvent`s in chain order. The feed and the tape speak one
vocabulary, `MarketEvent`, decoded once.

**Event vocabulary.** `MarketEvent`, every event a Perp or beacon emits,
in human units. The one place ABI shapes are known.

**Chain point.** Where an event sits in chain order, `ChainPoint`: block
number and log index. The key a tape row and a print are joined on.

**Print.** One `IndexUpdated` from the beacon, `IndexPrint`: the index at
a chain point and time. The beacon's series is its prints.

**Ownership log.** Who held which position when, `OwnershipLog`, folded
from the tape's `PositionTransferred` events. Custody at a chain point,
not the latest owner.

**Scan.** A chunked `eth_getLogs` over a range, `History`, which learns the
provider's limits as it goes. A scan is throughput work and is never on
the trading path.

**Era.** A contract version whose logs are on chain forever. Calls target
only the deployed contracts; events stay decodable for every era that
emitted them. `PerpDeployedEvents` is the deployed era's event shape where
it differs from the contracts repository's main branch, which the `Perp`
interface follows.

## Chain plumbing

**Transport.** `HftTransport`: several RPC endpoints behind one provider,
routed by health and latency, with reads and writes classified
differently. An endpoint is one URL; the pool of them is the transport.

**Transient.** A failure a retry can fix: the endpoint was slow, the
replica was behind, the receipt was late. `PerpCityError::is_transient`
says so and retry loops key on it. A failure that names a permanent
condition, pruned state, a rejected log range, an unregistered module, is
not transient and a retry is wrong.

**Block unavailable.** The replica does not have the block a read is pinned
to, `ContractError::BlockUnavailable`. Transient: it will.

**State unavailable.** The node has the header but pruned the state behind
it, `ContractError::StateUnavailable`. Not transient: an archive endpoint
is the fix.

**Multicall.** Several views in one request through Multicall3, at one
block. `aggregate` fails as a whole; `blockAndAggregate` also reports the
block it ran in.

**Pipeline.** The send path: nonce, gas and signing with no RPC on the hot
path, `TxPipeline`. Urgency scales the gas price; the nonce manager owns
the account's next nonce.

## Retired words

| Do not write | Write | Why |
|---|---|---|
| book, taker book, liquidity book | pool, pool snapshot, tick map | order-book vocabulary on an AMM |
| mark price, for the pool price | pool price | the mark is a different price |
| fair price, as a thing read | mark | the fair price is the function; the mark is its value |
| `get_fair_price`, `FairPrice` | `get_mark`, `Mark` | the same |
| `MakerRange` | `MakerBand`, `TickRange` | a range is an interval; a band is a range with liquidity |
| `load_taker_market_snapshot`, `TakerMarketSnapshot` | `get_pool_snapshot`, `PoolSnapshot` | it is the pool, for any reader |
| `tick_lower`, `tick_upper` as loose fields | `TickRange` | the pair carries an invariant |
| perp id | the market's address | a Perp is one market; there is no id |
| block-pinned as a prefix on a read's name | a read on `StateAt` | the tense is the receiver's, not the method's |

## Naming rules

- Types are nouns for the thing, not for the read that produced it:
  `PoolSnapshot`, `Mark`, `MakerBand`, never `PoolSnapshotResult`.
- A now-read is `get_<noun>`; a read on a handle is `<noun>`. A read that
  builds something from several reads is still named for what it returns.
- A wire-unit field or parameter carries its unit as a suffix: `_atoms`,
  `_x96`, `_x128`, `_e6`, `_wad`. An unsuffixed `f64` is the human unit.
- A struct that carries its block is a snapshot and has a `block` field.
- The contract's own name is used for the contract's own thing
  (`ammPrice`, `lastTouch`, `fundingPerDay`) inside bindings and ported
  math, and this document's word everywhere the SDK speaks for itself.
- A new concept gets an entry here in the same change that introduces
  its type. A name that needs a sentence of explanation at every use is
  the wrong name.
