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

**Unknown is `None`, never a guess.** A log the decoder does not
recognise, an admin event, an ERC-20 transfer sharing the ERC-721
`Transfer` topic, returns `None` and the caller skips it. A log the
decoder does recognise but cannot decode is an error, because a gap in
a tape is worse than a failure.

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

## The type system

| Type | What it is | The invariant it carries |
|---|---|---|
| [`MarketEvent`] | one event, either tense | the same value from the same log, however delivered; human units; unknown shapes are `None` |
| [`SwapInfo`] | a taker swap's outcome | the four fee legs sum to the total; deltas are the swap's, signed as V4 signs them |
| [`MakerSettle`] | what a touch credited a maker | the settle the chain performed, not a preview |
| [`CumulativesInfo`] | the accounting trackers at an accrual | raw X96 and X128, verbatim; conversion is the consumer's |
| [`decode_log`] | the decoder | one function; every era's shape; `None` for the unknown, an error for the malformed |

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
  are the fix, and the research crate's reconciliation names the gap
  until then.
- **`ModifyLiquidity` and `IndexUpdated` are in the vocabulary but not on
  a market's tape**, because they are emitted by other addresses. A tape
  is one address's logs; the beacon's series and the pool's liquidity
  changes are separate scans. A market-shaped scan across addresses is
  SDK #101.
- **The path `feeds::events` still re-exports this module** from before it
  moved to the crate root. It should go once downstream imports from the
  new path.
