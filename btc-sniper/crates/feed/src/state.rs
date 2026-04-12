//! Global market-state container shared by all crates.
//!
//! `MarketState` is a simple "god object" that holds:
//!
//! * a dashmap of per-asset [`OrderBook`]s
//! * the global [`BtcTape`]
//! * monotonic counters for messages processed (for latency benching)
//!
//! It is wrapped in an `Arc<MarketState>` at startup and cloned freely into
//! every task. There is no global mutable state beyond the internal locks.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;

use crate::book::OrderBook;
use crate::btc::BtcTape;
use crate::types::AssetId;

/// Shared market state — cheap to clone (`Arc` under the hood).
#[derive(Debug)]
pub struct MarketState {
    books: DashMap<AssetId, Arc<OrderBook>>,
    tape: BtcTape,
    messages_total: AtomicU64,
    last_ws_ts_nanos: AtomicU64,
}

impl MarketState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            books: DashMap::with_capacity(32),
            tape: BtcTape::new(),
            messages_total: AtomicU64::new(0),
            last_ws_ts_nanos: AtomicU64::new(0),
        })
    }

    /// Get or create the book for `asset`.
    pub fn book(&self, asset: AssetId) -> Arc<OrderBook> {
        self.books
            .entry(asset)
            .or_insert_with(|| Arc::new(OrderBook::new()))
            .clone()
    }

    /// Iterate all known (asset, book) pairs — used by the dashboard.
    pub fn for_each_book<F: FnMut(AssetId, &OrderBook)>(&self, mut f: F) {
        for entry in self.books.iter() {
            f(*entry.key(), entry.value());
        }
    }

    pub fn tape(&self) -> &BtcTape {
        &self.tape
    }

    #[inline]
    pub fn incr_messages(&self, ts_nanos: u64) {
        self.messages_total.fetch_add(1, Ordering::Relaxed);
        self.last_ws_ts_nanos.store(ts_nanos, Ordering::Relaxed);
    }

    pub fn messages_total(&self) -> u64 {
        self.messages_total.load(Ordering::Relaxed)
    }

    pub fn last_ws_ts_nanos(&self) -> u64 {
        self.last_ws_ts_nanos.load(Ordering::Relaxed)
    }
}

impl Default for MarketState {
    fn default() -> Self {
        // Default isn't particularly useful since callers almost always want
        // the `Arc`-wrapped version, but deriving it keeps clippy quiet.
        Self {
            books: DashMap::new(),
            tape: BtcTape::new(),
            messages_total: AtomicU64::new(0),
            last_ws_ts_nanos: AtomicU64::new(0),
        }
    }
}
