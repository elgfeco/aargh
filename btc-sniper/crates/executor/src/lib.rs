//! # sniper-executor
//!
//! Polymarket CLOB order placement, cancellation, and lifecycle tracking.
//!
//! Submodules:
//!
//! * [`client`] — authenticated HTTP client (reqwest, HTTP/2, keep-alive)
//! * [`order`] — order structs + EIP-712 pre-signing helpers
//! * [`manager`] — stateful `OrderManager` that owns open orders, applies
//!   TTL-based stale cancellation, and emits fills to downstream consumers
//! * [`latency`] — `hdrhistogram`-backed per-stage timing buckets with a
//!   SIGUSR1 snapshot dump
//!
//! The HOT path (decision → order fired) goes through
//! [`manager::OrderManager::submit`] which never allocates: the order
//! template is pre-serialized at startup and only the `price`, `size`, and
//! `salt` fields are swapped at fire time.

pub mod client;
pub mod latency;
pub mod manager;
pub mod order;

pub use client::ClobClient;
pub use latency::{LatencyStage, LatencyStats};
pub use manager::{OrderManager, OrderManagerConfig};
pub use order::{Order, OrderId, OrderState, OrderTemplate};
