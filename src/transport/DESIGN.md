# `transport`: how requests reach a node

Up: the [root](../../DESIGN.md). Everything that talks to the chain,
[`client`](../client/DESIGN.md), [`history`](../history/DESIGN.md),
[`feeds`](../feeds/DESIGN.md), goes through here; [`errors`](../errors/DESIGN.md)
is where a transport failure's meaning is decided.

## Purpose

This module turns several RPC endpoints of varying health into one
provider the rest of the crate can treat as reliable. It sends reads and
signed raw transactions only, refuses every other request before it
leaves, retries what is safe to retry,
routes around endpoints that are failing, hedges reads that are worth
racing, and reconnects the WebSocket the feeds live on. It is the layer
where "the node did not answer" is turned into a decision rather than a
panic.

It is not where failures are named for the caller. The transport decides
what to do with a request; `errors` decides what a failure means to a
strategy.

## What matters

**Reads and writes are different acts, and this transport sends one
kind of write.** A read that fails can be sent again to any endpoint with
no consequence. A write that fails after the transaction was signed may
already be in a mempool, and sending it again is either idempotent (the
same bytes) or catastrophic (a new nonce). So `HftTransport` is a trading
transport: it sends a method on an allowlist of reads without side
effects, alone or in a batch of reads, and a single
`eth_sendRawTransaction`, and nothing else. Every other method, an
unknown or new one included, and any batch that holds one is refused
before it is sent. It retries reads freely, retries the raw send only as
the same signed bytes, after an error answer or no answer, and never
invents a second transaction. The execution model is: reads are cheap to
repeat, writes are cheap to repeat only as the same bytes, and the caller
reconciles a doubtful write by its receipt.

**A declined request is not a failing endpoint.** A provider that
answers a too-wide log query with a client error is doing its job; one
that times out is not. Counting the first against health would open the
circuit breaker on a healthy endpoint during every scan. So a decline is
answered to the caller unchanged and left off the health record, and a
narrowing search can decline as often as it needs.

**Health is per endpoint and lock-free to read.** Each endpoint has a
circuit breaker (closed, open, half-open with a probe), a latency EMA
and a decaying error rate. Selection reads atomic mirrors of that state
so that the steady state, all endpoints healthy, takes no lock.

**Hedging trades rate limit for latency, and says so.** A hedged read
fans out to several endpoints and takes the first answer, cancelling the
rest. It is faster and costs more requests. It is a strategy the caller
chooses, not a default.

**Costs are the provider's units.** Compute units, monthly allotments,
per-subscription billing. A design that is correct but burns the
allotment is wrong; the header subscription that was replaced by a poller
is the standing example.

## The mental model

A request enters as a packet, is refused unless the serialized body of
each call holds the method and id of its metadata and it is a read (or a
batch of reads) or a single raw send, and is routed. Reads go to the read pool if one is configured and its breakers
give an endpoint (a closed circuit, or a probe of an open one whose recovery timeout has passed), else
the shared pool, by round robin, latency, or hedged fan-out. Writes go to
the write pool, else shared. Each endpoint is a boxed HTTP transport
behind its own breaker; a breaker that opens takes the endpoint out of
rotation until a probe succeeds.

```text
                        a request (tower::Service<RequestPacket>)
                                        │
           read, batch of reads, or one eth_sendRawTransaction?
                     anything else → refused, nothing sent
                          ┌─────────────┴──────────────┐
                        READ                       RAW SEND
                          │                             │
             read pool healthy?                 write pool healthy?
              yes │    │ no → shared pool          yes │    │ no → shared pool
                  ▼    ▼                               ▼    ▼
        ┌──────────────────────┐             ┌──────────────────────┐
        │ Strategy             │             │ one endpoint         │
        │  round robin         │             │ same signed bytes    │
        │  latency-based       │             │ typed id, tx hash    │
        │  hedged: fan out,    │             │ checked              │
        │   first answer under │             └──────────┬───────────┘
        │   its id wins, others│                        │
        │   cancelled          │              answered?
        └──────────┬───────────┘           no │        │ error answer
                   │                          │        │ (underpriced, funds, …)
             answered?                        │        ▼
        no │            │ declined            │   retry, same bytes
           ▼            ▼                     ▼
     retry w/ backoff  return as is,     retry, same bytes; outcome of
     (endpoint failed  health untouched  the first attempt unknown →
      to answer)       (endpoint did     the caller trusts the receipt
                        its job)

   per endpoint:   ┌────────┐ failures ≥ threshold ┌──────┐ cooldown ┌──────────┐
                   │ CLOSED │─────────────────────►│ OPEN │─────────►│ HALF-OPEN│
                   └────────┘                      └──────┘          └────┬─────┘
                        ▲   probe succeeds                                │ one probe
                        └─────────────────────────────────────────────────┘ (ProbePermit)
                                            probe fails → OPEN again
```

The top is the decision every request goes through, and the two columns
differ in exactly one way: what "try again" is allowed to mean. A read is
stateless at the node, so it can be sent again to anyone, hedged across
several, and retried with backoff when the endpoint failed to answer.
A write is a signed transaction, so "again" can only ever be the same
bytes, which the node treats as idempotent, and never a fresh
transaction; and when the first attempt got no answer, its outcome is
unknown, which is why the send path reports the hash and the caller
reconciles by receipt. The middle distinction, declined versus no answer,
is the one that keeps scans from tripping breakers: an endpoint that says
no to a too-wide log request has done its job, and that answer goes back
to the caller with the endpoint's health untouched. The bottom is the
per-endpoint breaker: it takes an endpoint out of rotation on repeated
failures, lets one probe through after the cooldown, and closes again
only when the probe succeeds.

A read retries with backoff when the endpoint failed to answer, never
when it declined. A single `eth_sendRawTransaction` retries after an
error answer and after no answer, with the same signed bytes; the caller
trusts the receipt over the error. Its last explicit rejection
(`underpriced`, a fee or funds answer) is passed on as the node's answer
only when every attempt got one: after an attempt with no answer, or
with another answer (`already known`, `nonce too low`), a node can hold
the transaction, and the rejection comes back as a transport error that
names it. Its answer counts only under its own id, compared as typed (the
number `0` is not the string `"0"`); a response under another id or of
another shape is no answer, and is retried as one. So is a body that is
no JSON-RPC response, a bare error object with no id included, and a
success whose result is no transaction hash, such as an error object in
its place: a write's rejection is only an error response under its own
id.

Every other request is refused before it is sent, with a local error
(`RpcError::LocalUsageError`): `HftTransport sends only single signed
eth_sendRawTransaction writes; <method> is not supported`, and for a
batch `; a batch may hold only reads` after it. That covers a call the node signs
(`eth_sendTransaction`, `personal_sendTransaction`), whose nonce the node
picks, so that a resend after a lost reply could be a second trade; a
synchronous or conditional send; `eth_subscribe` over HTTP; an unknown
method; and a batch with any of them, a raw send included. The read list
names the reads the SDK sends and the common reads next to them, and a
test sends each read the SDK uses through alloy and checks that the list
is exactly those. A list of reads fails safe: a new send method is
refused, not retried as a read. Earlier rounds sent these writes once and
checked their answers; each check added a case on a path the SDK never
uses, so the transport now sends only what the SDK sends: it signs
locally and sends one `eth_sendRawTransaction`. A raw send or a read
with a null id, which alloy cannot tell from an answer with no id, is
refused too, and so are a batch whose calls repeat an id, whose answers
could not be told apart, and an empty batch, which a node answers with
one error and not a batch.

An answer that does not answer its request counts against the
endpoint's health: an answer under another id or of another shape, a
body that is no JSON-RPC response (a bare error object with no id, which
alloy decodes as an error response, included), and a raw send's success
that is no transaction hash. A read's answer that answers each call by
its id counts against the endpoint when any answer in it is a rate
limit, once for the whole batch; any other answer, an error under the
call's own id included, is the endpoint working. An execution error
(code `3`, a revert wording, revert data) is never a rate limit, even
when the contract reverts with the words "rate limit". A read that got
no answer under its id is asked again within the read retries, because
alloy takes a single answer without reading its id and would hand the
caller another call's result; a hedged read waits for an answer under
its id from another endpoint.

Every check above reads a call's method and id from alloy's metadata,
but the endpoint gets the call's serialized JSON body, and a packet built
by hand at the public `Service<RequestPacket>` entry point can make the
two disagree. So a call whose body holds another method or id, compared
as typed, or is no single call, is refused before endpoint selection
with `RpcError::LocalUsageError`, and nothing is sent.

The WebSocket manager is the subscription side of the same idea: one
socket, many subscriptions, reconnect with capped backoff, and the
callers re-subscribe.

## The type system

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`HftTransport`](provider.rs#L613) | the provider's transport: many endpoints, one `tower::Service`; sends reads and single signed raw transactions, refuses every other request before routing | [`HftTransport::new`](provider.rs#L623) from a [`TransportConfig`](config.rs#L167) | [`ChainReader::new`](../client/DESIGN.md), [`ChainReader::arbitrum`](../client/DESIGN.md) and [`ChainReader::arbitrum_sepolia`](../client/DESIGN.md), which build the provider over it, so every read and send in the crate passes through it. The strategy layer builds one per process, and a second one for scans so their declines and timeouts never touch the trading endpoints' health. |
| [`TransportConfig`](config.rs#L167), [`TransportConfigBuilder`](config.rs#L201) | the pools and their policies: shared, read and write pools; per-endpoint timeouts; the retry and breaker settings. [`build`](config.rs#L303) validates the configuration, and transport construction also checks the probe limit | the builder or a caller using the public fields | [`HftTransport::new`](provider.rs#L623). The strategy layer assembles one from its environment; the builder is where a keyed URL enters the crate. |
| [`Strategy`](config.rs#L53) | how a read is routed: round robin, latency-based, or hedged; a caller's choice | the caller | [`TransportConfigBuilder::strategy`](config.rs#L275). Hedging costs N requests per read, which is why it is a choice and not a default. |
| [`ReadRetryConfig`](config.rs#L101) | what a read retries: no answer only, with backoff | the caller; `Default` is 2 retries | [`TransportConfigBuilder::read_retry`](config.rs#L287). |
| [`WriteRetryConfig`](config.rs#L134) | what a single `eth_sendRawTransaction`, the only write the transport sends, retries: an error answer or no answer, the same bytes; every other write is refused before it is sent | the caller; `Default` is 3 retries | [`TransportConfigBuilder::write_retry`](config.rs#L293). |
| [`CircuitBreakerConfig`](config.rs#L69) | when a breaker trips: failure threshold, cooldown, positive probe limit | the caller; `Default` is the trading defaults | [`TransportConfigBuilder::circuit_breaker`](config.rs#L281); [`EndpointHealth::new`](health.rs#L117), one breaker per endpoint. |
| [`EndpointHealth`](health.rs#L78), [`ProbePermit`](provider.rs#L220), [`Reserved`](health.rs#L32) | one endpoint's health: breaker, latency EMA, decaying error rate, readable without a lock, a failure dated when it arrives; a bounded number of probes; a permit for one request | [`EndpointHealth::new`](health.rs#L117), one per endpoint in a pool | the transport pools select over the tracker. |
| [`ProbeSlot`](health.rs#L51) | a single-use slot bound to its issuing tracker and recovery cycle | issued by `reserve` in `Reserved::Probe`. [`EndpointHealth::record_answered`](health.rs#L268), [`EndpointHealth::record_success`](health.rs#L290) and [`EndpointHealth::release_probe`](health.rs#L241) return a foreign slot to its caller | [`EndpointHealth::record_answered`](health.rs#L268), [`EndpointHealth::record_success`](health.rs#L290) and [`EndpointHealth::release_probe`](health.rs#L241) settle the slot on its issuing tracker. |
| [`EndpointStatus`](health.rs#L101), [`CircuitState`](health.rs#L58) | one endpoint's health, read out | [`HftTransport::health_status`](provider.rs#L642) | nothing in the crate. The strategy layer's metrics, which is why it is a plain value and not a handle. |
| [`WsManager`](ws.rs#L69) | the subscription side: one socket; reconnect with capped backoff; re-subscription is the caller's | [`WsManager::connect`](ws.rs#L94) with a [`ReconnectConfig`](ws.rs#L43) | [`MarketFeed::subscribe`](../feeds/DESIGN.md), [`BlockHeaderFeed::subscribe`](../feeds/DESIGN.md) and [`LiveTakerMarket::subscribe`](../feeds/DESIGN.md), each one subscription over it. The strategy layer holds one per process. |
| [`ReconnectConfig`](ws.rs#L43) | the backoff: initial, maximum, multiplier, attempts | the caller; `Default` is capped exponential | [`WsManager::connect`](ws.rs#L94). |

## Efficiency

The transport trades requests for latency and reliability, and every
trade is a setting a caller chose.

- **A hedged read costs N requests for one answer**, one per endpoint
  fanned out to, with the losers cancelled the moment the first answer
  lands so their cost stops at the request already sent. It buys the
  fastest endpoint's latency at N times the request cost; it is a
  `Strategy` the caller selects, not a default.
- **A read retries up to 2 more times**, with backoff, and only when the
  endpoint failed to answer. A decline costs the one request and no
  retry.
- **A write retries up to 3 times**, on an error answer or no answer,
  always as the same signed bytes, so a retry can never cost a second
  transaction's gas, even after an attempt that may have succeeded.
- **Selection is lock-free in the steady state.** Health is read from
  atomic mirrors, so choosing an endpoint costs no contention when every
  endpoint is healthy.
- **A breaker saves requests.** An endpoint that has failed past the
  threshold receives one probe per cooldown, not the trading loop's
  traffic.
- **One socket serves every subscription** on the WebSocket side;
  reconnection costs one handshake plus the re-subscriptions.

## Edges

- From the [root](../../DESIGN.md): failure is classified; latency is a
  design input; costs are the provider's units.
- To [`errors`](../errors/DESIGN.md): the transport passes failures up as
  transport errors; the read that saw one decides what it means. A
  transport error reaching the caller unwrapped is transient by
  definition, because the transport already spent its retries.
- From [`client`](../client/DESIGN.md): every read and every send. The
  write classification is what makes `TxBuilder::send`'s accounting
  possible.
- From [`history`](../history/DESIGN.md): scans are the reason declines
  are off the health record. A research process builds its own transport
  for scans so its slow requests cannot open the breaker the agents trade
  through.
- From [`feeds`](../feeds/DESIGN.md): the WebSocket manager is here;
  the feeds are subscriptions on it.
- Out to the strategy layer: a process builds one transport from its
  environment. The key is in the URL, so this module never logs or traces
  one, and its errors leave it with the URL cut: an HTTP endpoint is sent
  to without alloy's `Http`, whose request span carries the full URL.
  [`redact_text`](redact.rs#L71) is for the caller's own logs.

## Terminology

- **Endpoint**: one RPC URL. **Pool**: the endpoints for a kind of
  request; shared, read or write. **Transport**: all of them behind one
  provider.
- **Read, write**: the two kinds of request; classified before routing.
- **Retry**: the same request again, under the kind's rule. **Hedge**:
  the same read to several endpoints at once, first answer wins.
- **Decline**: the endpoint answered no to this request; not a failure of
  the endpoint. **Explicit rejection**: a node's answer to a write that
  says it does not hold the transaction. **No answer**: timeout or dropped connection; a failure
  of the endpoint.
- **Breaker**: per-endpoint state that takes it out of rotation;
  **closed, open, half-open**: its states; **probe**: the one request that
  tests a half-open breaker.
- **Reserved**: an endpoint held for one purpose.
- **Probe slot**: one of the half-open breaker's probe places, held by
  one request's permit; **recovery cycle**: the group of probes whose
  slots name the same cycle. A late failure from a request sent while closed
  holds no slot, ends the current half-open cycle and costs one more recovery window.

## Accepted structure

- **`Reserved` is the verdict of `reserve`.** The pool uses it to build
  a `ProbePermit`. The permit gives the slot back when dropped. Only the
  issuing tracker's current cycle can use the slot to close the circuit
  or free a probe. A foreign tracker returns the slot unchanged.

- **Single-endpoint selection prefers closed circuits.** This includes hedged
  writes. Hedged reads can probe with spare fan-out. With a closed candidate,
  recovery cycles start at least one recovery interval apart.

## Debts

- **As on main, single-endpoint routing probes only when all peers in the pool
  have failed.** Recovery with a healthy peer needs a separate, bounded health
  read so live requests can use the healthy peer.
- **Write retries cover outcomes the safety argument does not** (SDK
  #107). A write retried on no answer is idempotent as bytes, but the
  first attempt's outcome is unknown, and the argument that the receipt
  reconciles it depends on the caller doing so.
- **Two edges in health accounting and selection** are known to be
  slightly wrong (SDK #108); neither has bitten.
- **Whether an endpoint serves `eth_getProof` is learned by failing**,
  once per process, rather than recorded (SDK #116).
- **The module doc still compares itself to the Zig SDK.** The comparison
  was true and is now history; the design should stand on its own
  reasons.
