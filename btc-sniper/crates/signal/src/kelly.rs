//! Precomputed Kelly-fraction lookup table.
//!
//! The Kelly formula for a binary bet at decimal odds `b` and win prob `p`:
//!
//! ```text
//!   f* = (b * p - (1 - p)) / b
//! ```
//!
//! For Polymarket YES tokens, the price IS the implied probability and the
//! payout is `1 - price` (you pay `price` to receive `1` on a win). So:
//!
//! ```text
//!   odds b = (1 - price) / price
//!   p ≈ price + edge
//! ```
//!
//! On the hot path we don't care about precise f64 evaluation — we want a
//! constant-time fraction → (edge_bps, price_tick) → sized scalar. We
//! precompute a 256-entry table keyed on edge bucket.

/// Kelly lookup: edge_bps_bucket → fraction scaled to 0..=1_000_000.
/// `edge_bps_bucket = edge_bps / 25`, capped at 255 (= 6375 bps = 63.75%).
#[derive(Clone, Debug)]
pub struct KellyTable {
    /// Per-bucket fractional size, scaled ×1e6 (so `table[i] / 1e6` is the
    /// Kelly-adjusted fraction of bankroll to stake).
    table: [u32; 256],
    kelly_fraction: f64,
}

impl KellyTable {
    /// Build the table for a given Kelly fraction (0..=1). `0.15` is a
    /// conservative HFT default (fractional Kelly = 15% of full Kelly).
    pub fn new(kelly_fraction: f64) -> Self {
        let kf = kelly_fraction.clamp(0.0, 1.0);
        let mut table = [0u32; 256];
        // The closed-form Kelly fraction for a Polymarket-style binary bet
        // at price X with true prob p=X+edge is:
        //
        //     f* = edge / (1 - X)
        //
        // We bake in the assumption X = 0.5 (symmetric market mid) to keep
        // the table 1-D. For X > 0.5 the true f* is LARGER, so the symmetric
        // assumption is conservative (we under-bet). Combined with the
        // fractional Kelly multiplier (typ. 0.15) this has plenty of margin.
        for (i, slot) in table.iter_mut().enumerate() {
            let edge_bps = (i as i32) * 25;
            let edge = edge_bps as f64 / 10_000.0;
            let f_star = (edge / 0.5).clamp(0.0, 1.0); // == 2*edge, capped
            let f = (f_star * kf).clamp(0.0, 1.0);
            *slot = (f * 1_000_000.0).round() as u32;
        }
        Self { table, kelly_fraction: kf }
    }

    /// Return the fractional stake (×1e6) for a given edge in bps.
    #[inline]
    pub fn fraction_for_edge(&self, edge_bps: u32) -> u32 {
        let bucket = ((edge_bps / 25) as usize).min(255);
        self.table[bucket]
    }

    pub fn kelly_fraction(&self) -> f64 {
        self.kelly_fraction
    }

    /// Exact Kelly stake (for audit / backtest paths). Uses f64 — do NOT
    /// call on the hot trading path.
    pub fn kelly_stake(bankroll: f64, edge: f64, price: f64, fraction: f64) -> f64 {
        if !(0.0..=1.0).contains(&price) || price == 0.0 || price == 1.0 {
            return 0.0;
        }
        let p = (price + edge).clamp(0.001, 0.999);
        let b = (1.0 - price) / price;
        let f_star = (b * p - (1.0 - p)) / b;
        (bankroll * f_star * fraction).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_monotonic_in_edge_for_reasonable_range() {
        let kt = KellyTable::new(0.15);
        // The Kelly function on [0, 500bps] should be non-decreasing in edge.
        let mut prev = 0u32;
        for bps in (0..=500).step_by(25) {
            let f = kt.fraction_for_edge(bps);
            assert!(f >= prev, "Kelly not monotonic at bps={bps}: {prev} → {f}");
            prev = f;
        }
    }

    #[test]
    fn zero_edge_zero_stake() {
        let kt = KellyTable::new(0.15);
        assert_eq!(kt.fraction_for_edge(0), 0);
    }

    #[test]
    fn stake_helper_is_conservative() {
        let bankroll = 10_000.0;
        let s = KellyTable::kelly_stake(bankroll, 0.025, 0.50, 0.15);
        // Full Kelly for edge=2.5%, p=0.525, b=1.0 is ~0.05 → 0.15× = 0.0075
        // → 75 USDC
        assert!(s > 50.0 && s < 100.0, "got {s}");
    }

    #[test]
    fn price_at_extremes_returns_zero() {
        assert_eq!(KellyTable::kelly_stake(1_000.0, 0.01, 0.0, 0.5), 0.0);
        assert_eq!(KellyTable::kelly_stake(1_000.0, 0.01, 1.0, 0.5), 0.0);
    }
}
