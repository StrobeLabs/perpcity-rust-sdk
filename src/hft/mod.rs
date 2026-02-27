//! High-frequency trading infrastructure.
//!
//! This module provides lock-free, zero-RPC-on-hot-path primitives for
//! latency-sensitive trading:
//!
//! | Module | Purpose |
//! |---|---|
//! | [`nonce`] | Lock-free nonce acquisition with pending-tx tracking |
//! | [`gas`] | EIP-1559 gas fee caching with urgency-based scaling |
//! | [`pipeline`] | Transaction pipeline combining nonce + gas (zero RPC on prepare) |
//! | [`state_cache`] | Multi-layer TTL cache for on-chain state |
//! | [`latency`] | Rolling-window latency tracker with percentile stats |
//! | [`position_manager`] | Position tracking with stop-loss / take-profit / trailing-stop triggers |
//!
//! All modules accept explicit timestamps for deterministic testing —
//! no hidden clock dependencies.

pub mod gas;
pub mod latency;
pub mod nonce;
pub mod pipeline;
pub mod position_manager;
pub mod state_cache;
