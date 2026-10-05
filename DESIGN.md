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

**Money is exact, and its unit is a type.** Every quantity the contract
settles is an integer in a unit the market is denominated in, and the SDK
carries it as one until the last moment. Which unit is the type, not the
field's spelling, so a function's signature is its unit list and the two
pairs that look alike as integers — the two assets, a price and its square
root — cannot be swapped. The f64 surface exists for humans and
dashboards, never for arithmetic the chain will check. Ported contract math is transcribed one-to-one in `U256` and
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
beside it. Two builds are deployed, and a market's build is a fact read
once from its pool key; where the two differ the client chooses by it.
Events from the first audited deployment onward stay decodable for as long
as the crate exists, because a market's history is a fold over them and
anyone with the crate should be able to take it. The beta builds before it
are development history: served while they are live, deleted at the
cutover, and kept as tapes recorded before it.

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
numbered as NFTs, its own funding, fees and solvency, and six modules
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
The [`math`](src/math/DESIGN.md) node draws the relation.

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

```text
   now                                      at a block
   ───────────────────────────────          ─────────────────────────────────
   MarketReader::get_*                      MarketReader::state()      ─┐
                                            MarketReader::state_at(n)  ─┤
       │ each call on its own                                           ▼
       ▼                                                            StateAt
   ┌────────────┐  hit   ┌───────┐              resolves ONE header:  { block: number, hash, ts }
   │ state cache│───────►│ value │                                          │
   └─────┬──────┘        └───────┘              every read on the handle    │
         │ miss / uncached                      is pinned to that hash      ▼
         ▼                                       ┌────────┬─────────┬──────────┐
   ┌────────────┐                                │capacity│  pool   │   mark   │ ...
   │  the head  │  whatever block it is now      └────────┴─────────┴──────────┘
   └────────────┘                                     all from the same block

   head ──────────────────────────────────────────────────────────► block number
                                     ▲                        ▲
                                     └─ state(): head − 8 ────┘  the lagged snapshot block
                                        (every replica has it)
```

The left side is the tense a caller gets without asking: whatever the
head is at the moment of the call, or the cache's copy within its TTL,
and two such reads promise nothing about each other. The right side is
the tense a caller asks for by taking a handle. `state()` pins the lagged
block, eight behind the head, because on a load-balanced endpoint the
newest block is not yet on every replica; `state_at(n)` pins a block the
caller names. Once the handle exists, the reads on it are the same
functions with the same names and no block argument, because the block
is the handle's, not the call's.

**Two tenses of events, one vocabulary.** The feed streams the present
over a WebSocket; the tape replays the past from log scans. Both produce
the same decoded `MarketEvent`, so anything that folds events, ownership,
economics, a series, works on either tense unchanged. The crate's own
fold of that kind is the replay: a market's state rebuilt from its events
into the same types the pinned reads return, so what the chain said and
what the chain holds are compared with `==`.

**Two unit systems, one boundary.** The chain speaks in counts of atoms
and in fixed-point encodings; humans speak in USDC, perp tokens, prices
and fractions. The chain's side is one type per unit, in
[`units`](src/units/DESIGN.md), so a signature says what it takes; the
human side is `f64`, reached by a conversion on the type that says what it
costs. The crossing happens at the crate's surface and never in the middle
of math.

**Sending is a pipeline with an execution model.** Prepare with no RPC
(nonce from the manager, gas from the cache), sign locally, broadcast,
then poll for the receipt. Each stage has its own failure variants, and
the nonce manager's job is to make the account's next nonce a fact the
pipeline owns rather than a race with the node.

**Eras.** Two deployed builds are live, `58b42b7` and
`v0.2.2-upgradeable`, and a market is one of them for life; the contracts
repository's main branch is the next, and the first the crate promises to
keep decodable. Calls target only what is deployed, by the market's era
where the builds differ. The cutover to the audited build replaces both
live eras wholesale: their bindings, their event shapes and the
compensation they needed are deleted, not gated, and their markets'
histories live on as recorded tapes, readable by the release that
recorded them.

## The type system

How the model above becomes types. Each row names the concept, the type
that carries it, the invariant it holds, and the node that owns it.

| Concept | Type | Invariant | Home |
|---|---|---|---|
| A chain | [`ChainReader`](src/client/chain.rs#L56) | one transport, one deployment set, shared caches | [`client`](src/client/DESIGN.md) |
| A market, now | [`MarketReader`](src/client/market.rs#L26) | one `Perp` over a `ChainReader`; every read is current | [`client`](src/client/DESIGN.md) |
| A market, at a block | [`StateAt`](src/client/state.rs#L70) | the handle is the block; every read pinned to its hash | [`client`](src/client/DESIGN.md) |
| A market with a signer | [`PerpClient`](src/client/mod.rs#L182) | a `MarketReader` plus the send pipeline | [`client`](src/client/DESIGN.md) |
| A send | [`TxBuilder`](src/client/transactions.rs#L40) | one transaction, one nonce, one outcome | [`client`](src/client/DESIGN.md) |
| A tick interval | [`TickRange`](src/math/range.rs#L25) | `lower < upper`, both in the V4 domain, checked at construction | [`math::range`](src/math/range.rs#L1) |
| A maker's geometry | [`MakerBand`](src/math/range.rs#L140) | a `TickRange` with liquidity | [`math::range`](src/math/range.rs#L1) |
| The mark's inputs | [`Mark`](src/math/pricing.rs#L114) | pool price, index and EMAs from one block, advanced to it | [`math::pricing`](src/math/pricing.rs#L1) |
| A price pair | [`PricePair`](src/math/pricing.rs#L41) | the contract's `uint128` pair, spot or EMA | [`math::pricing`](src/math/pricing.rs#L1) |
| Capacity and its draw | [`MarketCapacity`](src/math/capacity.rs#L59) | capacity and open interest from one block | [`math::capacity`](src/math/capacity.rs#L1) |
| The pool at a block | [`PoolSnapshot`](src/math/swap.rs#L71) | price, liquidity and a tick map that reconciles with it | [`math::swap`](src/math/swap.rs#L1) |
| A settle previewed | [`MakerEquityBreakdown`](src/math/maker_equity.rs#L177) | exact atoms, the contract's arithmetic | [`math::maker_equity`](src/math/maker_equity.rs#L1) |
| A block | [`BlockContext`](src/math/mod.rs#L51) | number, hash, timestamp of one header | [`math`](src/math/DESIGN.md) |
| An event | [`MarketEvent`](src/events.rs#L156) | the market's vocabulary, human units, either tense | [`events`](src/events/DESIGN.md) |
| An event in chain order | [`TapeEvent`](src/history/tape/mod.rs#L66), [`ChainPoint`](src/history/tape/mod.rs#L57) | block and log index | [`history`](src/history/DESIGN.md) |
| Custody over time | [`OwnershipLog`](src/history/tape/custody.rs#L33) | a fold of transfers; owner at a chain point | [`history`](src/history/DESIGN.md) |
| A market rebuilt from its events | [`Replay`](src/history/replay/mod.rs#L137) | the reads' types, folded from the tape; equal to the reads at a block | [history/replay](src/history/replay/DESIGN.md) |
| A print | [`IndexPrint`](src/history/beacon.rs#L20) | the index at a chain point and time | [`history`](src/history/DESIGN.md) |
| A failure | [`PerpCityError`](src/errors/mod.rs#L45) | typed, with a stated transience | [`errors`](src/errors/DESIGN.md) |
| A transport | [`HftTransport`](src/transport/provider.rs#L523) | many endpoints, one provider, reads and writes classified | [`transport`](src/transport/DESIGN.md) |
| The send path | [`TxPipeline`](src/hft/pipeline.rs#L125), [`NonceManager`](src/hft/nonce.rs#L42) | zero RPC to prepare; the next nonce is owned | [`hft`](src/hft/DESIGN.md) |
| The chain's shapes | [`contracts`](src/contracts/DESIGN.md), `storage` | bindings match deployed bytecode; slots match the deployed layout | [`contracts`](src/contracts/DESIGN.md) |
| The unit boundary | `convert` | a human unit becomes a wire unit once, at the edge | [`client`](src/client/DESIGN.md) |

This table is the index; where each type flows is in the component
nodes. Each node's type table has the same two right-hand columns,
*Produced by* and *Consumed by*: the function that makes the type or the
type it is built from, and the function that takes it or the type built
from it, with the reason it is shaped for that consumer. A method on the
type itself is neither. The phrase "the strategy layer" in a consumer
cell marks a type as part of the surface the layer above the crate
builds on. A type with no producer or no consumer is a type to question.

The type graph itself is not written; it is read out of the signatures.
`cargo xtask design` builds rustdoc's JSON for the crate and takes every
public function's parameters and return as edges between the crate's
types, so the graph is a fact about the code, and the tables are the
curated layer over it: every producer and consumer a row names is
checked against a real signature, and the flows a row names are the
designed edges, drawn solid on the page beside the ones no node explains.
`--check` is the gate, `--fmt` rewrites the tables' links to their
canonical files, `--report` prints the graph's numbers, `--diff` builds
the graph at another commit and says what changed, which the design job
posts on every pull request, and `--open` draws it: types as nodes clustered by component, edges labelled with the
function that carries one type into another, each type's row and source
a click away.

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

## Invariants

The claims about the type system that the gate enforces, over the graph
the signatures give. Each is one sentence here and one predicate in
`xtask`, and the sentence is the error. The two are tied: the check fails
when a predicate runs that no bullet below opens with, or a bullet below
has no predicate behind it. A change that breaks one either restores it
or changes this list, in the same PR, with the reason.

- No read on a handle takes a block argument: the block is the handle's,
  and `state_at` is the one door to a named one.
- Every snapshot in `math` carries a `BlockContext`.
- A validated value, a public type in `math` with no public field, comes
  only through fallible constructors: every public function returning it
  returns a `Result` or an `Option`.
- Nothing in `math` takes or returns a provider, a transport or a handle.
- Every fallible public function returns one of the crate's errors, or
  the crate's `Result`.
- Every variant of `ContractError` and `TransactionError` says in its doc
  whether it is transient.
- Every public type is in exactly one node's table.

Two more used to be reported rather than enforced: no `f64` behind a name
carrying a wire suffix, and a suffixed field holding the primitive its
suffix names. Both were about a spelling convention standing in for a type.
The unit types replaced that convention, so the last suffixed name is gone
and both checks became vacuous; they are deleted rather than kept as
predicates that can no longer fire.

One more is a ratchet rather than a rule, and runs where there is a base
to compare with, on every pull request: a structure the report
questions, an island, a dead end, a two-cycle, a flow between documented
types that no node names, may exist, but a new one arrives answered. The
pull request either removes it, names it in a type table, or names it in
a node's "Accepted structure" or "Debts", and the design job fails until
one of those is true. Which of the two sections it goes in is itself the
answer: accepted structure is a shape the report questions and the design
keeps, with the argument against the question beside it, and a debt is
work owed. The report settles only the first, so the debts stay a work
queue and the questioned counts stay a number someone can act on. A type
is answered by the node that owns it, and a pair by either of theirs, so
two types sharing a name in two components are two questions. The
same ratchet holds a type to its row: when a type's own methods or fields
change and its row does not, the row is stale by construction, and the
job says so. What we accept is written down; what we did not notice
cannot land.

Each enforced invariant is proven able to fail. `xtask/tests/fixture/lib.rs`
is a crate shaped like this one with one planted violation per invariant
and, beside each, a neighbour that must not fire; the test documents it
with the same rustdoc and asserts that every plant, and nothing else, is
reported. A new invariant is not enforced until it has its plant.

## Efficiency

Efficiency here is not speed. It is three currencies, and a component
says which it spends and where.

**Requests**, in the provider's units: calls, compute units, bytes
returned, subscriptions held. This is the currency that gets a market
maker rate-limited or billed out of a monthly allotment. A component's
cost model states what one operation costs in requests, what is
amortised (immutables read once per market, a scan width learned once
per handle, a cache within its TTL), and what is per block rather than
per call.

**Latency on the hot path**: the code between a decision to trade and a
broadcast. The standard is not fast but zero: preparing a transaction
makes no request, and a design that would add one is rejected on that
ground alone. Every other operation says whether it is on the hot path
or off it.

**Gas**, which is money. A limit too low burns it; an estimate too high
wastes a cushion; a probe at the wrong cap answers the wrong question.

Three rules follow. Throughput work and the trading path never share a
budget: a scan has its own transport and timeout, and its declines stay
off the health record. Every read that can be one request at one block
is one request at one block, and a read that is several says why. And
efficiency is measured, not asserted: `ScanStats` and the latency
tracker exist so that a node's claims can be checked.

The nodes that spend a currency carry an efficiency section stating
their costs as numbers: requests per operation, blocks, bytes, seconds.
Those numbers are specifications. The benchmarking suite and the
regression gates on the roadmap are built around them: a benchmark
asserts what a node states, and a change that moves a number updates
the section and, once the suite exists, the gate. A cost stated as an
adjective cannot be asserted and does not belong in a node.

## Edges

The component nodes, what each provides to the rest, and what it takes.
The shape first, then the reasons.

```text
                     ┌───────────────────────────────┐
                     │      the strategy layer       │  downstream, above the crate
                     └───────────────┬───────────────┘
       builds on every public type   │
  ┌──────────────────────────────────┼─────────────────────────────────┐
  │                                  ▼                                 │
  │   ┌────────────┐   calls    ┌──────────┐   consumes      ┌──────┐  │
  │   │  convert   │◄───────────│  client  │────────────────►│ math │  │
  │   │   units    │  once, at  │ handles  │  snapshots in,  │ pure │  │
  │   └────────────┘  the edge  │  sends   │  results out    └──┬───┘  │
  │                             └─┬──┬───┬─┘                    │      │
  │          history() ┌──────────┘  │   └──────────┐ shapes    │      │
  │                    ▼             ▼              ▼           ▼      │
  │   ┌──────────┐  ┌─────────┐  ┌─────────┐  ┌───────────────────┐    │
  │   │  feeds   │  │ history │  │   hft   │  │ contracts storage │    │
  │   │ present  │  │  past   │  │ execute │  │  deployed shapes  │    │
  │   └────┬─────┘  └────┬────┘  └────┬────┘  └─────────┬─────────┘    │
  │        │ decode      │ decode     │                 │ bindings     │
  │        └──────┬──────┘            │                 │              │
  │               ▼                   │                 │              │
  │         ┌───────────┐             │                 │              │
  │         │  events   │  one vocabulary, either tense │              │
  │         └───────────┘             │                 │              │
  │                                   ▼                 ▼              │
  │   ┌─────────────────────────────────────────────────────────────┐  │
  │   │ transport   every request; reads and writes classified      │  │
  │   ├─────────────────────────────────────────────────────────────┤  │
  │   │ errors      every failure typed, with a stated transience   │  │
  │   ├─────────────────────────────────────────────────────────────┤  │
  │   │ units       what every number is, and its fixed-point math  │  │
  │   └─────────────────────────────────────────────────────────────┘  │
  └────────────────────────────────────────────────────────────────────┘
```

Read it top down as "who depends on whom". An arrow is an edge in the
graph: the node at its tail is why the types at its head are shaped as
they are. `client` is the centre because it is the only place a caller
addresses the chain: it fills the human surface on the left, hands
snapshots to `math` on the right, and reaches down to the three
machineries. `feeds` and `history` are the two tenses of the same
vocabulary, which is why both arrows land on `events`. The three bands at
the bottom have no arrows because everything above them uses them: every
request passes through `transport`, every failure is one of `errors`'
variants, and every number is one of `units`' types. The strategy layer above the crate consumes the public surface
and nothing else; it appears because its needs are why several types
exist, and it is the one edge that leaves the repository.

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
- **`convert`**: the unit boundary. The one place a human unit becomes a
  wire unit or a wire unit a human one, called once by `client` on each
  way. The human-unit types themselves are `client`'s, defined with the
  call that speaks them.
- **`units`**: what every number is. Provides one type per unit the market
  is denominated in, the one legal crossing between each dangerous pair,
  and the Solidity-compatible fixed-point arithmetic over those encodings,
  which `math`'s ports are built on. Depends on nothing but `errors` and
  `constants`, which is why every other module can take its types.

**Downstream** is the edge out of this crate: the strategy layer built on
it, ours or anyone's. Its vocabulary builds on these types rather than
redefining them: a strategy's band is a `MakerBand`, its range a
`TickRange`, its pool snapshot a `PoolSnapshot`, its events
`MarketEvent`s. Where a consumer had its own copy of a chain fact, the
copy was a defect and this crate grew the type.

## Terminology

The cross-cutting words. A component node owns the words it introduces;
these are the ones every node uses.

- **Market**: one `Perp` contract, identified by its address. `Perp` names
  the contract and appears in a binding or a client of it; everything a
  caller reads, holds or configures is named for the market.
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
- **Unit of account, encoding**: a count of something indivisible, where
  the integer is the quantity, against a packing of a continuous quantity
  into one. The first names the unit in its type, the second names the
  quantity and leaves the encoding to the accessor. The
  [`units`](src/units/DESIGN.md) node is the home of the distinction.
- **Atoms, X96, X128, WAD, e6**: a unit and four encodings. USDC, perp
  tokens, prices and fractions: the human view, always `f64`.
- **Now, at a block**: the two tenses of a read. A **snapshot** carries its
  block. **Pinned** means read by hash. The **lagged snapshot block** is
  the head less the lag.
- **Feed, tape**: the two tenses of events. A **print** is a beacon
  update; a **chain point** is a block and log index.
- **Transient**: a failure a retry can fix. The variant says.
- **Era**: a contract build and the event shapes it emits; a market's era
  is the build it was created on. The audited builds stay decodable for
  the crate's life; the beta builds before them only while live.

## Accepted structure

The report asks a question of every island, dead end and two-cycle it
finds, and each question has a usual conclusion: an island wants
connecting, a dead end wants a consumer or the surface marker, a cycle
means two types want to be one. An entry in a node's "Accepted structure"
is the answer when the shape stays: it names the types, and it says why
that conclusion does not apply here. An entry that cannot say the second
half is work owed, and belongs in that node's "Debts" instead. A node has
the section when it has something to put in it.

Two of these are about the graph rather than any component.

- **The graph is of the public surface, so a public type whose only holder
  is crate-private shows no edge.** That is the graph documenting what a
  caller can see, not a gap in it. `contracts::Modules` is the standing
  instance: the reads hold it whole, inside a view they do not export.
- **A type on the surface is named as such in a consumer cell**, because a
  type the layer above consumes has no consumer here by construction. The
  phrase is the marker, and a type that belongs to the strategy layer and
  lacks it reads as a dead end.

## Debts

- `is_transient` classifies some failures by which path wrapped them
  rather than by what they were (SDK #115). The pinned reads classify
  correctly; the bare contract calls do not yet.
- The graph reads an `&mut` output buffer as an input, so a function that
  fills a caller's vector appears to consume what it produces. One pair
  shows it today, `hft::TriggerAction` and `hft::PositionManager`, and the
  node names it; the fix is in the edge rule, not in the node.
- The maker-equity port, the deployed-era event shapes and the storage
  slot reads exist to compensate for the deployed contracts. The next era
  exposes settle previews and richer events, and the cutover deletes
  them wholesale.
- `hft::position_manager` and `hft::state_cache` are shaped for one bot on
  one market. They are older than the handle split and have not been
  re-examined against it.
- A consumer named in a type table must exist, since the doc gate
  resolves the link, but nothing yet checks that it takes the type.
