//! Binance trade-stream consumer — pushes into [`BtcTape`].
//!
//! Wire format: `wss://stream.binance.com:9443/ws/btcusdt@trade` sends
//! messages of the form
//!
//! ```json
//! { "e":"trade","E":1700000000000,"s":"BTCUSDT",
//!   "t":12345,"p":"70000.01","q":"0.0123","T":1700000000000,"m":false }
//! ```
//!
//! The `T` field is the trade time, `p` the price, `q` the quantity. We only
//! care about `p` and `q`. This consumer mirrors [`crate::polymarket`]'s
//! reconnect/backoff loop.

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

/// BTC reference-price WS consumer.
pub struct BinanceFeed {
    url: Url,
    source: Source,
    state: Arc<MarketState>,
}

impl BinanceFeed {
    pub fn new(url: &str, state: Arc<MarketState>) -> Result<Self> {
        Self::with_source(url, Source::Binance, state)
    }

    pub fn with_source(url: &str, source: Source, state: Arc<MarketState>) -> Result<Self> {
        Ok(Self {
            url: Url::parse(url).context("invalid BTC WS url")?,
            source,
            state,
        })
    }

    pub async fn run_forever(self: Arc<Self>) -> Result<()> {
        let mut backoff = Duration::from_millis(250);
        let max_backoff = Duration::from_secs(30);
        loop {
            match self.clone().run_once().await {
                Ok(()) => {
                    warn!(source = self.source.as_str(), "BTC feed closed — reconnecting");
                    backoff = Duration::from_millis(250);
                }
                Err(e) => {
                    error!(source = self.source.as_str(), error = %e,
                        "BTC feed error — reconnecting after {:?}", backoff);
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    }

    async fn run_once(self: Arc<Self>) -> Result<()> {
        info!(url = %self.url, source = self.source.as_str(), "BTC feed connecting");
        let (mut ws, _) = connect_async(self.url.as_str())
            .await
            .context("connect_async")?;

        let mut scratch = Vec::with_capacity(4096);
        let mut last_ping = Instant::now();
        while let Some(msg) = ws.next().await {
            let msg = msg.context("ws recv")?;
            match msg {
                Message::Text(t) => {
                    scratch.clear();
                    scratch.extend_from_slice(t.as_bytes());
                    if let Err(e) = self.dispatch(&mut scratch) {
                        debug!(error = %e, "binance parse error");
                    }
                }
                Message::Binary(b) => {
                    scratch.clear();
                    scratch.extend_from_slice(&b);
                    if let Err(e) = self.dispatch(&mut scratch) {
                        debug!(error = %e, "binance parse error");
                    }
                }
                Message::Ping(p) => {
                    ws.send(Message::Pong(p)).await.ok();
                }
                Message::Pong(_) => {}
                Message::Close(_) => return Ok(()),
                Message::Frame(_) => {}
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
        let obj = match v {
            simd_json::OwnedValue::Object(o) => o,
            _ => return Ok(()),
        };

        use simd_json::prelude::*;
        let ev = obj.get("e").and_then(|x| x.as_str()).unwrap_or("");
        if ev != "trade" {
            return Ok(());
        }

        let price = obj
            .get("p")
            .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()));
        let size = obj
            .get("q")
            .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()));

        if let (Some(p), Some(q)) = (price, size) {
            self.state.tape().record(BtcTick {
                price: p,
                size: q,
                ts: Instant::now(),
                source: self.source,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simd_json::OwnedValue;

    #[test]
    fn ignores_non_trade_events() {
        let state = MarketState::new();
        let feed = BinanceFeed::new("wss://example.com/ws", state.clone()).unwrap();
        let mut buf = br#"{"e":"kline","p":"70000","q":"1"}"#.to_vec();
        feed.dispatch(&mut buf).unwrap();
        assert_eq!(state.tape().snapshot().trade_count, 0);
    }

    #[test]
    fn records_valid_trade() {
        let state = MarketState::new();
        let feed = BinanceFeed::new("wss://example.com/ws", state.clone()).unwrap();
        let mut buf = br#"{"e":"trade","p":"70000.01","q":"0.25"}"#.to_vec();
        feed.dispatch(&mut buf).unwrap();
        let snap = state.tape().snapshot();
        assert_eq!(snap.trade_count, 1);
        assert!((snap.last_price - 70_000.01).abs() < 1e-6);
    }

    // Guards against simd-json API drift — it's easy to accidentally use
    // borrowed vs owned and have the test compile but panic at runtime.
    #[test]
    fn simd_json_owned_value_builds() {
        let mut buf = br#"{"x":1}"#.to_vec();
        let v: OwnedValue = simd_json::to_owned_value(&mut buf).unwrap();
        assert!(matches!(v, OwnedValue::Object(_)));
    }
}
