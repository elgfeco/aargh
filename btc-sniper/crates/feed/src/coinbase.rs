//! Coinbase Advanced Trade WebSocket consumer — pushes into [`BtcTape`].
//!
//! Wire format: `wss://advanced-trade-ws.coinbase.com` uses a JSON-RPC style
//! subscription. Subscribe with:
//!
//! ```json
//! { "type": "subscribe",
//!   "product_ids": ["BTC-USD"],
//!   "channel": "market_trades" }
//! ```
//!
//! Trade events arrive as:
//!
//! ```json
//! { "channel": "market_trades",
//!   "events": [{ "type": "update",
//!     "trades": [{ "price": "70000.01", "size": "0.0123",
//!                   "side": "BUY", "trade_id": "..." }] }] }
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::connect_async;
use tracing::{debug, error, info, warn};
use url::Url;

use crate::btc::{BtcTick, Source};
use crate::state::MarketState;

/// Coinbase BTC reference-price WS consumer.
pub struct CoinbaseFeed {
    url: Url,
    state: Arc<MarketState>,
}

impl CoinbaseFeed {
    pub fn new(url: &str, state: Arc<MarketState>) -> Result<Self> {
        Ok(Self {
            url: Url::parse(url).context("invalid Coinbase WS url")?,
            state,
        })
    }

    pub async fn run_forever(self: Arc<Self>) -> Result<()> {
        let mut backoff = Duration::from_millis(250);
        let max_backoff = Duration::from_secs(30);
        loop {
            match self.clone().run_once().await {
                Ok(()) => {
                    warn!("Coinbase feed closed — reconnecting");
                    backoff = Duration::from_millis(250);
                }
                Err(e) => {
                    error!(error = %e, "Coinbase feed error — reconnecting after {:?}", backoff);
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    }

    async fn run_once(self: Arc<Self>) -> Result<()> {
        info!(url = %self.url, "Coinbase BTC feed connecting");
        let (mut ws, _) = connect_async(self.url.as_str())
            .await
            .context("connect_async")?;

        // Subscribe to BTC-USD market trades
        let sub = serde_json::json!({
            "type": "subscribe",
            "product_ids": ["BTC-USD"],
            "channel": "market_trades"
        });
        ws.send(Message::Text(sub.to_string().into()))
            .await
            .context("subscribe send")?;
        info!("Coinbase BTC feed connected and subscribed");

        let mut scratch = Vec::with_capacity(4096);
        let mut last_ping = Instant::now();
        let mut frame_count: u64 = 0;
        let mut last_status = Instant::now();

        while let Some(msg) = ws.next().await {
            let msg = msg.context("ws recv")?;
            match msg {
                Message::Text(t) => {
                    scratch.clear();
                    scratch.extend_from_slice(t.as_bytes());
                    if let Err(e) = self.dispatch(&mut scratch) {
                        debug!(error = %e, "coinbase parse error");
                    }
                }
                Message::Binary(b) => {
                    scratch.clear();
                    scratch.extend_from_slice(&b);
                    if let Err(e) = self.dispatch(&mut scratch) {
                        debug!(error = %e, "coinbase parse error");
                    }
                }
                Message::Ping(p) => {
                    ws.send(Message::Pong(p)).await.ok();
                }
                Message::Pong(_) => {}
                Message::Close(_) => return Ok(()),
                Message::Frame(_) => {}
            }

            frame_count += 1;
            if last_status.elapsed() > Duration::from_secs(10) {
                let snap = self.state.tape().snapshot();
                info!(
                    source = "coinbase",
                    frames = frame_count,
                    btc_price = format_args!("{:.2}", snap.last_price),
                    trades = snap.trade_count,
                    ema_fast = format_args!("{:.2}", snap.ema_fast),
                    ema_slow = format_args!("{:.2}", snap.ema_slow),
                    momentum = format_args!("{:.4}", snap.momentum),
                    "BTC tape status"
                );
                last_status = Instant::now();
            }

            if last_ping.elapsed() > Duration::from_secs(15) {
                ws.send(Message::Ping(Vec::new())).await.ok();
                last_ping = Instant::now();
            }
        }
        Ok(())
    }

    fn dispatch(&self, raw: &mut [u8]) -> Result<()> {
        let v: simd_json::OwnedValue = simd_json::to_owned_value(raw)?;
        let obj = match &v {
            simd_json::OwnedValue::Object(o) => o,
            _ => return Ok(()),
        };

        use simd_json::prelude::*;

        let channel = obj.get("channel").and_then(|x| x.as_str()).unwrap_or("");
        if channel != "market_trades" {
            return Ok(());
        }

        let events = match obj.get("events") {
            Some(simd_json::OwnedValue::Array(arr)) => arr,
            _ => return Ok(()),
        };

        for event in events {
            let trades = match event.get("trades") {
                Some(simd_json::OwnedValue::Array(arr)) => arr,
                _ => continue,
            };
            for trade in trades {
                let price = trade
                    .get("price")
                    .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()));
                let size = trade
                    .get("size")
                    .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()));

                if let (Some(p), Some(q)) = (price, size) {
                    self.state.tape().record(BtcTick {
                        price: p,
                        size: q,
                        ts: Instant::now(),
                        source: Source::Coinbase,
                    });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_coinbase_trade() {
        let state = MarketState::new();
        let feed = CoinbaseFeed::new("wss://example.com/ws", state.clone()).unwrap();
        let mut buf = br#"{"channel":"market_trades","events":[{"type":"update","trades":[{"price":"71000.50","size":"0.5","side":"BUY","trade_id":"1"}]}]}"#.to_vec();
        feed.dispatch(&mut buf).unwrap();
        let snap = state.tape().snapshot();
        assert_eq!(snap.trade_count, 1);
        assert!((snap.last_price - 71_000.50).abs() < 1e-6);
    }

    #[test]
    fn ignores_non_trade_channel() {
        let state = MarketState::new();
        let feed = CoinbaseFeed::new("wss://example.com/ws", state.clone()).unwrap();
        let mut buf = br#"{"channel":"heartbeats","events":[]}"#.to_vec();
        feed.dispatch(&mut buf).unwrap();
        assert_eq!(state.tape().snapshot().trade_count, 0);
    }
}
