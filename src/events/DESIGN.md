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
shape that market ever emitted, and a scan over many markets meets both
live builds. So the decoder knows every shape that was ever live, and a
shape that lacks a field its sibling carries decodes to the same variant
with that field defaulted: `v0.2.2`'s `TakerClosed` has no liquidation
tails, so it reads as a voluntary close and the `TakerLiquidated` after
it says otherwise. This is the one place the era rule bends, and it bends
on purpose.

**The vocabulary is exact, and the human view is a method.** A consumer
used to read `swap.usd_delta` as dollars because the decoder had already
converted — which meant the exact word was gone upstream of every
consumer, and a fold over a million fee legs carried rounding it had no
way to avoid. Now every quantity is its unit's type, holding the
contract's own integer: the decoder narrows and never scales, a fold sums
atoms, and `.usdc()` or `.to_f64()` is called once, by the code that
wants a number for a person. The unit is in the type rather than in the
field's documentation, so a fee *rate* cannot be passed where a settled
fee belongs even if someone names them alike.

**Which way positive points is the field's.** The contract's words decide
signedness — `int256` becomes a delta type, `uint256` a count — and a
signed field says its own direction, because the market's do not agree: a
swap's deltas are positive when the position receives, while a settle's
`funding` is positive when the position *pays*. The types carry the
width and the asset; only the field can carry the direction.

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
                          │    MarketEvent    │   every quantity its unit's type
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
| [`MarketEvent`](../events.rs#L137) | one event, either tense: the same value from the same log, however delivered. **Every quantity carries its unit as a type**, with no `f64` left on the vocabulary: USDC as [`UsdcAtoms`](../units/DESIGN.md) or [`UsdcDelta`](../units/DESIGN.md) by whether the contract's word is signed, a perp amount as [`PerpAtoms`](../units/DESIGN.md) or [`PerpDelta`](../units/DESIGN.md), a price as [`Price`](../units/DESIGN.md), the two rates as [`FundingRate`](../units/DESIGN.md) and [`UtilizationRate`](../units/DESIGN.md) holding the contract's WAD exactly, liquidity as [`LUnits`](../units/DESIGN.md) and [`LDelta`](../units/DESIGN.md), a tick's checkpoints as [`Funding`](../units/DESIGN.md) and [`FundingPerSqrtPrice`](../units/DESIGN.md), and every long/short pair — capacity, open interest, the utilization rates — as one [`PerSide`](../units/DESIGN.md) field a side keys. The decoder converted to `f64` before, so the exact word was gone upstream of every consumer and a fold over a million fee legs accumulated rounding it could not avoid; now it sums integers and converts once at the end. A settled fee is spelled `liquidation_fee` rather than `liq_fee`, because the abbreviation also named a fee *rate* and the two are different quantities; a log of another vocabulary is `None`, and a log of *this* one that will not decode is an error naming the field or the signature — the two were one `None` until now, which is how a tape lost events without saying so | [`decode_log`](../events.rs#L323), the one decoder, which knows every era's shape; delivered live by [`MarketFeed::next`](../feeds/DESIGN.md) | [`TapeEvent`](../history/DESIGN.md), which stamps it with its chain point; the trades on `PerpClient`, which decode a receipt's logs with the same decoder to report a swap's deltas from the event rather than the request. The strategy layer's live cache and research folds match on it, and the ownership fold in `history` is the model: one `match`, either tense. |
| [`SwapInfo`](../events.rs#L81) | a taker swap's outcome: the four fee legs sum to the total; deltas are the swap's, signed as V4 signs them, and every field now carries its unit — the deltas as [`PerpDelta`](../units/DESIGN.md) and [`UsdcDelta`](../units/DESIGN.md), the four shares as [`UsdcAtoms`](../units/DESIGN.md), the total as a `UsdcDelta` because the contract's `totalFeeAmt` is signed while its shares are not. The price is spelled `pool_price` and typed [`Price`](../units/DESIGN.md), and the rename is the substance: it is the pool's price, not the one the contract marks at, so a basis taken against it is not the basis a liquidation uses | the decoder, inside `TakerOpened`, `TakerAdjusted` and `TakerClosed` | the taker trades' results, which report the realised deltas; the strategy layer's economics, which attribute each fee leg exactly rather than by a ratio on an aggregate. |
| [`MakerSettle`](../events.rs#L107) | what a touch credited a maker: the settle the chain performed, not a preview. `funding` is a [`UsdcDelta`](../units/DESIGN.md) positive when the position **pays** — the same direction as the equity preview's `funding_owed` — so it is subtracted from the earnings rather than added, which is why the type's own doc no longer claims a settle's parts all point one way; the utilization fees are a [`PerSide`](../units/DESIGN.md) of [`UsdcAtoms`](../units/DESIGN.md), as the preview's are | the decoder, inside the maker events | nothing in the crate. The strategy layer's maker income folds; the settle preview in `math` is checked against these. |
| [`CumulativesInfo`](../events.rs#L125) | the market's accumulators at an accrual, verbatim: a [`Funding`](../units/DESIGN.md), a [`FundingPerSqrtPrice`](../units/DESIGN.md), and the utilization payments and earnings each a [`PerSide`](../units/DESIGN.md) of [`Earnings`](../units/DESIGN.md), rather than the six bare words they used to be, so the encoding is the type's and what a reader must do with a level — take the growth since a checkpoint — is the type's too | the decoder, inside `CumulativesAccrued` | nothing in the crate. The strategy layer's reconciliation of accruals against settles, which subtracts its own checkpoint through `since`. |

The enum is exhaustive on purpose. A new event on the contract is a new
variant, and every `match` over the vocabulary that did not use a
wildcard fails to compile until it says what it does with it. Consumers
that fold economics should not use wildcards; consumers that only watch
one kind may.

## Edges

- From the [root](../../DESIGN.md): the era rule and the unit boundary.
- From `contracts`: the ABI shapes, `Perp` for the events both live
  builds share, `PerpDeployedEvents` for build `58b42b7`'s tailed maker
  closes and `PerpV022` for `v0.2.2`'s untailed taker close, locked by
  the ABI tests. The decoder is the only consumer of the event bindings.
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
