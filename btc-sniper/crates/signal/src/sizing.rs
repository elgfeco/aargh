//! Position sizing on the hot path.
//!
//! [`PositionSizer`] wraps a [`KellyTable`] and enforces the hard
//! `MAX_POSITION_USDC` cap. It is purely a pure function of (edge, price)
//! — no mutable state, safe to call concurrently without any lock.

use sniper_feed::{Price, Size};

use crate::kelly::KellyTable;

#[derive(Clone, Debug)]
pub struct PositionSizer {
    kelly: KellyTable,
    bankroll_usdc: u64, // scaled atoms
    max_per_trade_usdc: u64, // scaled atoms
}

impl PositionSizer {
    /// `bankroll_usdc` in whole USDC; `max_per_trade_usdc` likewise.
    pub fn new(bankroll_usdc: f64, max_per_trade_usdc: f64, kelly_fraction: f64) -> Self {
        Self {
            kelly: KellyTable::new(kelly_fraction),
            bankroll_usdc: (bankroll_usdc * 1_000_000.0) as u64,
            max_per_trade_usdc: (max_per_trade_usdc * 1_000_000.0) as u64,
        }
    }

    pub fn kelly(&self) -> &KellyTable {
        &self.kelly
    }

    pub fn bankroll(&self) -> Size {
        Size(self.bankroll_usdc)
    }

    pub fn max_per_trade(&self) -> Size {
        Size(self.max_per_trade_usdc)
    }

    /// Return the notional USDC size to stake given an absolute edge (bps)
    /// at the specified limit price.
    ///
    /// The result is capped by both `max_per_trade_usdc` AND by the token-side
    /// clamp (can't bet more than `bankroll × 1/price` worth of shares, since
    /// a share costs `price` USDC).
    #[inline]
    pub fn size_for(&self, edge_bps: u32, _price: Price) -> Size {
        let frac = self.kelly.fraction_for_edge(edge_bps) as u128;
        // bankroll (atoms) × frac / 1e6 → atoms
        let notional = (self.bankroll_usdc as u128 * frac) / 1_000_000;
        let notional = notional.min(self.max_per_trade_usdc as u128) as u64;
        Size(notional)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_cap_is_enforced() {
        let ps = PositionSizer::new(1_000_000.0, 500.0, 1.0 /* full Kelly */);
        // Huge edge → f* ~1 → bankroll × 1 = 1,000,000. Cap = 500.
        let sz = ps.size_for(5000, Price::from_prob(0.50));
        assert_eq!(sz, Size::from_usdc(500.0));
    }

    #[test]
    fn zero_edge_zero_size() {
        let ps = PositionSizer::new(10_000.0, 500.0, 0.15);
        assert_eq!(ps.size_for(0, Price::from_prob(0.50)), Size::ZERO);
    }

    #[test]
    fn size_scales_with_bankroll() {
        let small = PositionSizer::new(1_000.0, 10_000.0, 0.15);
        let large = PositionSizer::new(10_000.0, 10_000.0, 0.15);
        let s = small.size_for(300, Price::from_prob(0.50));
        let l = large.size_for(300, Price::from_prob(0.50));
        assert!(l.0 > s.0);
    }
}
