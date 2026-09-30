# `feeds`: the present tense

Up: the [root](../../DESIGN.md). Sideways: [`events`](../events/DESIGN.md)
for the vocabulary the market feed speaks; [`history`](../history/DESIGN.md)
for the same vocabulary in the past tense; [`transport`](../transport/DESIGN.md)
for the WebSocket underneath.

## Purpose

This module delivers the chain as it happens: a market's events as they
are emitted, block headers as they arrive, and, for a taker that quotes
in memory, a pool snapshot republished whenever it is refreshed. It is
the zero-RPC-per-read half of a trading loop: once subscribed, a consumer
learns the market's state from what the chain says rather than by
asking.

It does not interpret. The market feed filters logs by address and topic
and hands them to the one decoder; the header feed yields headers; the
taker feed publishes what someone else read. Policy about what to do with
a header or an event is the consumer's.

## What matters

**A feed is a subscription, and subscriptions die.** WebSocket
connections drop, providers rotate, and a subscription that was silently
lost is worse than one that failed loudly, because the consumer keeps
trading on a frozen view. So the manager reconnects with backoff, and the
consumer owns re-subscribing after a reconnect, since only it knows what
it was subscribed to. A dead feed shows up as silence, and the layer
above must treat silence as staleness rather than calm.

**Reconnection does not backfill.** Events emitted during a gap are gone
from the feed. A consumer that must not miss events bootstraps from the
tape and reconciles after a gap; the feed alone is not a source of truth
about the past, only about the present.

**A published snapshot is atomic or nothing.** The taker feed refreshes
by several reads and publishes only when every read succeeded against
one block hash. A consumer never sees a snapshot whose fields came from
different blocks, and a failed refresh leaves the last good snapshot in
place with its age visible.

**Costs are per subscription, not per read.** A header subscription
bills every block on some providers; on a chain producing four blocks a
second that burned a monthly allotment once. The base-fee poller in
Legion replaced it for that reason. A feed that is cheap to read is not
necessarily cheap to hold, and the choice is the consumer's, made with
the bill in view.

## The mental model

Three feeds, one connection manager.

`MarketFeed` is one market's present: a subscription filtered to the
`Perp`'s address and its beacon's, every matching log decoded into a
`MarketEvent`, consumed by calling `next` in a loop. There is no market
id; the address filter is the market.

`BlockHeaderFeed` is the chain's clock: each new header as it lands,
carrying the base fee a gas cache wants.

`LiveTakerMarket` is a shared, block-atomic `PoolSnapshot`: a publisher
refreshes it through the pinned pool read and publishes; consumers hold
the latest, watch for changes, and quote against it entirely in memory.
It is the feed shape for a value that is read rather than emitted.

`WsManager` is the connection under all three: one WebSocket serving any
number of subscriptions, reconnecting with capped exponential backoff,
leaving re-subscription to its callers.

## The type system

| Type | What it is | The invariant it carries |
|---|---|---|
| [`MarketFeed`] | one market's events as they happen | filtered to the market and its beacon; every log decoded by [`decode_log`](crate::events::decode_log); nothing interpreted here |
| [`BlockHeaderFeed`] | headers as they land | one header per block, in order |
| [`LiveTakerMarket`], [`LiveTakerMarketPublisher`] | a shared pool snapshot | published only when every read succeeded at one block hash; consumers see the latest and its currency |
| [`WsManager`](crate::transport::ws::WsManager), [`ReconnectConfig`](crate::transport::ws::ReconnectConfig) | the connection | one socket, many subscriptions; reconnects with backoff; re-subscription is the caller's |

## Edges

- From the [root](../../DESIGN.md): the two tenses of events; latency as a
  design input.
- To [`events`](../events/DESIGN.md): every log the market feed receives
  goes through the one decoder. The feed never matches on a topic to
  interpret it.
- Sideways to [`history`](../history/DESIGN.md): the same vocabulary from
  the past. A consumer bootstraps from the tape, then follows the feed;
  the two agree on every event they both saw.
- To [`transport`](../transport/DESIGN.md): the WebSocket manager lives
  there; the feeds are subscriptions over it.
- To [`client`](../client/DESIGN.md): the taker feed's publisher refreshes
  through `MarketReader::get_pool_snapshot`; the header feed's consumers
  push the base fee into `ChainReader::set_base_fee`.
- Out to Legion: the live plane's market-data cache is fed by
  `MarketFeed` and seeded from a snapshot read; its staleness guard is
  how silence is treated as staleness.

## Terminology

- **Feed**: a subscription that yields values as they happen. **Present
  tense**: what a feed delivers, as against the tape.
- **Subscribe, next**: open the stream, take the next value.
- **Reconnect**: re-establish the socket after a drop; **re-subscribe**:
  what the caller does afterwards. **Backoff**: the growing wait between
  reconnect attempts.
- **Gap**: events emitted while the feed was down; not backfilled.
- **Publish**: replace the shared snapshot with a new block-atomic one.
  **Current**: whether the shared snapshot is recent enough to quote on.

## Debts

- **`LiveTakerMarket` keeps its old name.** It is a live `PoolSnapshot`
  for takers; the type should say pool, as the snapshot it publishes now
  does.
- **The header feed's cost is the reason Legion polls instead.** A
  cheaper header source, or a feed that coalesces, would let the gas cache
  follow the chain again rather than a timer.
- **Re-subscription after reconnect is manual.** Every consumer writes
  the same loop; a subscription that remembers its filters and replays
  them on reconnect would remove the most common way a feed dies quietly.
