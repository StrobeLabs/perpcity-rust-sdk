//! Multi-endpoint RPC transport with health-aware routing.
//!
//! This module provides a transport layer that vastly improves on
//! the Zig SDK's hand-rolled connection management by leveraging
//! Alloy's transport system and tower middleware composition.
//!
//! | Module | Purpose |
//! |---|---|
//! | [`config`] | Transport configuration with builder pattern |
//! | [`health`] | Per-endpoint circuit breaker and latency tracking |
//! | [`provider`] | `HftTransport` — tower Service → Alloy Transport |
//! | [`ws`] | WebSocket subscription manager with auto-reconnect |
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │            RootProvider<HftTransport>            │
//! │  (standard Alloy Provider: get_block, send_tx)  │
//! └──────────────────────┬──────────────────────────┘
//!                        │ tower::Service<RequestPacket>
//! ┌──────────────────────▼──────────────────────────┐
//! │               HftTransport                      │
//! │  • Read/write classification                    │
//! │  • Retry for reads (with backoff)               │
//! │  • Hedged requests (fan out, take fastest)      │
//! │  • Endpoint selection (round-robin / latency)   │
//! └──┬─────────────┬─────────────┬──────────────────┘
//!    │             │             │
//! ┌──▼──┐      ┌──▼──┐      ┌──▼──┐
//! │EP 0 │      │EP 1 │      │EP 2 │  Per-endpoint:
//! │     │      │     │      │     │  • BoxTransport (HTTP)
//! │ CB  │      │ CB  │      │ CB  │  • CircuitBreaker
//! │ EMA │      │ EMA │      │ EMA │  • Latency EMA
//! └─────┘      └─────┘      └─────┘
//! ```

pub mod config;
pub mod health;
pub mod provider;
pub mod ws;
