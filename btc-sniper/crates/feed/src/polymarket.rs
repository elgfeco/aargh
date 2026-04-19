//! Polymarket CLOB WebSocket consumer.
//!
//! Subscribes to the `market` channel for a set of `asset_id`s and maintains
//! per-asset [`OrderBook`]s inside [`MarketState`].
//!
//! The parser uses [`simd_json`] with pre-allocated scratch buffers — the WS
//! reader task owns a single `Vec<u8>` that is reused for every incoming
//! frame. We intentionally avoid `serde_json` on the hot path because its
//! deserialization is ~3× slower than simd-json at the scale we care about.
//!
//! ## Wire format (Polymarket v2)
//!
//! ```text
//! // Snapshot
//! { "event_type": "book",
//!   "asset_id":   "0x...",
//!   "market":     "0x...",
//!   "hash":       "...",
//!   "timestamp":  "1720000000000",
//!   "bids": [ { "price": "0.52", "size": "123.45" }, ... ],
//!   "asks": [ { "price": "0.55", "size": " 67.89" }, ... ] }
//!
//! // Price-change diff
//! { "event_type": "price_change",
//!   "asset_id": "0x...",
//!   "changes":  [ { "price": "0.52", "side": "BUY",  "size": "0" }, ... ],
//!   "timestamp": "..." }
//! ```
//!
//! We treat any field we don't understand as optional — the bot must never
//! crash on a schema drift from Polymarket.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use futures::{SinkExt, StreamExt};
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::connect_async;
use tracing::{debug, error, info, warn};
use url::Url;

use crate::state::MarketState;
use crate::types::{parse_hex32, AssetId, Price, Size};

/// Subset of Polymarket messages we actually act on. Other `event_type`
/// values are ignored (but logged at debug).
#[derive(Clone, Debug)]
pub enum PolymarketMessage {
    Book {
        asset: AssetId,
        bids: Vec<(Price, Size)>,
        asks: Vec<(Price, Size)>,
        ts_ms: u64,
    },
    PriceChange {
        asset: AssetId,
        bids: Vec<(Price, Size)>,
        asks: Vec<(Price, Size)>,
        ts_ms: u64,
    },
    LastTrade {
        asset: AssetId,
        price: Price,
        size: Size,
        ts_ms: u64,
    },
}

/// Top-level WS consumer. Holds the URL, the asset subscription list, and a
/// reference to the shared state it writes into.
pub struct PolymarketFeed {
    url: Url,
    assets: Vec<AssetId>,
    state: Arc<MarketState>,
    /// Ping interval (informational; tungstenite handles Pong automatically)
    ping_interval: Duration,
}

impl PolymarketFeed {
    pub fn new(url: &str, assets: Vec<AssetId>, state: Arc<MarketState>) -> Result<Self> {
        Ok(Self {
            url: Url::parse(url).context("invalid Polymarket WS url")?,
            assets,
            state,
            ping_interval: Duration::from_secs(5),
        })
    }

    /// Run forever: reconnect with exponential backoff on any failure. Only
    /// returns `Err` if the connection can never be established (e.g. bad
    /// URL) — the cancellation path is driven by dropping the tokio task.
    pub async fn run_forever(self: Arc<Self>) -> Result<()> {
        let mut backoff = Duration::from_millis(250);
        let max_backoff = Duration::from_secs(30);

        loop {
            match self.clone().run_once().await {
                Ok(()) => {
                    warn!("polymarket WS closed cleanly — reconnecting");
                    backoff = Duration::from_millis(250);
                }
                Err(e) => {
                    error!(error = %e, "polymarket WS error — reconnecting after {:?}", backoff);
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    }

    async fn run_once(self: Arc<Self>) -> Result<()> {
        info!(url = %self.url, assets = self.assets.len(), "polymarket WS connecting");
        let (mut ws, _resp) = connect_async(self.url.as_str())
            .await
            .context("connect_async failed")?;

        // Subscribe payload — send token IDs as decimal strings (Polymarket
        // WS expects decimal, not hex).
        let asset_ids: Vec<String> = self
            .assets
            .iter()
            .map(|a| bytes32_to_decimal(a))
            .collect();
        let sub = serde_json::json!({
            "type": "subscribe",
            "channel": "market",
            "assets_ids": asset_ids,
        });
        debug!(payload = %sub, "polymarket subscribe");
        ws.send(Message::text(sub.to_string())).await?;
        info!(assets = self.assets.len(), "polymarket WS connected and subscribed");

        let mut last_ping = Instant::now();
        let mut scratch: Vec<u8> = Vec::with_capacity(16 * 1024);
        let mut frame_count: u64 = 0;

        while let Some(msg) = ws.next().await {
            let msg = msg.context("ws recv failed")?;
            match msg {
                Message::Text(t) => {
                    frame_count += 1;
                    if frame_count <= 3 {
                        debug!(frame = frame_count, len = t.len(),
                            preview = &t[..t.len().min(300)],
                            "polymarket raw frame");
                    }
                    scratch.clear();
                    scratch.extend_from_slice(t.as_bytes());
                    self.dispatch_frame(&mut scratch)?;
                }
                Message::Binary(b) => {
                    frame_count += 1;
                    scratch.clear();
                    scratch.extend_from_slice(&b);
                    self.dispatch_frame(&mut scratch)?;
                }
                Message::Ping(p) => {
                    ws.send(Message::Pong(p)).await.ok();
                }
                Message::Pong(_) => {}
                Message::Close(_) => {
                    return Ok(());
                }
                Message::Frame(_) => {}
            }

            if last_ping.elapsed() >= self.ping_interval {
                ws.send(Message::Ping(Vec::new())).await.ok();
                last_ping = Instant::now();
            }
        }

        Ok(())
    }

    /// Parse a raw frame, mutate the book, and bump counters. Parse errors
    /// are logged and ignored — we never crash a feed task on bad input.
    fn dispatch_frame(&self, raw: &mut [u8]) -> Result<()> {
        let parsed = match parse_message(raw) {
            Ok(Some(m)) => m,
            Ok(None) => return Ok(()), // uninteresting event
            Err(e) => {
                debug!(error = %e, "polymarket parse error");
                return Ok(());
            }
        };
        self.apply(parsed);
        Ok(())
    }

    fn apply(&self, msg: PolymarketMessage) {
        let ts_nanos = now_nanos();
        self.state.incr_messages(ts_nanos);
        match msg {
            PolymarketMessage::Book { asset, bids, asks, .. } => {
                let book = self.state.book(asset);
                let seq = ts_nanos;
                book.apply_snapshot(&bids, &asks, seq);
            }
            PolymarketMessage::PriceChange { asset, bids, asks, .. } => {
                let book = self.state.book(asset);
                book.apply_diff(&bids, &asks, ts_nanos);
            }
            PolymarketMessage::LastTrade { asset, .. } => {
                // Reserved for trade-tape integration (not in scope here).
                let _ = asset;
            }
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

/// Public entry point for tests and the executor crate.
///
/// Parses a raw JSON frame into a [`PolymarketMessage`]. Returns `Ok(None)`
/// when the frame is syntactically valid but doesn't carry an event we
/// subscribe to (e.g. heartbeats).
pub fn parse_message(buf: &mut [u8]) -> Result<Option<PolymarketMessage>> {
    // simd-json mutates the input buffer in place — that's why we take &mut [u8].
    let v: simd_json::OwnedValue = simd_json::to_owned_value(buf)?;
    // Top-level may be a single object or an array of objects.
    match v {
        simd_json::OwnedValue::Array(items) => {
            for item in items {
                if let Ok(Some(m)) = parse_one(&item) {
                    return Ok(Some(m));
                }
            }
            Ok(None)
        }
        other => parse_one(&other),
    }
}

fn parse_one(v: &simd_json::OwnedValue) -> Result<Option<PolymarketMessage>> {
    use simd_json::prelude::*;
    let obj = match v {
        simd_json::OwnedValue::Object(o) => o,
        _ => return Ok(None),
    };

    let event_type = obj
        .get("event_type")
        .and_then(|x| x.as_str())
        .unwrap_or("");

    let asset_hex = obj
        .get("asset_id")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    let asset = match parse_hex32(asset_hex) {
        Some(a) => a,
        None => match crate::discovery::decimal_to_bytes32(asset_hex) {
            Some(a) => a,
            None => return Ok(None),
        },
    };

    let ts_ms = obj
        .get("timestamp")
        .and_then(|x| x.as_str().and_then(|s| s.parse::<u64>().ok()))
        .unwrap_or(0);

    match event_type {
        "book" => {
            let bids = parse_levels(obj.get("bids"))?;
            let asks = parse_levels(obj.get("asks"))?;
            Ok(Some(PolymarketMessage::Book {
                asset,
                bids,
                asks,
                ts_ms,
            }))
        }
        "price_change" => {
            let (bids, asks) = parse_changes(obj.get("changes"))?;
            Ok(Some(PolymarketMessage::PriceChange {
                asset,
                bids,
                asks,
                ts_ms,
            }))
        }
        "last_trade_price" => {
            let price = obj
                .get("price")
                .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()))
                .ok_or_else(|| anyhow!("missing price"))?;
            let size = obj
                .get("size")
                .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()))
                .unwrap_or(0.0);
            Ok(Some(PolymarketMessage::LastTrade {
                asset,
                price: Price::from_prob(price),
                size: Size::from_usdc(size),
                ts_ms,
            }))
        }
        _ => Ok(None),
    }
}

fn parse_levels(v: Option<&simd_json::OwnedValue>) -> Result<Vec<(Price, Size)>> {
    use simd_json::prelude::*;
    let arr = match v {
        Some(simd_json::OwnedValue::Array(a)) => a,
        _ => return Ok(Vec::new()),
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        if let simd_json::OwnedValue::Object(obj) = item {
            let price = obj
                .get("price")
                .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()));
            let size = obj
                .get("size")
                .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()));
            if let (Some(p), Some(s)) = (price, size) {
                out.push((Price::from_prob(p), Size::from_usdc(s)));
            }
        }
    }
    Ok(out)
}

fn parse_changes(v: Option<&simd_json::OwnedValue>) -> Result<(Vec<(Price, Size)>, Vec<(Price, Size)>)> {
    use simd_json::prelude::*;
    let arr = match v {
        Some(simd_json::OwnedValue::Array(a)) => a,
        _ => return Ok((Vec::new(), Vec::new())),
    };
    let mut bids = Vec::new();
    let mut asks = Vec::new();
    for item in arr {
        if let simd_json::OwnedValue::Object(obj) = item {
            let price = obj
                .get("price")
                .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()));
            let size = obj
                .get("size")
                .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()));
            let side = obj.get("side").and_then(|x| x.as_str()).unwrap_or("");
            if let (Some(p), Some(s)) = (price, size) {
                let pair = (Price::from_prob(p), Size::from_usdc(s));
                if side.eq_ignore_ascii_case("buy") {
                    bids.push(pair);
                } else {
                    asks.push(pair);
                }
            }
        }
    }
    Ok((bids, asks))
}

/// Convert a 32-byte big-endian array to a decimal string. Inverse of
/// `discovery::decimal_to_bytes32`.
fn bytes32_to_decimal(bytes: &[u8; 32]) -> String {
    // Simple big-integer base conversion: repeatedly divide by 10.
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
    // Skip leading zeros, but keep at least one digit
    let s = String::from_utf8(digits).unwrap_or_else(|_| "0".into());
    let trimmed = s.trim_start_matches('0');
    if trimmed.is_empty() { "0".into() } else { trimmed.into() }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_book_snapshot() {
        let asset_hex = "0x".to_owned() + &"ab".repeat(32);
        let mut buf = format!(
            r#"{{"event_type":"book","asset_id":"{asset_hex}","timestamp":"1720000000000",
             "bids":[{{"price":"0.50","size":"100"}}],
             "asks":[{{"price":"0.55","size":"200"}}]}}"#
        )
        .into_bytes();
        let m = parse_message(&mut buf).unwrap().unwrap();
        match m {
            PolymarketMessage::Book { bids, asks, .. } => {
                assert_eq!(bids.len(), 1);
                assert_eq!(asks.len(), 1);
                assert_eq!(bids[0].0, Price::from_prob(0.50));
                assert_eq!(asks[0].1, Size::from_usdc(200.0));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_price_change_with_mixed_sides() {
        let asset_hex = "0x".to_owned() + &"ab".repeat(32);
        let mut buf = format!(
            r#"{{"event_type":"price_change","asset_id":"{asset_hex}","timestamp":"1",
             "changes":[
               {{"price":"0.50","size":"0","side":"BUY"}},
               {{"price":"0.55","size":"300","side":"SELL"}}
             ]}}"#
        )
        .into_bytes();
        let m = parse_message(&mut buf).unwrap().unwrap();
        match m {
            PolymarketMessage::PriceChange { bids, asks, .. } => {
                assert_eq!(bids.len(), 1);
                assert_eq!(bids[0].1, Size::ZERO); // delete
                assert_eq!(asks.len(), 1);
                assert_eq!(asks[0].1, Size::from_usdc(300.0));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_unknown_event_is_none() {
        let mut buf = br#"{"event_type":"heartbeat"}"#.to_vec();
        assert!(parse_message(&mut buf).unwrap().is_none());
    }
}
