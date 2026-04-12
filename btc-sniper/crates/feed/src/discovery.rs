//! Market discovery — polls the Polymarket Gamma API for active BTC
//! up/down markets and returns their token IDs for WS subscription.
//!
//! The 5m/15m/60m BTC markets on Polymarket rotate every few minutes.
//! This module auto-discovers the current active set so the bot can
//! subscribe to their WS feeds without hardcoded condition IDs.

use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::types::AssetId;

/// A market discovered from the Gamma API.
#[derive(Clone, Debug)]
pub struct DiscoveredMarket {
    /// Condition ID (32 bytes) — identifies the market question.
    pub condition_id: AssetId,
    /// YES outcome token ID (32 bytes) — this is the `asset_id` on the WS.
    pub yes_token: AssetId,
    /// NO outcome token ID (32 bytes).
    pub no_token: AssetId,
    /// Slug like `btc-updown-5m-1776054900`.
    pub slug: String,
    /// Human-readable question.
    pub question: String,
    /// Whether the market is actively accepting orders.
    pub accepting_orders: bool,
}

/// Gamma API market response (only deserialize fields we need).
#[derive(Deserialize)]
struct GammaMarket {
    #[serde(rename = "conditionId", default)]
    condition_id: Option<String>,
    #[serde(rename = "clobTokenIds", default)]
    clob_token_ids: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    closed: Option<bool>,
    #[serde(rename = "acceptingOrders", default)]
    accepting_orders: Option<bool>,
}

/// Convert a decimal string (like the CLOB token ID) into a big-endian
/// 32-byte array. Returns `None` on parse failure or overflow.
pub fn decimal_to_bytes32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut result = [0u8; 32];
    for &ch in s.as_bytes() {
        if !(b'0'..=b'9').contains(&ch) {
            return None;
        }
        let digit = (ch - b'0') as u16;
        let mut carry = digit;
        for byte in result.iter_mut().rev() {
            let val = (*byte as u16) * 10 + carry;
            *byte = (val & 0xff) as u8;
            carry = val >> 8;
        }
        if carry != 0 {
            return None; // overflow: number > 2^256
        }
    }
    Some(result)
}

/// Fetch active BTC up/down markets from the Gamma API.
///
/// Filters for slugs matching `btc-updown-*` that are open and accepting
/// orders. Returns an empty vec on API failure (never panics).
pub async fn discover_btc_markets(
    client: &reqwest::Client,
    gamma_base: &str,
) -> Vec<DiscoveredMarket> {
    let url = format!(
        "{}/markets?closed=false&active=true&limit=200&order=startDate&ascending=false",
        gamma_base,
    );

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "gamma API request failed");
            return Vec::new();
        }
    };

    if !resp.status().is_success() {
        warn!(status = %resp.status(), "gamma API returned error");
        return Vec::new();
    }

    let markets: Vec<GammaMarket> = match resp.json().await {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "gamma API JSON parse failed");
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for m in markets {
        let slug = m.slug.unwrap_or_default();
        if !slug.contains("btc-updown") {
            continue;
        }
        if m.closed.unwrap_or(true) {
            continue;
        }
        if !m.active.unwrap_or(false) {
            continue;
        }

        // Parse condition ID (hex)
        let cid_hex = match m.condition_id {
            Some(ref s) => s.as_str(),
            None => continue,
        };
        let condition_id = match crate::types::parse_hex32(cid_hex) {
            Some(b) => b,
            None => {
                debug!(slug = %slug, "skipping: bad conditionId");
                continue;
            }
        };

        // Parse clobTokenIds: JSON string containing a JSON array of decimal strings
        // e.g. "[\"1234...\", \"5678...\"]"
        let token_str = match m.clob_token_ids {
            Some(ref s) => s.clone(),
            None => continue,
        };
        let token_ids: Vec<String> = match serde_json::from_str(&token_str) {
            Ok(v) => v,
            Err(_) => {
                debug!(slug = %slug, "skipping: bad clobTokenIds JSON");
                continue;
            }
        };
        if token_ids.len() < 2 {
            continue;
        }

        let yes_token = match decimal_to_bytes32(&token_ids[0]) {
            Some(b) => b,
            None => {
                debug!(slug = %slug, "skipping: bad YES token decimal");
                continue;
            }
        };
        let no_token = match decimal_to_bytes32(&token_ids[1]) {
            Some(b) => b,
            None => {
                debug!(slug = %slug, "skipping: bad NO token decimal");
                continue;
            }
        };

        out.push(DiscoveredMarket {
            condition_id,
            yes_token,
            no_token,
            slug,
            question: m.question.unwrap_or_default(),
            accepting_orders: m.accepting_orders.unwrap_or(false),
        });
    }

    if !out.is_empty() {
        info!(count = out.len(), "discovered BTC updown markets");
        for dm in &out {
            debug!(
                slug = %dm.slug,
                question = %dm.question,
                accepting = dm.accepting_orders,
                "  discovered market"
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_to_bytes32_one() {
        let b = decimal_to_bytes32("1").unwrap();
        assert_eq!(b[31], 1);
        for &x in &b[..31] {
            assert_eq!(x, 0);
        }
    }

    #[test]
    fn decimal_to_bytes32_256() {
        // 256 = 0x0100
        let b = decimal_to_bytes32("256").unwrap();
        assert_eq!(b[31], 0);
        assert_eq!(b[30], 1);
    }

    #[test]
    fn decimal_to_bytes32_real_token_id() {
        let s = "56078938060096976448086754249497300447360333783952000147427828224794011030104";
        let b = decimal_to_bytes32(s).unwrap();
        // Verify it's non-trivial (not all zeros)
        assert!(b.iter().any(|&x| x != 0));
    }

    #[test]
    fn decimal_to_bytes32_empty() {
        assert!(decimal_to_bytes32("").is_none());
    }

    #[test]
    fn decimal_to_bytes32_bad_chars() {
        assert!(decimal_to_bytes32("12abc").is_none());
    }

    #[test]
    fn decimal_to_bytes32_roundtrip() {
        // 255 = 0xFF
        let b = decimal_to_bytes32("255").unwrap();
        assert_eq!(b[31], 0xFF);
        // 65535 = 0xFFFF
        let b = decimal_to_bytes32("65535").unwrap();
        assert_eq!(b[31], 0xFF);
        assert_eq!(b[30], 0xFF);
    }
}
