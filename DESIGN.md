# The Perp City SDK

The root of the design graph. Every component node points back here for
the aerial view, and this node points down at each of them. Read this
first; then read the node of the component you are changing; then follow
its edges before you touch a type.

## Purpose

This crate is the truth about the chain for a Perp City market: what the
contracts are, what they hold at a block, what they emitted, what they
compute, and how to send them a transaction that lands. It is the layer
below every strategy, ours and anyone else's, so it is also a product: an
outside market maker should be able to build on it without reading our
bots.

It is deliberately not a strategy. Nothing here decides when to trade,
how much, or what to do when a market misbehaves. When a piece of code
needs a `sol!` block, a storage slot, a port of contract math, or a send
pipeline, it belongs here; when it needs a policy, it belongs above.

The test of the crate is pedagogical. Someone who reads it should come
away knowing how these markets work: what a market is, what prices it
has and which one it values positions at, what a maker's band is and
what it can back, what a read at a block means, and what it costs to get
a transaction wrong. If the types do not teach that, they are the wrong
types.

## What matters

**Money is exact.** Every quantity the contract settles is an integer in a
wire unit, and the SDK carries it as one until the last moment. The f64
surface exists for humans and dashboards, never for arithmetic the chain
will check. Ported contract math is transcribed one-to-one in `U256` and
`I256` with the contract's rounding, and is anchored to golden vectors
from real chain state, not to synthetic fixtures. A port that reproduces a
live settle to the atom is correct; one that is close is wrong.

**Every order matters.** A transaction that lands is real money moved. The
send path is designed so that nothing about an order is ambiguous after
the fact: which nonce it used, which hash it got, whether the failure came
before or after the broadcast, and what the caller must do about it. A
failure that could mean "sent" is reported as a hash to look up, never as
"failed".

**The chain is the deployed one.** Bindings match deployed bytecode, not
the contracts repository's head. When they disagree the chain wins and the
fix goes in the binding, locked by an ABI test with the on-chain evidence
beside it. Events are the one exception, since logs from an earlier era
are on chain forever and must stay decodable.

**A read has a block.** A value from the chain is a fact about one block,
and two values that must agree must come from the same one. The crate
makes this a property of the type a read hangs off, not a discipline the
caller has to remember.

**Failure is classified, not described.** A caller's retry loop keys on
whether a failure is transient. Getting that wrong in either direction is
a bug with a cost: retrying a pruned-state read forever, or giving up on a
replica that is one block behind. So every failure the crate can name is a
typed variant with a stated transience.

**Latency is a design input, not an optimisation.** The hot path of a
trading loop makes zero RPCs to prepare a transaction, and every read
that can be one request is one request.

## The mental model

**A market is one contract.** A `Perp` is a market: its own Uniswap V4
pool (token 0 the perp, token 1 the collateral, USDC), its own positions
numbered as NFTs, its own funding, fees and solvency, and five modules
governance can swap: the beacon it takes its index from, and the fees,
funding, margin-ratio, price-impact and pricing rules it delegates to.
There is no market id. The contract's address is the market, and every
event names its market by the address that emitted it.

**Three prices, one relation.** The pool price is the AMM's spot, the
price a trade moves. The index is the beacon's print. The contract
smooths both into EMAs at every touch, and values every position at the
mark: the pricing module's fair price of the pool price, the index and
the two EMAs advanced to now. Health, liquidation, `valPnl` and
utilization accrual all price at the mark. Liquidity geometry, capacity,
and the effect of a trade all live in pool-price space. Confusing the two
is the single most consequential naming error available in this system,
and the crate's names are chosen so that it cannot be made silently.

**Two kinds of position.** A taker holds a signed exposure opened by
swapping against the pool. A maker holds collateral as concentrated
liquidity in a band, a tick range with liquidity standing in it, and
earns the pool's LP fees, utilization fees and funding on the side its
capacity backs. Capacity is what a band can back: the perp its liquidity
spans above the pool price backs longs, below it backs shorts, fixed at
the moment the liquidity was placed. Open interest draws on capacity;
headroom is what is left; utilization is the ratio the fees module prices
from.

**Two tenses of reads.** A read is either *now*, the head at the moment of
the call or the cache within its TTL, each read on its own; or *at a
block*, through a handle that resolved one header and pins every read to
its hash, so that values read through one handle agree by construction.
The receiver type says which tense a read is in. There is no third tense.

**Two tenses of events, one vocabulary.** The feed streams the present
over a WebSocket; the tape replays the past from log scans. Both produce
the same decoded `MarketEvent`, so anything that folds events, ownership,
economics, a series, works on either tense unchanged.

**Two unit systems, one boundary.** The chain speaks in atoms, X96, X128,
WAD and e6. Humans speak in USDC, perp tokens, prices and fractions. A
name carries its unit as a suffix when it is a wire unit and none when it
is human. Conversion happens at the crate's surface, once, in `convert`,
and never in the middle of math.

**Sending is a pipeline with an execution model.** Prepare with no RPC
(nonce from the manager, gas from the cache, sign locally), broadcast,
then poll for the receipt. Each stage has its own failure variants, and
the nonce manager's job is to make the account's next nonce a fact the
pipeline owns rather than a race with the node.

**Eras.** The deployed contracts are one era; the contracts repository's
main branch is the next. Calls target only what is deployed. Events from
every era that emitted them stay decodable. A cutover replaces the era
wholesale and deletes the compensation the earlier era needed.

## The type system

How the model above becomes types. Each row names the concept, the type
that carries it, the invariant it holds, and the node that owns it.

| Concept | Type | Invariant | Home |
|---|---|---|---|
| A chain | [`ChainReader`] | one transport, one deployment set, shared caches | [`client`] |
| A market, now | [`MarketReader`] | one `Perp` over a `ChainReader`; every read is current | [`client`] |
| A market, at a block | [`StateAt`] | the handle is the block; every read pinned to its hash | [`client`] |
| A market with a signer | [`PerpClient`] | a `MarketReader` plus the send pipeline | [`client`] |
| A send | [`TxBuilder`] | one transaction, one nonce, one outcome | [`client`] |
| A tick interval | [`TickRange`] | `lower < upper`, both in the V4 domain, checked at construction | [`math::range`] |
| A maker's geometry | [`MakerBand`] | a `TickRange` with liquidity | [`math::range`] |
| The mark's inputs | [`Mark`] | pool price, index and EMAs from one block, advanced to it | [`math::pricing`] |
| A price pair | [`PricePair`] | the contract's `uint128` pair, spot or EMA | [`math::pricing`] |
| Capacity and its draw | [`MarketCapacity`] | capacity and open interest from one block | [`math::capacity`] |
| The pool at a block | [`PoolSnapshot`] | price, liquidity and a tick map that reconciles with it | [`math::swap`] |
| A settle previewed | [`MakerEquityBreakdown`] | exact atoms, the contract's arithmetic | [`math::maker_equity`] |
| A block | [`BlockContext`] | number, hash, timestamp of one header | [`math`] |
| An event | [`MarketEvent`] | the market's vocabulary, human units, either tense | [`events`] |
| An event in chain order | [`TapeEvent`](history::TapeEvent), [`ChainPoint`](history::ChainPoint) | block and log index | [`history`] |
| Custody over time | [`OwnershipLog`](history::OwnershipLog) | a fold of transfers; owner at a chain point | [`history`] |
| A print | [`IndexPrint`](history::IndexPrint) | the index at a chain point and time | [`history`] |
| A failure | [`PerpCityError`] | typed, with a stated transience | [`errors`] |
| A transport | [`HftTransport`] | many endpoints, one provider, reads and writes classified | [`transport`] |
| The send path | [`TxPipeline`](hft::pipeline::TxPipeline), [`NonceManager`](hft::nonce::NonceManager) | zero RPC to prepare; the next nonce is owned | [`hft`] |
| The chain's shapes | [`contracts`], `storage` | bindings match deployed bytecode; slots match the deployed layout | [`contracts`] |
| The human surface | [`types`], [`convert`] | inert data in human units; conversion once, at the edge | [`types`] |

Three shapes recur and are worth naming, because a new type should be one
of them or have a reason not to be.

A **handle** owns a scope and hands out reads in a tense: `ChainReader`
for a chain, `MarketReader` for a market now, `StateAt` for a market at a
block. Handles are cheap to clone and carry no policy of their own beyond
their tense.

A **snapshot** is a read's result that carries the block it came from.
Its fields all come from that block, and any further pinned read derives
its hash from it. A snapshot is inert; the math that consumes it is pure
and lives in `math`.

A **validated value** is a type whose constructor is the only place its
invariant is checked: `TickRange` is the model. The check happens at the
boundary the value entered, a chain read, a config, an event, and the
math downstream trusts it. A loose pair of fields that together carry an
invariant is the smell this shape exists to remove.

## Edges

The component nodes, what each provides to the rest, and what it takes.

- **`client`**: the handles and the reads. Provides `ChainReader`,
  `MarketReader`, `StateAt`, `PerpClient`, `TxBuilder`. Consumes
  `contracts` and `storage` for shapes, `math` for the snapshot types it
  fills and the ports it feeds, `hft` for the send path, `errors` for
  classification. The tense split lives here.
- **`math`**: pure ports of contract math over pre-fetched inputs. Provides
  the geometry, pricing, capacity, swap and settle types the client fills
  and strategies compute with. Consumes nothing from the chain: no
  provider ever appears in `math`.
- **`events`**: the vocabulary. Provides `MarketEvent` and the decoder.
  Consumed by `feeds` and `history`, which never re-decode, and by
  everything above that folds events.
- **`feeds`** and **`history`**: the two tenses of events. `feeds` provides
  the live streams; `history` provides scans, the tape, the ownership
  fold, prints and transfers. Both consume `events`.
- **`hft`**: the execution machinery. Provides the pipeline, nonce and gas
  caches, the state cache the now-reads serve from. Consumed by `client`.
- **`transport`**: endpoints, health and routing. Provides `HftTransport`.
  Everything that talks to a node goes through it.
- **`errors`**: the failure taxonomy and its transience. Consumed
  everywhere; the classification of a node's message into a typed
  variant happens at the read that saw it.
- **`contracts`** and **`storage`**: the deployed shapes. Bindings, ABI
  locks, slot derivation. Consumed by `client` and `math`; never by a
  strategy directly.
- **`types`** and **`convert`**: the human surface and the unit boundary.
  Consumed by `client` on the way out and by callers on the way in.

**Legion** is the edge out of this crate. Its agent vocabulary builds on
these types rather than redefining them: a strategy's band is a
`MakerBand`, its range a `TickRange`, its pool snapshot a `PoolSnapshot`,
its events `MarketEvent`s. Where Legion had its own copy of a chain fact,
the copy was a defect and this crate grew the type. Legion's root node
records what it consumes and why.

## Terminology

The cross-cutting words. A component node owns the words it introduces;
these are the ones every node uses.

- **Market**: one `Perp` contract, identified by its address.
- **Pool price**: the AMM's spot, `ammPrice` on chain. Not the mark.
- **Index**: the beacon's print.
- **EMAs**: the contract's smoothed pool price and index, a `PricePair`,
  advanced from the last touch.
- **Mark**: the fair price of the pool price, the index and the advanced
  EMAs; what the contract values positions at.
- **Taker, maker**: the two kinds of position; a maker's geometry is a
  **band**, a **range** with liquidity.
- **Capacity, open interest, headroom, utilization**: what bands can back,
  what takers hold, the difference, and the ratio.
- **Atoms, X96, X128, WAD, e6**: the wire units, always suffixed. USDC,
  perp tokens, prices and fractions: the human units, never suffixed.
- **Now, at a block**: the two tenses of a read. A **snapshot** carries its
  block. **Pinned** means read by hash. The **lagged snapshot block** is
  the head less the lag.
- **Feed, tape**: the two tenses of events. A **print** is a beacon
  update; a **chain point** is a block and log index.
- **Transient**: a failure a retry can fix. The variant says.
- **Era**: a contract version whose logs are on chain forever.

## Debts

- Maker equity still resolves its own block and builds its own `Mark`
  from a nine-view multicall rather than reading through `StateAt`
  (SDK #120). Until it moves, the tense rule has one exception.
- `PerpSnapshot` carries its block by number only, since a head read has
  no header to carry. It is the one snapshot without a hash.
- `is_transient` classifies some failures by which path wrapped them
  rather than by what they were (SDK #115). The pinned reads classify
  correctly; the bare contract calls do not yet.
- The maker-equity port, the deployed-era event shapes and the storage
  slot reads exist to compensate for the deployed contracts. The next era
  exposes settle previews and richer events, and the cutover deletes
  them wholesale.
- `hft::position_manager` and `hft::state_cache` are shaped for one bot on
  one market. They are older than the handle split and have not been
  re-examined against it.
- Not every component has a design node yet. `client`, `math` and
  `events` come with this root; the rest are written by the first change
  that touches them, and until then this node is their only design.
