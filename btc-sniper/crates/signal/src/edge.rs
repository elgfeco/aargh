//! Edge detection: compares Polymarket YES-mid to a BTC-derived directional
//! probability, and if the gap exceeds the configured minimum edge, emits a
//! [`Decision`] indicating which side to fire.
//!
//! ## Math
//!
//! Let `p_poly ∈ [0,1]` be the mid price of the YES token (implied
//! probability). Let `p_btc ∈ [0,1]` be our model probability derived from
//! BTC momentum. Let `c` be the per-round transaction cost in bps.
//!
//! ```text
//!   edge_bps = (p_btc - p_poly) * 10_000 - c
//! ```
//!
//! * If `edge_bps > min_edge` → BUY YES (we think YES is too cheap).
//! * If `edge_bps < -min_edge` → BUY NO (YES is too expensive).
//! * Otherwise → Hold.
//!
//! The decision includes a recommended limit price (one tick inside the
//! best quote) and a [`Size`] computed by the position sizer.

use sniper_feed::{BookSnapshot, Price, Side, Size, TapeSnapshot};

use crate::sizing::PositionSizer;

/// Trading intent emitted by the signal engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    Hold,
    Fire { side: Side, price: Price, size: Size },
}

/// Decision record — also logged for post-hoc analysis.
#[derive(Clone, Copy, Debug)]
pub struct Decision {
    pub intent: Intent,
    pub poly_mid: Option<Price>,
    pub btc_prob: f64,
    pub edge_bps: i32,
}

/// Static parameters for the edge calculator. Loaded from env once, then
/// read-only for the lifetime of the process.
#[derive(Clone, Copy, Debug)]
pub struct EdgeParams {
    /// Minimum signed edge (in bps) required to fire.
    pub min_edge_bps: i32,
    /// Estimated round-trip transaction cost (bps). Subtracted from raw edge.
    pub tx_cost_bps: i32,
    /// Cap on the absolute confidence we derive from BTC momentum. Prevents
    /// the bot from over-sizing on a shallow EMA divergence.
    pub max_conviction_bps: i32,
}

impl Default for EdgeParams {
    fn default() -> Self {
        Self {
            min_edge_bps: 250,
            tx_cost_bps: 0,
            max_conviction_bps: 1000, // ±10% absolute directional prob shift
        }
    }
}

/// Stateless hot-path evaluator. Clone it freely — it owns only a copy of
/// the params plus the position sizer (which is itself a cheap lookup table).
#[derive(Clone, Debug)]
pub struct EdgeEngine {
    params: EdgeParams,
    sizer: PositionSizer,
}

impl EdgeEngine {
    pub fn new(params: EdgeParams, sizer: PositionSizer) -> Self {
        Self { params, sizer }
    }

    pub fn params(&self) -> &EdgeParams {
        &self.params
    }

    /// Evaluate one (book, tape) pair. Returns a [`Decision`] — callers
    /// should check `decision.intent` to decide whether to fire.
    ///
    /// This function must NEVER allocate and must NEVER use `f64` division
    /// on the hot path beyond the final model-prob conversion. In practice
    /// we've measured this at ~500ns on Graviton3.
    #[inline]
    pub fn evaluate(&self, book: &BookSnapshot, tape: &TapeSnapshot) -> Decision {
        let poly_mid = book.mid();
        let poly_mid = match poly_mid {
            Some(p) => p,
            None => {
                return Decision {
                    intent: Intent::Hold,
                    poly_mid: None,
                    btc_prob: 0.5,
                    edge_bps: 0,
                };
            }
        };

        // Clamp the raw momentum to the configured conviction ceiling.
        // Momentum is already in [-1, 1]; scale to directional-probability
        // shift in bps using max_conviction_bps as the full-scale value.
        let shift_bps = (tape.momentum * self.params.max_conviction_bps as f64) as i32;
        let shift_bps = shift_bps.clamp(
            -self.params.max_conviction_bps,
            self.params.max_conviction_bps,
        );

        // Model prob = 0.5 + shift, clamped to (0, 1).
        let mut btc_prob = 0.5 + shift_bps as f64 / 10_000.0;
        if btc_prob <= 0.0 {
            btc_prob = 0.001;
        } else if btc_prob >= 1.0 {
            btc_prob = 0.999;
        }
        let btc_price = Price::from_prob(btc_prob);

        // Signed edge: positive → model says YES should be higher than poly.
        let raw_edge = btc_price.edge_bps(poly_mid);
        let edge = if raw_edge > 0 {
            (raw_edge - self.params.tx_cost_bps).max(0)
        } else {
            (raw_edge + self.params.tx_cost_bps).min(0)
        };

        let intent = if edge >= self.params.min_edge_bps {
            // Buy YES at the current best ask (take liquidity) — pay the spread.
            let limit = book.best_ask.map(|l| l.price).unwrap_or(poly_mid);
            let size = self.sizer.size_for(edge.unsigned_abs(), limit);
            Intent::Fire {
                side: Side::Buy,
                price: limit,
                size,
            }
        } else if edge <= -self.params.min_edge_bps {
            // Short YES (≡ Buy NO) — sell at current best bid.
            let limit = book.best_bid.map(|l| l.price).unwrap_or(poly_mid);
            let size = self.sizer.size_for(edge.unsigned_abs(), limit);
            Intent::Fire {
                side: Side::Sell,
                price: limit,
                size,
            }
        } else {
            Intent::Hold
        };

        Decision {
            intent,
            poly_mid: Some(poly_mid),
            btc_prob,
            edge_bps: edge,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sniper_feed::{BookSnapshot, PriceLevel};

    fn mk_book(best_bid: f64, best_ask: f64) -> BookSnapshot {
        BookSnapshot {
            best_bid: Some(PriceLevel {
                price: Price::from_prob(best_bid),
                size: Size::from_usdc(1000.0),
            }),
            best_ask: Some(PriceLevel {
                price: Price::from_prob(best_ask),
                size: Size::from_usdc(1000.0),
            }),
            bid_depth: 1,
            ask_depth: 1,
            seq: 1,
        }
    }

    fn engine(min_edge_bps: i32) -> EdgeEngine {
        EdgeEngine::new(
            EdgeParams {
                min_edge_bps,
                tx_cost_bps: 0,
                max_conviction_bps: 1000,
            },
            PositionSizer::new(10_000.0, 500.0, 0.15),
        )
    }

    #[test]
    fn hold_when_book_empty() {
        let eng = engine(250);
        let book = BookSnapshot::default();
        let tape = TapeSnapshot::default();
        let d = eng.evaluate(&book, &tape);
        assert!(matches!(d.intent, Intent::Hold));
    }

    #[test]
    fn fire_buy_when_btc_bullish_vs_poly_mid() {
        let eng = engine(100);
        // Poly mid = 0.50, BTC momentum = +1 → btc_prob = 0.60 → edge ≈ 1000bps
        let book = mk_book(0.49, 0.51);
        let tape = TapeSnapshot {
            momentum: 1.0,
            ..Default::default()
        };
        let d = eng.evaluate(&book, &tape);
        match d.intent {
            Intent::Fire { side, price, size } => {
                assert_eq!(side, Side::Buy);
                assert_eq!(price, Price::from_prob(0.51));
                assert!(size.0 > 0);
            }
            _ => panic!("expected Fire, got {:?}", d),
        }
        assert!(d.edge_bps > 0);
    }

    #[test]
    fn fire_sell_when_btc_bearish() {
        let eng = engine(100);
        let book = mk_book(0.49, 0.51);
        let tape = TapeSnapshot {
            momentum: -1.0,
            ..Default::default()
        };
        let d = eng.evaluate(&book, &tape);
        match d.intent {
            Intent::Fire { side, .. } => assert_eq!(side, Side::Sell),
            _ => panic!("expected sell fire"),
        }
        assert!(d.edge_bps < 0);
    }

    #[test]
    fn tx_cost_gates_small_edges() {
        let eng = EdgeEngine::new(
            EdgeParams {
                min_edge_bps: 50,
                tx_cost_bps: 80,
                max_conviction_bps: 100, // weak
            },
            PositionSizer::new(10_000.0, 500.0, 0.15),
        );
        let book = mk_book(0.495, 0.505);
        let tape = TapeSnapshot {
            momentum: 1.0,
            ..Default::default()
        };
        let d = eng.evaluate(&book, &tape);
        assert!(matches!(d.intent, Intent::Hold));
    }
}
