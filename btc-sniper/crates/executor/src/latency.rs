//! Latency histograms for each stage of the execution pipeline.
//!
//! We use `hdrhistogram` because it gives constant-time `record()` and
//! accurate percentile reads, even under high sample rates. Each stage is a
//! separate histogram so we can isolate slowdowns.
//!
//! The snapshot is printed to stdout on SIGUSR1 — see `main.rs` for the
//! signal wiring.

use std::fmt;

use hdrhistogram::Histogram;
use parking_lot::Mutex;

/// Pipeline stages we measure. Ordered from earliest to latest.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum LatencyStage {
    /// WS frame received → parsed into a PolymarketMessage
    WsParse,
    /// Parsed message → edge engine produced a Decision
    SignalEval,
    /// Decision → HTTP request flushed onto the wire
    OrderFired,
    /// HTTP request flushed → 2xx response / fill confirmation
    OrderAcked,
}

impl LatencyStage {
    pub fn as_str(self) -> &'static str {
        match self {
            LatencyStage::WsParse => "ws_parse",
            LatencyStage::SignalEval => "signal_eval",
            LatencyStage::OrderFired => "order_fired",
            LatencyStage::OrderAcked => "order_acked",
        }
    }
}

/// One histogram per stage. `record_ns` accepts nanosecond deltas — the
/// histogram auto-resizes up to 60 seconds.
pub struct LatencyStats {
    ws_parse: Mutex<Histogram<u64>>,
    signal_eval: Mutex<Histogram<u64>>,
    order_fired: Mutex<Histogram<u64>>,
    order_acked: Mutex<Histogram<u64>>,
}

impl LatencyStats {
    pub fn new() -> Self {
        // 1ns → 60s, 3 significant digits. ~4 KiB per histogram.
        let h = || Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3).unwrap();
        Self {
            ws_parse: Mutex::new(h()),
            signal_eval: Mutex::new(h()),
            order_fired: Mutex::new(h()),
            order_acked: Mutex::new(h()),
        }
    }

    #[inline]
    pub fn record(&self, stage: LatencyStage, ns: u64) {
        let h = match stage {
            LatencyStage::WsParse => &self.ws_parse,
            LatencyStage::SignalEval => &self.signal_eval,
            LatencyStage::OrderFired => &self.order_fired,
            LatencyStage::OrderAcked => &self.order_acked,
        };
        let _ = h.lock().record(ns);
    }

    /// Snapshot percentiles for all four stages. Safe to call from any
    /// thread; takes the locks one at a time to avoid deadlock.
    pub fn snapshot(&self) -> LatencySnapshot {
        LatencySnapshot {
            ws_parse: percentile_row(&self.ws_parse.lock()),
            signal_eval: percentile_row(&self.signal_eval.lock()),
            order_fired: percentile_row(&self.order_fired.lock()),
            order_acked: percentile_row(&self.order_acked.lock()),
        }
    }
}

impl Default for LatencyStats {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LatencyRow {
    pub count: u64,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub p999_ns: u64,
    pub max_ns: u64,
}

fn percentile_row(h: &Histogram<u64>) -> LatencyRow {
    LatencyRow {
        count: h.len(),
        p50_ns: h.value_at_quantile(0.50),
        p95_ns: h.value_at_quantile(0.95),
        p99_ns: h.value_at_quantile(0.99),
        p999_ns: h.value_at_quantile(0.999),
        max_ns: h.max(),
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LatencySnapshot {
    pub ws_parse: LatencyRow,
    pub signal_eval: LatencyRow,
    pub order_fired: LatencyRow,
    pub order_acked: LatencyRow,
}

impl fmt::Display for LatencySnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "stage         count     p50     p95     p99    p999     max")?;
        let rows = [
            ("ws_parse", self.ws_parse),
            ("signal_eval", self.signal_eval),
            ("order_fired", self.order_fired),
            ("order_acked", self.order_acked),
        ];
        for (name, r) in rows {
            writeln!(
                f,
                "{:12} {:7} {:5}ns {:5}ns {:5}ns {:5}ns {:5}ns",
                name, r.count, r.p50_ns, r.p95_ns, r.p99_ns, r.p999_ns, r.max_ns
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_reads_percentiles() {
        let s = LatencyStats::new();
        for ns in (100..10_000u64).step_by(50) {
            s.record(LatencyStage::SignalEval, ns);
        }
        let snap = s.snapshot();
        assert_eq!(snap.signal_eval.count, (10_000u64 - 100) / 50);
        assert!(snap.signal_eval.p50_ns > 0);
        assert!(snap.signal_eval.p99_ns >= snap.signal_eval.p50_ns);
    }
}
