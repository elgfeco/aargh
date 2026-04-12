//! Order types and pre-serialization.
//!
//! Polymarket CLOB orders are EIP-712-typed structs. In production we would
//! sign them via `ethers::signers::LocalWallet::sign_typed_data(...)`. Here
//! we expose the wire shape and a deterministic "pre-template" so the hot
//! path only has to swap `price`, `size` and a fresh `salt` before POSTing.
//!
//! **Signing is intentionally stubbed** for the skeleton: the `sign()`
//! method returns a 65-byte `0x00...` signature. Wire up a real
//! `ethers::signers::LocalWallet` in `client.rs::new()` to go live.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sniper_feed::{AssetId, Price, Side, Size};

static ORDER_SEQ: AtomicU64 = AtomicU64::new(1);

/// Opaque order identifier used by the bot internally (NOT the CLOB id).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct OrderId(pub u64);

impl OrderId {
    pub fn next() -> Self {
        OrderId(ORDER_SEQ.fetch_add(1, Ordering::Relaxed))
    }
}

/// Lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderState {
    Pending,
    Acked,
    PartialFill,
    Filled,
    Cancelled,
    Rejected,
}

/// In-memory representation of an open or completed order.
#[derive(Clone, Debug)]
pub struct Order {
    pub id: OrderId,
    pub asset: AssetId,
    pub side: Side,
    pub price: Price,
    pub size: Size,
    pub filled: Size,
    pub state: OrderState,
    pub submitted_at_nanos: u64,
    pub clob_id: Option<String>,
}

impl Order {
    pub fn new(asset: AssetId, side: Side, price: Price, size: Size) -> Self {
        Self {
            id: OrderId::next(),
            asset,
            side,
            price,
            size,
            filled: Size::ZERO,
            state: OrderState::Pending,
            submitted_at_nanos: now_nanos(),
            clob_id: None,
        }
    }

    pub fn remaining(&self) -> Size {
        self.size.saturating_sub(self.filled)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            OrderState::Filled | OrderState::Cancelled | OrderState::Rejected
        )
    }
}

/// Pre-serialized order template. At startup we build one per asset/side and
/// cache its JSON shape as a `Vec<u8>` with placeholder tokens for
/// `price`/`size`. On the hot path we `mem::replace` those tokens into a
/// scratch buffer and POST — no `serde_json::to_string` allocation.
///
/// NOTE: the current implementation uses `serde_json` for serialization
/// because the `OrderManager::submit` path already holds an async lock for
/// the HTTP call. In a second optimization pass we would use a fully
/// text-templated pre-serialized body and swap fields via byte-range edits.
#[derive(Clone, Debug)]
pub struct OrderTemplate {
    pub asset: AssetId,
    pub side: Side,
    pub maker: String,  // wallet address (0x…)
    pub signer: String, // same as maker for L1 accounts
    pub taker: String,  // 0x0 for open orders
    pub nonce: u64,
    pub expiration_secs: u64,
    pub fee_rate_bps: u32,
}

/// Wire shape of a signed CLOB order. Only used by the executor — we don't
/// need to deserialize it anywhere.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedOrder {
    pub salt: String,
    pub maker: String,
    pub signer: String,
    pub taker: String,
    pub token_id: String,
    pub maker_amount: String,
    pub taker_amount: String,
    pub expiration: String,
    pub nonce: String,
    pub fee_rate_bps: String,
    pub side: String,
    pub signature_type: u8,
    pub signature: String,
}

impl OrderTemplate {
    /// Build a signed wire order. `price` is a [`Price`] tick; `size` is
    /// USDC atoms. The `salt` is a fresh random nonce.
    pub fn sign(&self, price: Price, size: Size, salt: u64) -> SignedOrder {
        // maker_amount / taker_amount encoding on Polymarket:
        //   BUY:  maker_amount = size × price, taker_amount = size
        //   SELL: maker_amount = size,          taker_amount = size × price
        // Everything is in 6-decimal USDC units; we stay in scaled atoms.
        let price_atoms = price.0 as u64; // 0..=1_000_000
        // size is already in atoms
        let shares = size.0; // shares are stored as the same USD unit for simplicity
        let cost = (shares as u128 * price_atoms as u128 / 1_000_000) as u64;
        let (maker_amount, taker_amount) = match self.side {
            Side::Buy => (cost, shares),
            Side::Sell => (shares, cost),
        };

        SignedOrder {
            salt: salt.to_string(),
            maker: self.maker.clone(),
            signer: self.signer.clone(),
            taker: self.taker.clone(),
            token_id: format!("0x{}", hex_encode(&self.asset)),
            maker_amount: maker_amount.to_string(),
            taker_amount: taker_amount.to_string(),
            expiration: self.expiration_secs.to_string(),
            nonce: self.nonce.to_string(),
            fee_rate_bps: self.fee_rate_bps.to_string(),
            side: match self.side {
                Side::Buy => "BUY".into(),
                Side::Sell => "SELL".into(),
            },
            signature_type: 0,
            // TODO: replace with real ethers::signers EIP-712 signature.
            signature: format!("0x{}", "00".repeat(65)),
        }
    }
}

#[inline]
fn now_nanos() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tpl(side: Side) -> OrderTemplate {
        OrderTemplate {
            asset: [0xab; 32],
            side,
            maker: "0xdead".into(),
            signer: "0xdead".into(),
            taker: "0x0000000000000000000000000000000000000000".into(),
            nonce: 1,
            expiration_secs: 0,
            fee_rate_bps: 0,
        }
    }

    #[test]
    fn order_ids_are_unique_and_monotonic() {
        let a = OrderId::next();
        let b = OrderId::next();
        assert!(b.0 > a.0);
    }

    #[test]
    fn remaining_decreases_with_fills() {
        let mut o = Order::new([0; 32], Side::Buy, Price::from_prob(0.5), Size::from_usdc(100.0));
        assert_eq!(o.remaining(), Size::from_usdc(100.0));
        o.filled = Size::from_usdc(30.0);
        assert_eq!(o.remaining(), Size::from_usdc(70.0));
    }

    #[test]
    fn sign_buy_computes_cost_from_price_times_size() {
        let t = tpl(Side::Buy);
        // size = 100 USDC worth of shares, price = 0.40
        let so = t.sign(Price::from_prob(0.4), Size::from_usdc(100.0), 123);
        // maker_amount (cost) = 100 * 0.4 = 40 USDC = 40_000_000 atoms
        assert_eq!(so.maker_amount, "40000000");
        // taker_amount (shares) = 100 USDC = 100_000_000 atoms
        assert_eq!(so.taker_amount, "100000000");
        assert_eq!(so.side, "BUY");
        assert_eq!(so.salt, "123");
    }

    #[test]
    fn sign_sell_inverts_legs() {
        let t = tpl(Side::Sell);
        let so = t.sign(Price::from_prob(0.4), Size::from_usdc(100.0), 1);
        // sell: maker_amount = shares, taker_amount = cost
        assert_eq!(so.maker_amount, "100000000");
        assert_eq!(so.taker_amount, "40000000");
    }
}
