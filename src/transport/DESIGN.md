# `transport`: how requests reach a node

Up: the [root](../../DESIGN.md). Everything that talks to the chain,
[`client`](../client/DESIGN.md), [`history`](../history/DESIGN.md),
[`feeds`](../feeds/DESIGN.md), goes through here; [`errors`](../errors/DESIGN.md)
is where a transport failure's meaning is decided.

## Purpose

This module turns several RPC endpoints of varying health into one
provider the rest of the crate can treat as reliable. It classifies
every request as a read or a write, retries what is safe to retry,
routes around endpoints that are failing, hedges reads that are worth
racing, and reconnects the WebSocket the feeds live on. It is the layer
where "the node did not answer" is turned into a decision rather than a
panic.

It is not where failures are named for the caller. The transport decides
what to do with a request; `errors` decides what a failure means to a
strategy.

## What matters

**Reads and writes are different acts.** A read that fails can be sent
again to any endpoint with no consequence. A write that fails after the
transaction was signed may already be in a mempool, and sending it again
is either idempotent (the same bytes) or catastrophic (a new nonce). So
the transport classifies every request, retries reads freely and writes
only on a pre-mempool rejection or on no answer, and never invents a
second transaction. The execution model is: reads are cheap to repeat,
writes are cheap to repeat only as the same bytes, and the caller
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

A request enters as a packet, is classified read or write, and is
routed. Reads go to the read pool if one is configured and healthy, else
the shared pool, by round robin, latency, or hedged fan-out. Writes go to
the write pool, else shared. Each endpoint is a boxed HTTP transport
behind its own breaker; a breaker that opens takes the endpoint out of
rotation until a probe succeeds.

```text
                        a request (tower::Service<RequestPacket>)
                                        │
                              classify: read or write?
                          ┌─────────────┴──────────────┐
                        READ                          WRITE
                          │                             │
             read pool healthy?                 write pool healthy?
              yes │    │ no → shared pool          yes │    │ no → shared pool
                  ▼    ▼                               ▼    ▼
        ┌──────────────────────┐             ┌──────────────────────┐
        │ Strategy             │             │ one endpoint         │
        │  round robin         │             │ same signed bytes    │
        │  latency-based       │             └──────────┬───────────┘
        │  hedged: fan out,    │                        │
        │   first answer wins, │              answered?
        │   others cancelled   │           no │        │ rejected pre-mempool
        └──────────┬───────────┘              │        │ (nonce too low, gas, …)
                   │                          │        ▼
             answered?                        │   retry, same bytes
        no │            │ declined            │
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
when it declined. A write retries when the node rejected it before the
mempool, and when the node did not answer, with the same signed bytes;
the caller trusts the receipt over the error.

The WebSocket manager is the subscription side of the same idea: one
socket, many subscriptions, reconnect with capped backoff, and the
callers re-subscribe.

## The type system

| Type | What it is | The invariant it carries |
|---|---|---|
| [`HftTransport`](provider::HftTransport) | the provider's transport | many endpoints, one `tower::Service`; every request classified read or write before routing |
| [`TransportConfig`](config::TransportConfig), [`TransportConfigBuilder`](config::TransportConfigBuilder) | the pools and their policies | shared, read and write pools; per-endpoint timeouts; the retry and breaker settings |
| [`ReadRetryConfig`](config::ReadRetryConfig), [`WriteRetryConfig`](config::WriteRetryConfig) | what is retried | reads on no answer only; writes on pre-mempool rejection or no answer, same bytes |
| [`Strategy`](config::Strategy) | how a read is routed | round robin, latency-based, or hedged; a caller's choice |
| [`EndpointPool`](provider::EndpointPool), [`EndpointHealth`](health::EndpointHealth), [`EndpointStatus`](health::EndpointStatus), [`CircuitState`](health::CircuitState) | health | per-endpoint breaker, latency EMA, decaying error rate; readable without a lock |
| [`CircuitBreakerConfig`](config::CircuitBreakerConfig) | when a breaker trips | failure threshold, cooldown, probe |
| [`ProbePermit`](provider::ProbePermit) | one half-open probe at a time | a breaker probes with one request, not a flood |
| [`Reserved`](health::Reserved) | an endpoint held for a purpose | a scan's endpoint is not the trading loop's |
| [`WsManager`](ws::WsManager), [`ReconnectConfig`](ws::ReconnectConfig) | the subscription side | one socket; reconnect with capped backoff; re-subscription is the caller's |

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
- **A write retries up to 3 times**, only on a pre-mempool rejection or
  no answer, always as the same signed bytes, so a retry can never cost
  a second transaction's gas.
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
  environment; redacting keyed endpoint URLs in logs is the caller's
  rule, since the key is in the URL and this module never logs one.

## Terminology

- **Endpoint**: one RPC URL. **Pool**: the endpoints for a kind of
  request; shared, read or write. **Transport**: all of them behind one
  provider.
- **Read, write**: the two kinds of request; classified before routing.
- **Retry**: the same request again, under the kind's rule. **Hedge**:
  the same read to several endpoints at once, first answer wins.
- **Decline**: the endpoint answered no to this request; not a failure of
  the endpoint. **No answer**: timeout or dropped connection; a failure
  of the endpoint.
- **Breaker**: per-endpoint state that takes it out of rotation;
  **closed, open, half-open**: its states; **probe**: the one request that
  tests a half-open breaker.
- **Reserved**: an endpoint held for one purpose.

## Debts

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
