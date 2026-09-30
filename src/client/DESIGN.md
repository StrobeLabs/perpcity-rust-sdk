# `client`: the handles, the reads, and the sends

Up: the [root](../../DESIGN.md). Sideways: [`math`](../math/DESIGN.md)
for the types the reads fill, [`events`](../events/DESIGN.md) for the
vocabulary the feeds and history speak, and `hft`, `transport`, `errors`
and `contracts` for the machinery underneath.

## Purpose

This module is how a caller addresses the chain. It answers three
questions and nothing else: what does this market hold, at which block;
what would this action do; and send this, and tell me what happened.
Every read a strategy makes of a Perp City market and every transaction
it sends passes through here, so this is where a read's block becomes a
fact and a send's outcome becomes unambiguous.

It is not where math lives. A read here fills a snapshot type from
`math` and hands it over; the computation on it is pure and lives there.
And it is not where policy lives: the client never decides whether to
trade, only how to read and how to send.

## What matters

**Scope is a ladder, and each rung is a type.** Three things have
different lifetimes and different sharing: a chain (one transport, one
deployment set, caches every reader shares), a market on it (one `Perp`),
and a signer on a market (one wallet, one nonce sequence). Before the
split these were one type, and every consumer that was not one bot on one
market built its own workaround: a research reader without a signer, a
treasury service holding a vector of clients over one cloned transport, a tool
building a fresh client per market inside a loop. The workarounds were
the evidence the type was wrong. Now `ChainReader`, `MarketReader` and
`PerpClient` are the three rungs, each cheap to clone, each holding
exactly its scope, and a helper that needs one rung accepts anything
above it through `AsRef`.

**A read's tense is the receiver's, never the caller's.** The single
most expensive class of bug in a trading system is two values that
disagree on which block they describe: open interest from one block
against capacity from another, a position against a mark eight blocks
newer. The client removes that class by making block policy a property
of the type a read hangs off. `MarketReader` reads *now*; `StateAt` reads
*at a block*, and the handle is the block. There is no method that takes
a block argument. If a caller needs two values to agree, it takes one
handle and reads both on it, and the types make the alternative
unavailable.

**A send must be accountable after the fact.** A transaction that lands
moves real money, and a transaction whose fate is unknown is worse than
one that failed. So the send path resolves its uncertainty rather than
reporting it: every failure after broadcast carries the hash to look up;
a doubtful nonce is repaired from the chain before the next send, never
rewound locally; and nothing is submitted while the sequence is in
doubt. The pipeline owns the account's next nonce so that ordering is a
fact the client holds rather than a race with the node.

**Probes are sends without the send.** Whether a liquidation would
succeed is a question only the contract can answer, and it depends on who
asks: the deployed contracts charge the sender for bad debt, so a probe
from an unfunded address gets a different answer than the funded sender's
transaction would. A probe is therefore an `eth_call` from the address
that will send, at the gas cap the send will use, and its typed reverts
are the answer: healthy, wrong kind of position, or would succeed.

**Batches are one block or nothing.** A read over many positions is one
multicall per stage, every stage at the same block, and a failure is
scoped to the smallest unit it actually broke: one position, one chunk,
or the market-wide read that everything depends on. A batch never
silently drops an id; every input gets an outcome, and the outcome says
whether a retry can help.

## The mental model

Think of the module as three handles and one path.

`ChainReader` is the chain. It owns the transport and the provider, the
chain id and the deployment addresses, the history handle, the base-fee
cache and the state cache. It answers questions addressed to something
other than a market: a wallet's balances, a beacon's index, the lagged
block. Build one per process; everything else shares it.

`MarketReader` is a market on that chain: the `Perp` address and the
chain reader, nothing more. Its reads are the *now* tense, from the head
or the cache, each independently current, named `get_*`. It hands out the
other tense through `state()` and `state_at(number)`.

`StateAt` is that market at one block. Constructing it resolves the
header once; every read on it is pinned to the hash, and the results
carry the block. Its reads have no prefix: `capacity`, `pool`, `mark`,
`solvency`, `position`, `maker_band`. The three reads that once pinned
their own block each (capacity, margin ratios, the pool) live here, and
`MarketReader` keeps four one-line conveniences over a fresh lagged
handle for callers that want one value and do not care about agreement:
`get_capacity`, `get_margin_ratios`, `get_pool_snapshot` and `get_mark`.
Those four are the `get_*` reads whose block is the lagged snapshot
block rather than the head; their results carry it.

`PerpClient` is a signer on a market: a `MarketReader` plus the wallet
and the transaction pipeline. Every write goes through it, and it is
`AsRef` to both readers, so a read helper written against a reader takes
a client unchanged.

```text
   ChainReader                                          one per process
   │   transport, provider, deployments, history(), the caches
   │
   └─ MarketReader  = ChainReader + a Perp address       one per market
      │   get_* (now) · state() / state_at(n) → StateAt (at a block)
      │
      └─ PerpClient = MarketReader + a signer            one per wallet
             tx() · the trades · a pipeline that owns the nonce

   AsRef<ChainReader>   ChainReader, MarketReader, PerpClient
   AsRef<MarketReader>  MarketReader, PerpClient
```

Each rung adds one thing to the rung above and owns nothing from below:
the chain reader has no market, the market reader has no signer. Sharing
runs the other way, so one chain reader serves every market in a
process and every client of a market shares its reader's caches. The two
`AsRef` lines are the whole API for "what does this function need": a
bound names the innermost rung a function uses, and anything built over
that rung satisfies it. A helper that takes `&PerpClient` to read is
over-asking, and the bound is how a reviewer sees it.

The path is the send. `TxBuilder` collects one transaction's parameters
and `send` does the whole thing: repair the nonce sequence if the last
send left it in doubt, simulate (an `eth_estimateGas` that doubles as the
simulation, or a cached limit plus an `eth_call` preflight, so a revert
always decodes to a typed error before anything is signed), take a nonce
and gas from the pipeline with no RPC, sign, broadcast, poll for the
receipt. Each stage has its own failure variants, and the receipt is the
only success.

```text
 stage         success             failure ──► variant           transient?  nonce      hash
 ────────────  ──────────────────  ──────────────────────────────  ──────────  ─────────  ─────
 resync        sequence repaired   in flight, cannot repair yet
   (only if    from the chain's    └─► NonceDesynced               yes         untouched  none
   desynced)   count
     │
     ▼
 simulate      gas limit known;    contract reverts (typed)
   estimate    a revert would      └─► SimulationReverted          no          untouched  none
   or cached   have been decoded   empty revert, or out of gas at the cap
   + preflight                     └─► SimulationFailed            no          untouched  none
                                   node unreachable, fee stale
     │                             └─► GasUnavailable              yes         untouched  none
     ▼
 prepare       nonce acquired,     pipeline full
   zero RPC    fees resolved       └─► TooManyInFlight             yes         untouched  none
     │
     ▼
 sign          signed bytes        local failure
   local                           └─► SigningFailed               no          RELEASED   none
     │
     ▼
 broadcast     hash accepted       request failed after signing
                                   └─► BroadcastFailed             yes         DOUBTFUL   known
     │
     ▼
 receipt       mined, succeeded ─► Ok(receipt)                                 RESOLVED   known
   poll        mined, reverted     └─► Reverted                    no          RESOLVED   known
               mined, out of gas   └─► OutOfGas                    no          RESOLVED   known
               no receipt in time  └─► ReceiptTimeout              yes         DOUBTFUL   known
```

The two right-hand columns are the whole point of the diagram: they are
the two facts a caller must know after any failure, and every row
answers both. Before `sign`, nothing has left the process, so the nonce
is untouched and no hash exists; a signing failure is provably local, so
the nonce it acquired is handed straight back. From `broadcast` on, the
hash is known before the request is made, so a caller can always look
the transaction up. `RESOLVED` means the chain consumed the nonce, for
better or worse. `DOUBTFUL` is the state the design exists for: the
transaction may or may not have landed, no local bookkeeping can tell,
and both reusing and rewinding the nonce are wrong. So the pipeline is
marked desynced, no send starts until nothing is in flight, and the next
send takes the chain's transaction count, which includes the doubtful
transaction if and only if it is live. Nothing is ever rewound. The
transience column is what a retry loop keys on: a `no` means the same
send would fail the same way.

Around the path sit the trades, thin over it: each takes human-unit
parameters, scales them once, and delegates to an exact twin that is the
single submission path. Limits follow the swap's direction. And the
probes: the liquidation `eth_call`s that ask the contract the question the
send would ask.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`ChainReader`] | one chain: one transport, one deployment set, one set of caches shared by everything built over it | [`ChainReader::new`] over an [`HftTransport`](crate::transport::provider::HftTransport) and a [`ChainDeployments`](crate::types::ChainDeployments); [`ChainReader::arbitrum`] and [`ChainReader::arbitrum_sepolia`] for the known chains | [`ChainReader::market`], which is how every market reader is made; [`ChainReader::history`], which hands out the scanning handle; the wallet and index reads; any helper bounded on `AsRef<ChainReader>`. The strategy layer builds one per process, and that sharing is why the caches live here and not on a market. |
| [`MarketReader`] | one market, read now: a `Perp` over a `ChainReader`; every read is independently current; no read takes a block | [`ChainReader::market`] | [`MarketReader::state`] and [`MarketReader::state_at`], the door to the other tense; [`PerpClient::new`], as the market a signer trades; [`LiveTakerMarket::subscribe`](crate::feeds::LiveTakerMarket::subscribe), as the reader a publisher refreshes through; any helper bounded on `AsRef<MarketReader>`. It is the reader a live cache seeds from and the one a research process holds without a signer, which is why it exists apart from `PerpClient`. |
| [`StateAt`] | one market at one block: the handle resolved one header; every read is pinned to its hash; results carry the block | [`MarketReader::state`], at the lagged snapshot block; [`MarketReader::state_at`], at a block the caller names | its own reads, which fill the snapshots in `math` and the state types in `types`, and are where the tense rule is enforced; the strategy layer's block-pinned sources, which hold it behind a trait so a forensic read can be stubbed. |
| [`PerpClient`] | one signer on one market: a `MarketReader` plus a wallet and a pipeline; the pipeline owns the next nonce | [`PerpClient::new`] from a [`MarketReader`] and any alloy signer | [`PerpClient::tx`] for a raw call, and the trades, probes and transfers over it, each a builder plus a decode of the receipt; the [`TxBuilder`] borrows it for the pipeline and the wallet. The strategy layer holds one per wallet; a helper that only reads should not take it. |
| [`TxBuilder`] | one transaction, not yet sent: one nonce, one hash, one typed outcome | [`PerpClient::tx`] | [`TxBuilder::send`], the only way out, which drives the pipeline and returns the receipt. Every trade on `PerpClient` goes through it, and the strategy layer uses it directly for a call the trades do not cover. Its shape, parameters first and one `send`, is what makes every failure variant a stage. |
| [`MakerEquityOutcome`], [`MakerEquityKind`] | one position's result in a batch: one outcome per input id, in input order; `Computed` carries a [`MakerEquityBreakdown`](crate::math::maker_equity::MakerEquityBreakdown); `Failed` carries an error whose transience says whether to retry | [`MarketReader::get_maker_equities`], [`MarketReader::get_maker_equities_at_mark`] | nothing in the crate. The strategy layer's liquidation scanners and equity audits, which retry the transient failures and act on the rest; the per-position shape exists so one bad row cannot fail the batch. |
| `MarketImmutables` (crate-private) | a market's deployment-fixed values: pool id and tick spacing; read once per market, never pinned | the first pinned pool read on a market, then the chain reader's cache | the pool reads, and no caller |

The two right-hand columns are where the type flows. A link under
*Produced by* is the function that makes one, or the type it is built
from. A link under *Consumed by* is a function that takes the type, or a
type built from it, with the reason it is shaped for that consumer. A
method on the type itself is neither.

Three things about the shape are deliberate.

The `AsRef` ladder is the whole API for "what does this helper need". A
function that only reads a market is bounded on `impl AsRef<MarketReader>`
and takes a reader or a client; one that only needs the chain is bounded
on `impl AsRef<ChainReader>` and takes any of the three. A helper that
takes `&PerpClient` to read is over-asking, and the bound is how a
reviewer sees it.

Snapshot types are filled here and defined in `math`. `StateAt::pool`
fills a `PoolSnapshot`, `capacity` a `MarketCapacity`, `mark` a `Mark`,
and the maker-equity batch a `MakerMarketSnapshot` behind its outcomes.
The client's job is to read the right views at the right block and hand
the inert result to pure math. No arithmetic on chain values happens
here beyond unit scaling at the edge.

Errors are classified at the read that saw them. A pinned read maps the
node's "pruned state" answer to `StateUnavailable`, not transient, and
its "no such block" answer to `BlockUnavailable`, transient, because the
read knows the block it asked about and the caller's retry loop needs to
know which it was. A now-read at the head has no block to name and
passes the transport's error through.

## Efficiency

Every read's cost, in requests, at which block, and what is cached. A
request is one JSON-RPC call; a multicall is one request however many
views it batches.

| Read | Requests | Block | Cached |
|---|---|---|---|
| `get_pool_price`, `get_funding_rate` | 1 | head | fast layer, 2 s |
| `get_open_interest` | 1 | head | no |
| `get_perp_config` | 3, plus 3 for fees and bounds on a slow-layer miss | head | slow layer, 60 s |
| `get_perp_data` | 3 | head | no |
| `get_perp_snapshot` | 1 multicall + 1 pinned index, plus the slow layer on a miss | one block, the head | slow layer for fees and bounds |
| `state()` | 2 (block number, header) | lagged | no |
| `state_at(n)` | 1 (header) | named | no |
| `get_capacity`, `get_margin_ratios`, `get_pool_snapshot`, `get_mark` | `state()` plus the pinned read below | lagged | as the pinned read |
| `StateAt::solvency`, `next_pos_id`, `position`, `maker_band`, `pool_tick`, `collateral` | 1 each | pinned | no |
| `StateAt::capacity` | 1 multicall | pinned | no |
| `StateAt::margin_ratios` | 3 | pinned | no |
| `StateAt::mark` | 1 multicall + 1 pinned index | pinned | no |
| `StateAt::pool` | 1 multicall + index + bounds + bitmap, + tick words when any tick is set: 4 or 5; plus 2 for the immutables once per market per process | pinned | immutables, forever |
| `get_maker_equities` | market-wide multicall + index once; per chunk of at most 500 ids: 1 row multicall + 1 `extsload` + 1 `eth_getProof` (or up to 16 concurrent `eth_getStorageAt` where proofs are not served); 4 chunks in flight | one lagged block for the whole batch | no |
| `get_positions_by_owner` | 1 + one `ownerOf` per id ever minted | head | no |
| `get_balances_batch` | 1 multicall | head | fast layer |
| liquidation probes | 1 `eth_call` at the liquidation gas cap | head | no |

A send costs one simulation (`eth_estimateGas`, or one `eth_call`
preflight when the limit is cached or explicit), zero requests to
prepare, one broadcast, and receipt polls every 2 s after a 2 s initial
delay for up to 30 s. A resync, when the sequence was in doubt, is one
`eth_getTransactionCount` before the next send. Nothing on the hot path
between deciding and broadcasting makes a request.

Two costs in this table are not what they should be and are debts:
`margin_ratios` is three requests where one multicall would do, and
`get_positions_by_owner` is linear in every position ever minted.

## Edges

- From the [root](../../DESIGN.md): the two tenses, the handle and
  snapshot shapes, the unit boundary.
- To [`math`](../math/DESIGN.md): every snapshot the reads fill is
  defined there, as is every port the reads feed. `math` never sees a
  provider; this module never does the arithmetic. The contract between
  them is a snapshot type: inert, block-stamped, exact.
- To [`events`](../events/DESIGN.md) and `history`: `ChainReader::history`
  hands out the past tense; the feeds are built over the same transport.
  The client does not decode events.
- To `hft`: the pipeline, the nonce manager and the gas cache are the
  send's machinery; the state cache is what now-reads serve from. The
  client is their only caller.
- To `transport`: every request goes through `HftTransport`, whose
  read/write classification is what makes a read retryable and a write
  not.
- To `errors`: the variants this module produces are defined there with
  their transience; this module decides which variant a node's answer is.
- To `contracts` and `storage`: the bindings the reads call and the slots
  the batches read. Shapes, not policy.
- Out to the strategy layer: every reader a strategy holds is one of
  these three handles; a research source over block-pinned reads is
  `StateAt` behind a trait; a live cache is seeded from
  `get_perp_snapshot` and then follows the feed.

## Terminology

- **Handle**: a value that owns a scope and hands out reads in a tense.
  The three are the **chain reader**, the **market reader** and the
  **state handle**; a **client** is a market reader with a signer.
- **Now**: the tense of a `MarketReader` read; the head at the moment of
  the call, or the cache within its TTL. **At a block**: the tense of a
  `StateAt` read; pinned to the handle's hash.
- **Lagged snapshot block**: the head less `SNAPSHOT_BLOCK_LAG`, what
  `state()` pins; far enough back that every replica has it.
- **Pinned**: read at a block hash. **Convenience**: a `get_*` on the
  market reader that is one read on a fresh lagged handle.
- **Send**: the whole path from builder to receipt. **Simulate**: the
  preflight every send runs before signing. **Resync**: repairing a
  doubtful nonce sequence from the chain's transaction count.
- **Probe**: an `eth_call` that asks whether a send would succeed, from
  the address that would send it.
- **Batch**: a many-position read at one block, in multicall stages.
  **Chunk**: the slice of a batch one stage reads at once; a chunk's shared
  read failing fails the chunk, not the batch.
- **Immutables**: a market's deployment-fixed values, read once.
- **Fast layer, slow layer**: the state cache's two TTLs; now-reads of
  prices, funding and balances come from the fast one, fees and bounds
  from the slow one.

## Debts

- **Maker equity is off the handle.** `get_maker_equities` resolves its
  own lagged block and builds its own `Mark` from a nine-view multicall.
  It is the one read that still pins its own block, and until SDK #120
  moves it onto `StateAt` the tense rule has an exception.
- **`get_positions_by_owner` is a linear scan** of every id ever minted,
  because the chain offers no owner index. It is correct and slow; the
  ownership fold over the tape (`history`) is the right answer for anything
  above a handful of positions.
- **`PerpSnapshot` carries its block by number only.** A head read has no
  header to carry a hash from. It is the one snapshot a caller cannot pin
  further reads to.
- **The liquidation twins are keyed by an enum called `Book`.** It names
  which kind of position a liquidation targets, and "book" is retired
  vocabulary; it should be a position kind.
- **`get_perp_config` is three separate calls** where `get_perp_snapshot`
  is one batch; the older read predates the batch and has not been folded
  into it.
- **Transience is by wrapping path for bare calls.** A now-read that
  fails at the transport surfaces as an ABI error the classification does
  not recognise as transient (SDK #115). Pinned reads classify correctly;
  the head reads do not yet.
- **The deployed-era compensation lives partly here.** The maker-equity
  batch and its storage reads exist because the deployed contracts expose
  no settle preview; the next era's `previewPosition` replaces the batch
  wholesale, and this module loses its largest file at the cutover.
