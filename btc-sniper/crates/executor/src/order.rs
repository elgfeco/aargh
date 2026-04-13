//! Order types, EIP-712 signing, and pre-serialization.
//!
//! Polymarket CLOB orders are EIP-712-typed structs signed via ECDSA.
//! The `OrderTemplate::sign()` method computes the EIP-712 struct hash,
//! domain separator hash, and signs the digest with a `k256::SigningKey`.
//!
//! The HOT path (decision -> signed order) uses the pre-built template:
//! only `price`, `size`, and `salt` are swapped at fire time.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Result};
use ethers::abi::{encode, Token};
use ethers::types::{Address, U256};
use ethers::utils::keccak256;
use k256::ecdsa::SigningKey;
use serde::{Deserialize, Serialize};
use sniper_feed::{AssetId, Price, Side, Size};

static ORDER_SEQ: AtomicU64 = AtomicU64::new(1);

// --- EIP-712 constants -------------------------------------------------------

/// Polymarket CTF Exchange on Polygon.
const CTF_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
/// Polymarket Neg Risk CTF Exchange on Polygon.
const NEG_RISK_CTF_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";
/// Polygon chain ID.
const CHAIN_ID: u64 = 137;

const DOMAIN_TYPE: &str =
    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
const ORDER_TYPE: &str = "Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)";

fn domain_separator(neg_risk: bool) -> [u8; 32] {
    let exchange: Address = if neg_risk {
        NEG_RISK_CTF_EXCHANGE
    } else {
        CTF_EXCHANGE
    }
    .parse()
    .expect("hardcoded exchange address");

    keccak256(encode(&[
        Token::FixedBytes(keccak256(DOMAIN_TYPE).to_vec()),
        Token::FixedBytes(keccak256("Polymarket CTF Exchange").to_vec()),
        Token::FixedBytes(keccak256("1").to_vec()),
        Token::Uint(U256::from(CHAIN_ID)),
        Token::Address(exchange),
    ]))
}

#[allow(clippy::too_many_arguments)]
fn order_struct_hash(
    salt: U256,
    maker: Address,
    signer: Address,
    taker: Address,
    token_id: U256,
    maker_amount: U256,
    taker_amount: U256,
    expiration: U256,
    nonce: U256,
    fee_rate_bps: U256,
    side: u8,
    signature_type: u8,
) -> [u8; 32] {
    keccak256(encode(&[
        Token::FixedBytes(keccak256(ORDER_TYPE).to_vec()),
        Token::Uint(salt),
        Token::Address(maker),
        Token::Address(signer),
        Token::Address(taker),
        Token::Uint(token_id),
        Token::Uint(maker_amount),
        Token::Uint(taker_amount),
        Token::Uint(expiration),
        Token::Uint(nonce),
        Token::Uint(fee_rate_bps),
        Token::Uint(U256::from(side)),
        Token::Uint(U256::from(signature_type)),
    ]))
}

fn eip712_digest(domain_sep: [u8; 32], struct_hash: [u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(66);
    buf.push(0x19);
    buf.push(0x01);
    buf.extend_from_slice(&domain_sep);
    buf.extend_from_slice(&struct_hash);
    keccak256(buf)
}

/// Convert a big-endian 32-byte array to a decimal string.
fn bytes32_to_decimal(bytes: &[u8; 32]) -> String {
    let mut tmp = *bytes;
    let mut digits = Vec::with_capacity(80);
    loop {
        let mut remainder: u16 = 0;
        let mut all_zero = true;
        for byte in tmp.iter_mut() {
            let val = (remainder << 8) | (*byte as u16);
            *byte = (val / 10) as u8;
            remainder = val % 10;
            if *byte != 0 {
                all_zero = false;
            }
        }
        digits.push(b'0' + remainder as u8);
        if all_zero {
            break;
        }
    }
    digits.reverse();
    let s = String::from_utf8(digits).unwrap_or_else(|_| "0".into());
    let trimmed = s.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".into()
    } else {
        trimmed.into()
    }
}

// --- Order types -------------------------------------------------------------

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

/// Pre-serialized order template. One per asset/side, cached at startup.
#[derive(Clone, Debug)]
pub struct OrderTemplate {
    pub asset: AssetId,
    pub side: Side,
    pub maker: String,  // wallet address (0x...)
    pub signer: String, // same as maker for L1 accounts
    pub taker: String,  // 0x0 for open orders
    pub nonce: u64,
    pub expiration_secs: u64,
    pub fee_rate_bps: u32,
    pub neg_risk: bool,
}

/// Wire shape of a signed CLOB order.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedOrder {
    pub salt: u64,
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
    /// Not serialized — used by client to pick the right endpoint.
    #[serde(skip)]
    pub neg_risk: bool,
}

impl OrderTemplate {
    /// Build and sign a wire order. Uses real EIP-712 ECDSA signing when
    /// `signing_key` is Some, otherwise produces a stub signature (dry-run).
    pub fn sign(
        &self,
        price: Price,
        size: Size,
        salt: u64,
        signing_key: Option<&SigningKey>,
    ) -> Result<SignedOrder> {
        // maker_amount / taker_amount encoding on Polymarket:
        //   BUY:  maker_amount = size * price, taker_amount = size
        //   SELL: maker_amount = size,          taker_amount = size * price
        // Everything is in 6-decimal USDC units; we stay in scaled atoms.
        let price_atoms = price.0 as u64; // 0..=1_000_000
        let shares = size.0;
        let cost = (shares as u128 * price_atoms as u128 / 1_000_000) as u64;
        let (maker_amount, taker_amount) = match self.side {
            Side::Buy => (cost, shares),
            Side::Sell => (shares, cost),
        };

        let side_num: u8 = match self.side {
            Side::Buy => 0,
            Side::Sell => 1,
        };
        let sig_type: u8 = 0; // EOA

        let token_id_decimal = bytes32_to_decimal(&self.asset);

        let signature = match signing_key {
            Some(sk) => {
                let maker_addr: Address = self
                    .maker
                    .parse()
                    .map_err(|_| anyhow!("bad maker address: {}", self.maker))?;
                let signer_addr: Address = self
                    .signer
                    .parse()
                    .map_err(|_| anyhow!("bad signer address: {}", self.signer))?;
                let taker_addr: Address = self
                    .taker
                    .parse()
                    .map_err(|_| anyhow!("bad taker address: {}", self.taker))?;
                let token_id_u256 = U256::from_big_endian(&self.asset);

                let dom_sep = domain_separator(self.neg_risk);
                let struct_hash = order_struct_hash(
                    U256::from(salt),
                    maker_addr,
                    signer_addr,
                    taker_addr,
                    token_id_u256,
                    U256::from(maker_amount),
                    U256::from(taker_amount),
                    U256::from(self.expiration_secs),
                    U256::from(self.nonce),
                    U256::from(self.fee_rate_bps),
                    side_num,
                    sig_type,
                );
                let digest = eip712_digest(dom_sep, struct_hash);

                let (ecdsa_sig, rec_id) = sk
                    .sign_prehash_recoverable(&digest)
                    .map_err(|e| anyhow!("ECDSA sign: {}", e))?;

                let (r_bytes, s_bytes) = ecdsa_sig.split_bytes();
                let v = u8::from(rec_id) + 27;
                let mut sig_bytes = Vec::with_capacity(65);
                sig_bytes.extend_from_slice(r_bytes.as_ref());
                sig_bytes.extend_from_slice(s_bytes.as_ref());
                sig_bytes.push(v);
                format!("0x{}", hex::encode(&sig_bytes))
            }
            None => {
                // Stub for dry-run / testing
                format!("0x{}", "00".repeat(65))
            }
        };

        Ok(SignedOrder {
            salt,
            maker: self.maker.clone(),
            signer: self.signer.clone(),
            taker: self.taker.clone(),
            token_id: token_id_decimal,
            maker_amount: maker_amount.to_string(),
            taker_amount: taker_amount.to_string(),
            expiration: self.expiration_secs.to_string(),
            nonce: self.nonce.to_string(),
            fee_rate_bps: self.fee_rate_bps.to_string(),
            side: match self.side {
                Side::Buy => "BUY".into(),
                Side::Sell => "SELL".into(),
            },
            signature_type: sig_type,
            signature,
            neg_risk: self.neg_risk,
        })
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
            neg_risk: false,
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
        let so = t.sign(Price::from_prob(0.4), Size::from_usdc(100.0), 123, None).unwrap();
        // maker_amount (cost) = 100 * 0.4 = 40 USDC = 40_000_000 atoms
        assert_eq!(so.maker_amount, "40000000");
        // taker_amount (shares) = 100 USDC = 100_000_000 atoms
        assert_eq!(so.taker_amount, "100000000");
        assert_eq!(so.side, "BUY");
        assert_eq!(so.salt, 123);
    }

    #[test]
    fn sign_sell_inverts_legs() {
        let t = tpl(Side::Sell);
        let so = t.sign(Price::from_prob(0.4), Size::from_usdc(100.0), 1, None).unwrap();
        // sell: maker_amount = shares, taker_amount = cost
        assert_eq!(so.maker_amount, "100000000");
        assert_eq!(so.taker_amount, "40000000");
    }

    #[test]
    fn bytes32_to_decimal_one() {
        let mut b = [0u8; 32];
        b[31] = 1;
        assert_eq!(bytes32_to_decimal(&b), "1");
    }

    #[test]
    fn bytes32_to_decimal_zero() {
        assert_eq!(bytes32_to_decimal(&[0u8; 32]), "0");
    }

    #[test]
    fn bytes32_to_decimal_256() {
        let mut b = [0u8; 32];
        b[30] = 1; // 256 = 0x0100
        assert_eq!(bytes32_to_decimal(&b), "256");
    }

    #[test]
    fn token_id_is_decimal_not_hex() {
        let t = tpl(Side::Buy);
        let so = t.sign(Price::from_prob(0.5), Size::from_usdc(10.0), 1, None).unwrap();
        // token_id must NOT start with "0x" — Polymarket expects decimal
        assert!(!so.token_id.starts_with("0x"), "token_id should be decimal, got: {}", so.token_id);
    }
}
