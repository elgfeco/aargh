//! Market-making quote engine.
//!
//! Instead of sniping stale quotes (taker), this module continuously posts
//! resting limit orders on both sides of the Polymarket book. Profit comes
//! from capturing the bid-ask spread while paying **zero fees** (maker) and
//! earning the 20% taker-fee rebate.
//!
//! ## Strategy
//!
//! 1. Compute a **fair value** from BTC momentum (same signal as the sniper).
//! 2. Post a bid at `fair - half_spread` and an ask at `fair + half_spread`.
//! 3. **Skew** the quotes based on inventory: if we're long (filled on bids),
//!    lower the bid and raise the ask to encourage sells.
//! 4. On a significant BTC move, **cancel and re-quote** immediately.
//! 5. Profit = spread captured − adverse selection losses.
//!
//! ## Why maker > taker on Polymarket
//!
//! | | Taker | Maker |
//! |---|---|---|
//! | Fee | 3.6% at mid (feeRate=0.072) | 0% + 20% rebate |
//! | Edge needed | >5% to profit | spread alone |
//! | Volume | sparse (big edges only) | continuous |

use sniper_feed::{BookSnapshot, Price, Size, TapeSnapshot};

/// Parameters for the market maker. Loaded from env once.
#[derive(Clone, Copy, Debug)]
pub struct MakerParams {
    /// Half-spread in basis points. Each side is offset by this amount from
    /// fair value. e.g. 150 = 1.5% each side → 3% round-trip spread.
    pub half_spread_bps: i32,

    /// Size per side in USDC atoms (6 decimals).
    pub quote_size_atoms: u64,

    /// BTC momentum shift (bps) that triggers a cancel + re-quote.
    /// Smaller = more responsive but more cancels.
    pub requote_threshold_bps: i32,

    /// Maximum net inventory (USDC atoms) before we go one-sided.
    /// Once hit, we only quote the side that reduces inventory.
    pub max_inventory_atoms: u64,

    /// Inventory skew factor. For each unit of inventory, shift quotes by
    /// this many bps to incentivize flattening. e.g. 50 = 0.5% per $100 inv.
    pub inventory_skew_bps_per_100usdc: i32,

    /// Maximum conviction from BTC momentum (bps). Same as EdgeParams.
    pub max_conviction_bps: i32,
}

impl Default for MakerParams {
    fn default() -> Self {
        Self {
            half_spread_bps: 150,
            quote_size_atoms: 100_000_000, // $100
            requote_threshold_bps: 50,
            max_inventory_atoms: 500_000_000, // $500
            inventory_skew_bps_per_100usdc: 30,
            max_conviction_bps: 1000,
        }
    }
}

/// A two-sided quote to post on the book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
    pub bid: Option<QuoteLeg>,
    pub ask: Option<QuoteLeg>,
    pub fair_price: Price,
    pub skew_bps: i32,
}

/// One side of a quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuoteLeg {
    pub price: Price,
    pub size: Size,
}

/// What happened since our last quote — do we need to act?
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuoteAction {
    /// Keep existing quotes, no change needed.
    Hold,
    /// Cancel existing quotes and post new ones.
    Requote(Quote),
    /// Cancel all quotes (no valid fair value or book is empty).
    CancelAll,
}

/// Stateless quote engine. Clone freely — it's just params.
#[derive(Clone, Debug)]
pub struct MarketMaker {
    params: MakerParams,
}

impl MarketMaker {
    pub fn new(params: MakerParams) -> Self {
        Self { params }
    }

    pub fn params(&self) -> &MakerParams {
        &self.params
    }

    /// Compute the ideal two-sided quote for one asset.
    ///
    /// # Arguments
    /// * `book` — current order book snapshot
    /// * `tape` — BTC price/momentum snapshot
    /// * `inventory_net_atoms` — signed net position (positive = long YES)
    /// * `prev_fair` — previous fair value (for requote threshold)
    ///
    /// Returns a `QuoteAction` telling the caller what to do.
    pub fn evaluate(
        &self,
        book: &BookSnapshot,
        tape: &TapeSnapshot,
        inventory_net_atoms: i64,
        prev_fair: Option<Price>,
    ) -> QuoteAction {
        // Need a mid price to anchor our fair value
        let poly_mid = match book.mid() {
            Some(p) => p,
            None => return QuoteAction::CancelAll,
        };

        // Need BTC data flowing
        if tape.trade_count == 0 {
            return QuoteAction::CancelAll;
        }

        // --- Compute fair value from BTC momentum ---
        let shift_bps = (tape.momentum * self.params.max_conviction_bps as f64) as i32;
        let shift_bps = shift_bps.clamp(
            -self.params.max_conviction_bps,
            self.params.max_conviction_bps,
        );

        // Fair value = poly mid shifted by our BTC signal
        let fair_prob = (poly_mid.as_prob() + shift_bps as f64 / 10_000.0)
            .clamp(0.01, 0.99);
        let fair = Price::from_prob(fair_prob);

        // --- Check if we need to requote ---
        if let Some(prev) = prev_fair {
            let delta_bps = ((fair.as_prob() - prev.as_prob()) * 10_000.0).abs() as i32;
            if delta_bps < self.params.requote_threshold_bps {
                return QuoteAction::Hold;
            }
        }

        // --- Inventory skew ---
        // Positive inventory (long) → lower bid, raise ask (encourage selling)
        // Negative inventory (short) → raise bid, lower ask (encourage buying)
        let inv_100usdc = inventory_net_atoms as f64 / 100_000_000.0;
        let skew_bps = (inv_100usdc * self.params.inventory_skew_bps_per_100usdc as f64) as i32;

        // --- Compute bid/ask prices ---
        let bid_offset_bps = self.params.half_spread_bps + skew_bps;
        let ask_offset_bps = self.params.half_spread_bps - skew_bps;

        let bid_prob = fair_prob - bid_offset_bps as f64 / 10_000.0;
        let ask_prob = fair_prob + ask_offset_bps as f64 / 10_000.0;

        let size = Size(self.params.quote_size_atoms);
        let inv_abs = inventory_net_atoms.unsigned_abs();

        // --- Build quote legs ---
        // Don't quote the side that would increase inventory beyond max
        let bid = if inv_abs < self.params.max_inventory_atoms || inventory_net_atoms < 0 {
            if bid_prob > 0.01 && bid_prob < 0.99 {
                Some(QuoteLeg {
                    price: Price::from_prob(bid_prob),
                    size,
                })
            } else {
                None
            }
        } else {
            None // too long, don't buy more
        };

        let ask = if inv_abs < self.params.max_inventory_atoms || inventory_net_atoms > 0 {
            if ask_prob > 0.01 && ask_prob < 0.99 {
                Some(QuoteLeg {
                    price: Price::from_prob(ask_prob),
                    size,
                })
            } else {
                None
            }
        } else {
            None // too short, don't sell more
        };

        // If neither side is valid, cancel everything
        if bid.is_none() && ask.is_none() {
            return QuoteAction::CancelAll;
        }

        QuoteAction::Requote(Quote {
            bid,
            ask,
            fair_price: fair,
            skew_bps,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sniper_feed::PriceLevel;

    fn mk_book(bid: f64, ask: f64) -> BookSnapshot {
        BookSnapshot {
            best_bid: Some(PriceLevel {
                price: Price::from_prob(bid),
                size: Size::from_usdc(1000.0),
            }),
            best_ask: Some(PriceLevel {
                price: Price::from_prob(ask),
                size: Size::from_usdc(1000.0),
            }),
            bid_depth: 5,
            ask_depth: 5,
            seq: 1,
        }
    }

    fn tape(momentum: f64) -> TapeSnapshot {
        TapeSnapshot {
            last_price: 70_000.0,
            vwap_15s: 70_000.0,
            vwap_60s: 70_000.0,
            ema_fast: 70_000.0,
            ema_slow: 70_000.0,
            momentum,
            trade_count: 100,
            last_source: None,
        }
    }

    fn maker(half_spread: i32) -> MarketMaker {
        MarketMaker::new(MakerParams {
            half_spread_bps: half_spread,
            quote_size_atoms: 100_000_000,
            requote_threshold_bps: 50,
            max_inventory_atoms: 500_000_000,
            inventory_skew_bps_per_100usdc: 30,
            max_conviction_bps: 1000,
        })
    }

    #[test]
    fn quotes_both_sides_at_neutral() {
        let mm = maker(150);
        let book = mk_book(0.49, 0.51);
        let t = tape(0.0); // neutral
        match mm.evaluate(&book, &t, 0, None) {
            QuoteAction::Requote(q) => {
                assert!(q.bid.is_some());
                assert!(q.ask.is_some());
                let bid = q.bid.unwrap();
                let ask = q.ask.unwrap();
                // Bid should be below fair, ask above
                assert!(bid.price.as_prob() < q.fair_price.as_prob());
                assert!(ask.price.as_prob() > q.fair_price.as_prob());
                // Spread should be ~2 × half_spread
                let spread = ask.price.as_prob() - bid.price.as_prob();
                assert!((spread - 0.03).abs() < 0.005); // ~3% spread
            }
            other => panic!("expected Requote, got {:?}", other),
        }
    }

    #[test]
    fn skews_when_long() {
        let mm = maker(150);
        let book = mk_book(0.49, 0.51);
        let t = tape(0.0);
        // Long $200 inventory
        match mm.evaluate(&book, &t, 200_000_000, None) {
            QuoteAction::Requote(q) => {
                let bid = q.bid.unwrap();
                let ask = q.ask.unwrap();
                // When long, bid should be lower (less eager to buy)
                // and ask should be closer to fair (eager to sell)
                let bid_offset = q.fair_price.as_prob() - bid.price.as_prob();
                let ask_offset = ask.price.as_prob() - q.fair_price.as_prob();
                assert!(bid_offset > ask_offset, "bid should be wider when long");
            }
            other => panic!("expected Requote, got {:?}", other),
        }
    }

    #[test]
    fn cancels_when_inventory_maxed() {
        let mm = maker(150);
        let book = mk_book(0.49, 0.51);
        let t = tape(0.0);
        // Maxed out long inventory
        match mm.evaluate(&book, &t, 500_000_000, None) {
            QuoteAction::Requote(q) => {
                // Should only quote the ask (to sell down inventory)
                assert!(q.bid.is_none(), "should not bid when max long");
                assert!(q.ask.is_some(), "should still offer to sell");
            }
            other => panic!("expected Requote, got {:?}", other),
        }
    }

    #[test]
    fn holds_when_fair_unchanged() {
        let mm = maker(150);
        let book = mk_book(0.49, 0.51);
        let t = tape(0.0);
        // First quote
        let prev_fair = Price::from_prob(0.50);
        // Fair hasn't moved beyond threshold
        let action = mm.evaluate(&book, &t, 0, Some(prev_fair));
        assert_eq!(action, QuoteAction::Hold);
    }

    #[test]
    fn requotes_on_momentum_shift() {
        let mm = maker(150);
        let book = mk_book(0.49, 0.51);
        let t = tape(1.0); // strong bullish momentum
        let prev_fair = Price::from_prob(0.50);
        // Momentum shifts fair value → should requote
        match mm.evaluate(&book, &t, 0, Some(prev_fair)) {
            QuoteAction::Requote(q) => {
                // Fair value should be shifted up
                assert!(q.fair_price.as_prob() > 0.50);
            }
            other => panic!("expected Requote, got {:?}", other),
        }
    }

    #[test]
    fn cancel_all_when_no_btc_data() {
        let mm = maker(150);
        let book = mk_book(0.49, 0.51);
        let t = TapeSnapshot::default(); // no trades
        let action = mm.evaluate(&book, &t, 0, None);
        assert_eq!(action, QuoteAction::CancelAll);
    }

    #[test]
    fn cancel_all_when_book_empty() {
        let mm = maker(150);
        let book = BookSnapshot::default();
        let t = tape(0.0);
        let action = mm.evaluate(&book, &t, 0, None);
        assert_eq!(action, QuoteAction::CancelAll);
    }
}
