//! BTC reference price tape: Binance / Coinbase / Kraken trade feeds.
//!
//! The [`BtcTape`] maintains a small ring buffer of the most recent trades
//! plus online statistics:
//!
//! * VWAP over 15s and 60s windows
//! * EMA(5) and EMA(20) of trade prices
//! * Raw momentum signal = `sign(EMA5 - EMA20)`
//!
//! Readers pull a stack-allocated [`TapeSnapshot`] — no allocations on the
//! read path. Writers are the individual WS tasks.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use tracing::warn;

/// A single BTC trade print.
#[derive(Clone, Copy, Debug)]
pub struct BtcTick {
    /// Trade price in USD (f64 is fine here — reference feed only).
    pub price: f64,
    pub size: f64,
    pub ts: Instant,
    pub source: Source,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Binance,
    Coinbase,
    Kraken,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Binance => "binance",
            Source::Coinbase => "coinbase",
            Source::Kraken => "kraken",
        }
    }
}

/// Immutable copy-on-read snapshot of the tape statistics.
#[derive(Clone, Copy, Debug, Default)]
pub struct TapeSnapshot {
    pub last_price: f64,
    pub vwap_15s: f64,
    pub vwap_60s: f64,
    pub ema_fast: f64,
    pub ema_slow: f64,
    /// Normalised momentum in [-1.0, 1.0]. Positive = bullish.
    pub momentum: f64,
    pub trade_count: u64,
    pub last_source: Option<Source>,
}

impl TapeSnapshot {
    /// "Signal probability" — a rough directional probability that BTC closes
    /// above its current level, derived from momentum. We deliberately keep
    /// this cheap so the hot path can evaluate it in a few nanoseconds.
    pub fn directional_prob(&self) -> f64 {
        // Compress momentum ∈ [-1,1] → prob ∈ [~0.3, ~0.7] via linear mix
        // around the neutral 0.5. Higher-quality estimates live in the
        // `signal` crate; this is just a convenience for the dashboard.
        0.5 + 0.2 * self.momentum.clamp(-1.0, 1.0)
    }
}

/// Tape statistics + ring buffer of recent trades.
#[derive(Debug)]
pub struct BtcTape {
    inner: RwLock<TapeInner>,
}

#[derive(Debug)]
struct TapeInner {
    // Keep a bounded ring so VWAP(60s) is cheap to recompute on each tick.
    recent: VecDeque<BtcTick>,
    ema_fast: f64,
    ema_slow: f64,
    snap: TapeSnapshot,
}

impl TapeInner {
    fn new() -> Self {
        Self {
            recent: VecDeque::with_capacity(4096),
            ema_fast: 0.0,
            ema_slow: 0.0,
            snap: TapeSnapshot::default(),
        }
    }
}

impl BtcTape {
    const EMA_FAST_ALPHA: f64 = 2.0 / (5.0 + 1.0);
    const EMA_SLOW_ALPHA: f64 = 2.0 / (20.0 + 1.0);
    const WINDOW_60S: Duration = Duration::from_secs(60);

    pub fn new() -> Self {
        Self {
            inner: RwLock::new(TapeInner::new()),
        }
    }

    /// Insert a trade print. Hot path for the BTC WS tasks.
    pub fn record(&self, tick: BtcTick) {
        if !tick.price.is_finite() || tick.price <= 0.0 {
            warn!(price = tick.price, "ignoring non-finite BTC tick");
            return;
        }
        let mut inner = self.inner.write();

        // Seed EMAs on first tick so they don't start at zero.
        if inner.snap.trade_count == 0 {
            inner.ema_fast = tick.price;
            inner.ema_slow = tick.price;
        } else {
            inner.ema_fast =
                Self::EMA_FAST_ALPHA * tick.price + (1.0 - Self::EMA_FAST_ALPHA) * inner.ema_fast;
            inner.ema_slow =
                Self::EMA_SLOW_ALPHA * tick.price + (1.0 - Self::EMA_SLOW_ALPHA) * inner.ema_slow;
        }

        inner.recent.push_back(tick);
        // Evict trades older than 60s. We loop because ring is FIFO by ts.
        while let Some(front) = inner.recent.front().copied() {
            if tick.ts.duration_since(front.ts) > Self::WINDOW_60S {
                inner.recent.pop_front();
            } else {
                break;
            }
        }
        if inner.recent.len() > 4000 {
            inner.recent.pop_front();
        }

        // Recompute VWAPs
        let cutoff_15 = tick.ts - Duration::from_secs(15);
        let (mut n15, mut d15) = (0.0_f64, 0.0_f64);
        let (mut n60, mut d60) = (0.0_f64, 0.0_f64);
        for t in inner.recent.iter() {
            n60 += t.price * t.size;
            d60 += t.size;
            if t.ts >= cutoff_15 {
                n15 += t.price * t.size;
                d15 += t.size;
            }
        }
        let vwap_15 = if d15 > 0.0 { n15 / d15 } else { tick.price };
        let vwap_60 = if d60 > 0.0 { n60 / d60 } else { tick.price };

        let delta = inner.ema_fast - inner.ema_slow;
        // Normalise to ±1 using slow EMA as scale. A 50bps divergence → ~1.0.
        let momentum = (delta / (inner.ema_slow * 0.005)).clamp(-1.0, 1.0);

        inner.snap = TapeSnapshot {
            last_price: tick.price,
            vwap_15s: vwap_15,
            vwap_60s: vwap_60,
            ema_fast: inner.ema_fast,
            ema_slow: inner.ema_slow,
            momentum,
            trade_count: inner.snap.trade_count + 1,
            last_source: Some(tick.source),
        };
    }

    pub fn snapshot(&self) -> TapeSnapshot {
        self.inner.read().snap
    }
}

impl Default for BtcTape {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(price: f64, size: f64, offset_ms: u64) -> BtcTick {
        BtcTick {
            price,
            size,
            ts: Instant::now() + Duration::from_millis(offset_ms),
            source: Source::Binance,
        }
    }

    #[test]
    fn emas_converge_and_momentum_is_bounded() {
        let tape = BtcTape::new();
        for i in 0..100 {
            tape.record(tick(70_000.0 + i as f64, 1.0, i * 10));
        }
        let snap = tape.snapshot();
        assert!(snap.ema_fast > snap.ema_slow); // rising
        assert!(snap.momentum >= 0.0 && snap.momentum <= 1.0);
        assert_eq!(snap.last_price, 70_099.0);
    }

    #[test]
    fn vwap_respects_size_weighting() {
        let tape = BtcTape::new();
        tape.record(tick(70_000.0, 1.0, 0));
        tape.record(tick(71_000.0, 3.0, 100));
        let snap = tape.snapshot();
        // VWAP = (70000*1 + 71000*3) / 4 = 70750
        assert!((snap.vwap_60s - 70_750.0).abs() < 0.5);
    }

    #[test]
    fn directional_prob_in_range() {
        let tape = BtcTape::new();
        tape.record(tick(70_000.0, 1.0, 0));
        let p = tape.snapshot().directional_prob();
        assert!((0.0..=1.0).contains(&p));
    }
}
