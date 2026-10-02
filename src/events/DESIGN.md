# `events`: the market's vocabulary

Up: the [root](../../DESIGN.md). Sideways: `feeds` streams these in the
present and [`history`](../history/DESIGN.md) replays them from the
past; `contracts` holds the ABI shapes decoded here.

## Purpose

This module is the one place the chain's logs become the market's
language. It defines `MarketEvent`, every event a `Perp`, its pool, its
position NFT or its beacon emits, in the units a human reasons in, and
`decode_log`, the one decoder that produces it. Everything above that
consumes events, the live cache, the tape folds, the economics, the
series, speaks this vocabulary and only this vocabulary.

It is a module and not a corner of `feeds` because it belongs to neither
tense. A feed delivers logs as they happen and a scan delivers them from
the past, and both hand them here. If decoding lived in either, the other
would re-decode or diverge.

## What matters

**One vocabulary, decoded once.** A fold over events, ownership over
time, a market's economics, an index series, must run unchanged on a live
stream and on a stored tape. That holds only if both produce identical
values from identical logs, which holds only if there is one decoder. The
feed and the history never interpret a log themselves.

**Every era's logs are on chain forever.** Calls target the deployed
contracts, but a scan from a market's deploy block will meet every event
shape that market ever emitted. So the decoder knows every shape that
was ever live, and a shape from an earlier era decodes to the same
variant as its successor with the fields it lacked defaulted. This is the
one place the deployed-versus-next-era rule bends, and it bends on
purpose.

**Human units at the boundary, raw where precision would be lost.** A
consumer should read `swap.usd_delta` as dollars and `funding_per_day` as
a fraction without touching Q96 math. But the accounting trackers,
cumulatives and tick funding, are X96 and X128 values whose meaning is
only exact as integers, so they are surfaced verbatim, named for what
they are, and converted downstream only by code that knows why.

**Unknown is `None`, never a guess; unreadable is an error.** A log that
is not this vocabulary's, an admin event, returns `None` and the caller
skips it. A log that *is* this vocabulary's and will not decode is an
error naming the field or the signature, because a gap in a tape is
worse than a failure and the two were one `None` until #146.

The one log that needs both answers is the ERC-721 `Transfer`, whose
topic0 ERC-20 shares, so the topic alone does not say whose event it is.
The arity does: two indexed fields and three topics is the ERC-20 shape
and someone else's token moving, `None`; three indexed fields and four
is this market's position NFT, and anything else at that topic claims to
be ours, so a failure to read it is a gap.

## The mental model

A market's life is a sequence of events, and every event is one of a
small number of kinds.

**Position events** say what happened to one position: opened, adjusted,
closed, converted, liquidated, backstopped, for makers and takers. A
taker event carries the swap that caused it, a `SwapInfo`: the two
deltas, the price the pool ended at, and the four fee legs exactly. A
maker event carries the settle the touch performed, a `MakerSettle`:
funding, the two utilization legs, LP fees. Events name a position id
and never a wallet; who held the position is a fold of transfers.

**Market events** say what the market's state became: capacity, open
interest, rates and EMAs at a touch, the cumulatives, ticks crossed or
initialized or deleted, the solvency moves. From these a consumer
reconstructs the market's live state without a read: the pool price from
the last swap, the index from the beacon, funding and the EMAs from the
last touch, open interest and capacity from their updates.

**Two events from outside the Perp** belong to the market anyway: the
PoolManager's `ModifyLiquidity`, since a maker's liquidity change is
logged by the pool with the position id as its salt, and the position
NFT's `Transfer`, since custody is the only way to attribute a position
to an address.

A `MarketEvent` does not carry where it happened. Its market is the
address that emitted it, and its chain position is the log's block and
index; `history` attaches those as a `TapeEvent`, and the feed's consumer
already knows which subscription delivered it.

```text
      the present                                       the past
      ───────────                                       ────────
   WebSocket subscription                        eth_getLogs scan
   (MarketFeed, one market)                      (History, a block range)
            │                                             │
            │ raw Log, as emitted                         │ raw Log, from the archive
            └──────────────────┐         ┌────────────────┘
                               ▼         ▼
                          ┌───────────────────┐
                          │    decode_log     │   one function
                          │  every era's ABI  │   not ours → None
                          └─────────┬─────────┘   ours, unreadable → error
                                    ▼
                          ┌───────────────────┐
                          │    MarketEvent    │   human units; raw where exact
                          └───┬───────────┬───┘
                    feed      │           │      tape: + ChainPoint
                              ▼           ▼            (block, log index)
                      consumer's loop   TapeEvent ─── in chain order
                      (live cache,           │
                       a strategy)           ▼
                                    folds: OwnershipLog, economics,
                                           a series, a reconciliation

           the same log yields the same MarketEvent from either side
```

The shape is a funnel, and the point is the neck. Two transports deliver
logs, and neither interprets them; both hand the raw log to the one
decoder and receive the same value back. Above the neck the two tenses
diverge only in what is attached: the feed's consumer already knows
which subscription delivered the event, while the tape stamps each one
with its chain point so that folds can join on order. That is why a fold
written over a tape, custody over time, a market's economics, an index
series, runs unchanged on a live stream: it was never written against a
transport, only against the vocabulary.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`MarketEvent`](../events.rs#L127) | one event, either tense: the same value from the same log, however delivered; human units for money, the unit's own type where the event carries a raw word — a liquidation's [`LUnits`](../units/DESIGN.md), a liquidity change's [`LDelta`](../units/DESIGN.md), a tick's [`Funding`](../units/DESIGN.md) checkpoints. A settled fee is spelled `liquidation_fee` rather than `liq_fee`, because the abbreviation also named a fee *rate* and the two are different quantities; a log of another vocabulary is `None`, and a log of *this* one that will not decode is an error naming the field or the signature — the two were one `None` until now, which is how a tape lost events without saying so | [`decode_log`](../events.rs#L314), the one decoder, which knows every era's shape; delivered live by [`MarketFeed::next`](../feeds/DESIGN.md) | [`TapeEvent`](../history/DESIGN.md), which stamps it with its chain point; the trades on `PerpClient`, which decode a receipt's logs with the same decoder to report a swap's deltas from the event rather than the request. The strategy layer's live cache and research folds match on it, and the ownership fold in `history` is the model: one `match`, either tense. |
| [`SwapInfo`](../events.rs#L74) | a taker swap's outcome: the four fee legs sum to the total; deltas are the swap's, signed as V4 signs them | the decoder, inside `TakerOpened`, `TakerAdjusted` and `TakerClosed` | the taker trades' results, which report the realised deltas; the strategy layer's economics, which attribute each fee leg exactly rather than by a ratio on an aggregate. |
| [`MakerSettle`](../events.rs#L95) | what a touch credited a maker: the settle the chain performed, not a preview | the decoder, inside the maker events | nothing in the crate. The strategy layer's maker income folds; the settle preview in `math` is checked against these. |
| [`CumulativesInfo`](../events.rs#L113) | the market's accumulators at an accrual, verbatim: six fields, each a [`Funding`](../units/DESIGN.md), [`FundingPerSqrtPrice`](../units/DESIGN.md) or [`Earnings`](../units/DESIGN.md) rather than the bare word it used to be, so the encoding is the type's and what a reader must do with a level — take the growth since a checkpoint — is the type's too | the decoder, inside `CumulativesAccrued` | nothing in the crate. The strategy layer's reconciliation of accruals against settles, which subtracts its own checkpoint through `since`. |

The enum is exhaustive on purpose. A new event on the contract is a new
variant, and every `match` over the vocabulary that did not use a
wildcard fails to compile until it says what it does with it. Consumers
that fold economics should not use wildcards; consumers that only watch
one kind may.

## Edges

- From the [root](../../DESIGN.md): the era rule and the unit boundary.
- From `contracts`: the ABI shapes, `Perp` for the current era and
  `PerpDeployedEvents` for the deployed one, locked by the ABI tests.
  The decoder is the only consumer of the event bindings.
- To `feeds`: the live stream hands every log here and forwards what
  comes back. It filters by address and topic; it does not interpret.
- To [`history`](../history/DESIGN.md): the scan hands every log here
  too, and attaches the chain position to make a `TapeEvent`. The
  ownership fold, the prints and the economics above are all folds over
  this vocabulary.
- Out to the strategy layer: a live cache's event handler and a research
  tape's folds both match on `MarketEvent`. Neither decodes.

## Terminology

- **Event vocabulary**: `MarketEvent`, the set of things the market can
  say. **Decode**: log in, vocabulary out, once.
- **Position event**: about one position id. **Market event**: about the
  market's state. **Touch**: the event of an accrual,
  `RatesAndEmasRefreshed`.
- **Swap**: a taker's trade against the pool, `SwapInfo`. **Settle**: what
  a touch credited a maker, `MakerSettle`.
- **Fee legs**: the LP, protocol, creator and insurance shares of a swap's
  fee, carried exactly.
- **Era**: a contract version's event shapes; the decoder knows every era
  a market emitted in.
- **Unknown**: a log that is not the market's vocabulary; skipped, never
  guessed.

## Debts

- **Maker events carry no price.** A maker's inventory PnL is therefore
  not on the tape; only its income is. The next contract era's events
  are the fix, and the strategy layer's reconciliation names the gap
  until then.
- **`ModifyLiquidity` and `IndexUpdated` are in the vocabulary but not on
  a market's tape**, because they are emitted by other addresses. A tape
  is one address's logs; the beacon's series and the pool's liquidity
  changes are separate scans. A market-shaped scan across addresses is
  SDK #101.
- **The path `feeds::events` still re-exports this module** from before it
  moved to the crate root. It should go once downstream imports from the
  new path.
