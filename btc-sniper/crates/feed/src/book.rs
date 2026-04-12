//! Local limit order book (one per Polymarket asset/token).
//!
//! Design notes:
//!
//! * The book uses two `BTreeMap<Price, Size>` — one for bids (descending
//!   iteration order gives best bid) and one for asks.
//! * Writes go through a `parking_lot::RwLock`. Readers (signal, dashboard)
//!   use the same lock but only ever take it briefly to snapshot top-of-book
//!   values into a stack-allocated [`BookSnapshot`].
//! * The hot read path (`best_bid`/`best_ask`/`mid`) is O(log N) against the
//!   BTreeMap, not O(1), but at typical book depth (<200 levels) the
//!   wall-clock cost is well under a microsecond and avoids the cache-churn
//!   of maintaining separate top-of-book atomics.
//! * All prices use the [`Price`] scaled-integer type — no floats on the hot
//!   path.

use std::collections::BTreeMap;

use parking_lot::RwLock;

use crate::types::{Price, Size};

/// One aggregated price level (price + total size).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PriceLevel {
    pub price: Price,
    pub size: Size,
}

/// Top-of-book snapshot returned to readers — purely stack-allocated.
#[derive(Clone, Copy, Debug, Default)]
pub struct BookSnapshot {
    pub best_bid: Option<PriceLevel>,
    pub best_ask: Option<PriceLevel>,
    pub bid_depth: u32,
    pub ask_depth: u32,
    /// Last update sequence number (monotonic).
    pub seq: u64,
}

impl BookSnapshot {
    /// Mid price, or `None` if the book is one-sided.
    #[inline]
    pub fn mid(&self) -> Option<Price> {
        let (b, a) = (self.best_bid?, self.best_ask?);
        Some(Price((b.price.0 + a.price.0) / 2))
    }

    /// Spread in basis points, or `None` if one-sided.
    #[inline]
    pub fn spread_bps(&self) -> Option<i32> {
        let (b, a) = (self.best_bid?, self.best_ask?);
        Some(a.price.edge_bps(b.price))
    }

    /// Order-book imbalance in `[-1.0, 1.0]`. Positive → buy pressure.
    pub fn imbalance(&self) -> f64 {
        let (b, a) = match (self.best_bid, self.best_ask) {
            (Some(b), Some(a)) => (b.size.0 as f64, a.size.0 as f64),
            _ => return 0.0,
        };
        let total = b + a;
        if total == 0.0 {
            0.0
        } else {
            (b - a) / total
        }
    }
}

/// Limit order book for a single Polymarket asset.
#[derive(Debug)]
pub struct OrderBook {
    inner: RwLock<BookInner>,
}

#[derive(Debug, Default)]
struct BookInner {
    bids: BTreeMap<Price, Size>,
    asks: BTreeMap<Price, Size>,
    seq: u64,
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(BookInner::default()),
        }
    }

    /// Completely replace the book from a CLOB snapshot message.
    pub fn apply_snapshot(&self, bids: &[(Price, Size)], asks: &[(Price, Size)], seq: u64) {
        let mut inner = self.inner.write();
        inner.bids.clear();
        inner.asks.clear();
        for &(p, s) in bids {
            if s.0 > 0 {
                inner.bids.insert(p, s);
            }
        }
        for &(p, s) in asks {
            if s.0 > 0 {
                inner.asks.insert(p, s);
            }
        }
        inner.seq = seq;
    }

    /// Apply an incremental diff. A size of [`Size::ZERO`] deletes the level.
    pub fn apply_diff(&self, bids: &[(Price, Size)], asks: &[(Price, Size)], seq: u64) {
        let mut inner = self.inner.write();
        for &(p, s) in bids {
            if s.0 == 0 {
                inner.bids.remove(&p);
            } else {
                inner.bids.insert(p, s);
            }
        }
        for &(p, s) in asks {
            if s.0 == 0 {
                inner.asks.remove(&p);
            } else {
                inner.asks.insert(p, s);
            }
        }
        inner.seq = seq;
    }

    /// Take a stack-allocated top-of-book snapshot.
    pub fn snapshot(&self) -> BookSnapshot {
        let inner = self.inner.read();
        let best_bid = inner
            .bids
            .iter()
            .next_back()
            .map(|(&price, &size)| PriceLevel { price, size });
        let best_ask = inner
            .asks
            .iter()
            .next()
            .map(|(&price, &size)| PriceLevel { price, size });
        BookSnapshot {
            best_bid,
            best_ask,
            bid_depth: inner.bids.len() as u32,
            ask_depth: inner.asks.len() as u32,
            seq: inner.seq,
        }
    }

    /// Current sequence number — useful for out-of-order diff detection.
    pub fn seq(&self) -> u64 {
        self.inner.read().seq
    }
}

impl Default for OrderBook {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64) -> Price {
        Price::from_prob(x)
    }
    fn s(x: f64) -> Size {
        Size::from_usdc(x)
    }

    #[test]
    fn snapshot_and_mid() {
        let book = OrderBook::new();
        book.apply_snapshot(
            &[(p(0.50), s(100.0)), (p(0.49), s(200.0))],
            &[(p(0.52), s(150.0)), (p(0.53), s(250.0))],
            1,
        );
        let snap = book.snapshot();
        assert_eq!(snap.best_bid.unwrap().price, p(0.50));
        assert_eq!(snap.best_ask.unwrap().price, p(0.52));
        assert_eq!(snap.mid().unwrap(), Price(510_000));
        assert_eq!(snap.spread_bps().unwrap(), 200);
    }

    #[test]
    fn diff_delete_and_insert() {
        let book = OrderBook::new();
        book.apply_snapshot(&[(p(0.50), s(100.0))], &[(p(0.55), s(100.0))], 1);
        // delete bid, insert new ask
        book.apply_diff(
            &[(p(0.50), Size::ZERO)],
            &[(p(0.54), s(80.0))],
            2,
        );
        let snap = book.snapshot();
        assert!(snap.best_bid.is_none());
        assert_eq!(snap.best_ask.unwrap().price, p(0.54));
        assert_eq!(snap.seq, 2);
    }

    #[test]
    fn imbalance_signs() {
        let book = OrderBook::new();
        book.apply_snapshot(&[(p(0.50), s(300.0))], &[(p(0.51), s(100.0))], 1);
        let imb = book.snapshot().imbalance();
        assert!(imb > 0.0); // buy pressure
    }
}
