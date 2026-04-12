//! Core scalar types shared by the feed, signal, and executor crates.
//!
//! We use **scaled integer arithmetic** on the hot path: prices are stored as
//! `u64` basis-point-like ticks rather than `f64`. On Polymarket, token prices
//! range over `[0, 1]` (probability), so we use
//!   `tick = round(price * 1_000_000)`.
//! Sizes are quoted in integer USDC atoms (6 decimals).

use std::fmt;

/// Polymarket *condition id* — identifies a market (YES/NO pair).
pub type MarketId = [u8; 32];

/// Polymarket *asset id* (a.k.a. token id) — identifies one side (YES or NO).
pub type AssetId = [u8; 32];

/// Scaled price tick. 1 tick = 1e-6 of probability. Range: [0, 1_000_000].
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
#[repr(transparent)]
pub struct Price(pub u32);

impl Price {
    pub const MIN: Price = Price(0);
    pub const MAX: Price = Price(1_000_000);

    /// Construct from a 0..=1 probability. Panics in debug on out-of-range;
    /// saturates in release.
    #[inline]
    pub fn from_prob(p: f64) -> Self {
        debug_assert!((0.0..=1.0).contains(&p), "prob out of range: {p}");
        let v = (p * 1_000_000.0).round();
        if v < 0.0 {
            Price(0)
        } else if v > 1_000_000.0 {
            Price(1_000_000)
        } else {
            Price(v as u32)
        }
    }

    #[inline]
    pub fn as_prob(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    /// Edge in basis points vs another price: `self - other` in 1/10_000.
    #[inline]
    pub fn edge_bps(self, other: Price) -> i32 {
        let delta = self.0 as i64 - other.0 as i64; // scaled 1e-6
        // 1 bps = 1/10_000 = 100 ticks. Return signed bps.
        (delta / 100) as i32
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.4}", self.as_prob())
    }
}

/// Scaled size — USDC atoms (6 decimals).
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash, Default)]
#[repr(transparent)]
pub struct Size(pub u64);

impl Size {
    pub const ZERO: Size = Size(0);

    #[inline]
    pub fn from_usdc(v: f64) -> Self {
        Size((v * 1_000_000.0).round().max(0.0) as u64)
    }

    #[inline]
    pub fn as_usdc(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    #[inline]
    pub fn checked_add(self, rhs: Size) -> Option<Size> {
        self.0.checked_add(rhs.0).map(Size)
    }

    #[inline]
    pub fn saturating_sub(self, rhs: Size) -> Size {
        Size(self.0.saturating_sub(rhs.0))
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.2}", self.as_usdc())
    }
}

/// Order side.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Side {
    /// Buy — places a bid into the book.
    Buy,
    /// Sell — places an ask into the book.
    Sell,
}

impl Side {
    #[inline]
    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    #[inline]
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        }
    }
}

/// Parse a 0x-prefixed or bare hex id into a 32-byte array. Returns `None` on
/// malformed input — callers on the config path can log a friendly error.
pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

#[inline]
fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_roundtrip() {
        let p = Price::from_prob(0.4567);
        assert_eq!(p.0, 456_700);
        assert!((p.as_prob() - 0.4567).abs() < 1e-9);
    }

    #[test]
    fn edge_bps_computes_signed_delta() {
        let a = Price::from_prob(0.55);
        let b = Price::from_prob(0.52);
        // 0.55 - 0.52 = 0.03 = 300 bps
        assert_eq!(a.edge_bps(b), 300);
        assert_eq!(b.edge_bps(a), -300);
    }

    #[test]
    fn size_arithmetic() {
        let a = Size::from_usdc(100.0);
        let b = Size::from_usdc(25.5);
        assert_eq!(a.checked_add(b).unwrap().as_usdc(), 125.5);
        assert_eq!(a.saturating_sub(b).as_usdc(), 74.5);
    }

    #[test]
    fn parse_hex32_ok() {
        let h = "0x".to_owned() + &"ab".repeat(32);
        let bytes = parse_hex32(&h).unwrap();
        assert_eq!(bytes, [0xab; 32]);
    }

    #[test]
    fn parse_hex32_rejects_bad_len() {
        assert!(parse_hex32("0xabc").is_none());
    }
}
