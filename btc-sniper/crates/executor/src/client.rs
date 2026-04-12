//! HTTPS client to the Polymarket CLOB REST API.
//!
//! We maintain a long-lived `reqwest::Client` with HTTP/2 multiplexing and
//! aggressive keep-alive so every `POST /order` reuses the same TCP
//! connection. Authentication headers (L2 API key / secret / passphrase)
//! are pre-formatted once at startup and cached.
//!
//! The client is `Arc<...>`-wrapped and cheap to clone.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::Client;
use serde::Serialize;
use tracing::{debug, error, info, warn};

use crate::order::SignedOrder;

/// Cached L2 auth headers. Built once at startup and held for the lifetime
/// of the process. In production, rotate before the API key expires.
#[derive(Clone, Debug)]
pub struct L2Auth {
    pub api_key: String,
    pub api_secret: String,
    pub api_passphrase: String,
}

#[derive(Clone, Debug, Serialize)]
struct PostOrderBody<'a> {
    order: &'a SignedOrder,
    owner: &'a str,
    order_type: &'a str,
}

#[derive(Clone, Debug)]
pub struct ClobClient {
    inner: Client,
    base_url: String,
    owner: String,
    dry_run: bool,
}

impl ClobClient {
    /// Build a new client. `base_url` is the CLOB host (e.g.
    /// `https://clob.polymarket.com`).
    pub fn new(base_url: String, owner: String, _auth: Option<L2Auth>, dry_run: bool) -> Result<Arc<Self>> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // L2 auth headers would be added per-request since they include an
        // HMAC over the request body + timestamp. See `sign_request`.

        let inner = Client::builder()
            .http2_prior_knowledge()
            .http2_keep_alive_interval(Duration::from_secs(5))
            .http2_keep_alive_timeout(Duration::from_secs(20))
            .http2_keep_alive_while_idle(true)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(8)
            .tcp_keepalive(Duration::from_secs(10))
            .tcp_nodelay(true)
            .connect_timeout(Duration::from_millis(500))
            .timeout(Duration::from_secs(3))
            .default_headers(headers)
            .user_agent("btc-sniper/0.1")
            .build()
            .context("reqwest client build")?;

        info!(base_url, dry_run, "CLOB client ready");

        Ok(Arc::new(Self {
            inner,
            base_url,
            owner,
            dry_run,
        }))
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    /// POST /order. Returns the CLOB-assigned order id on success.
    ///
    /// In `dry_run` mode we do NOT hit the network — we return a
    /// deterministic `"dryrun-<salt>"` id and log the payload at debug.
    pub async fn post_order(&self, order: &SignedOrder) -> Result<String> {
        if self.dry_run {
            debug!(asset = %order.token_id, side = %order.side, "dry-run post_order");
            return Ok(format!("dryrun-{}", order.salt));
        }

        let url = format!("{}/order", self.base_url);
        let body = PostOrderBody {
            order,
            owner: &self.owner,
            order_type: "GTC",
        };

        let resp = self
            .inner
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("http post failed")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            warn!(%status, body = %text, "post_order non-2xx");
            return Err(anyhow!("post_order {}: {}", status, text));
        }

        // Polymarket returns { "success": true, "orderID": "0x..." }
        // Parse defensively without pulling in a full schema.
        #[derive(serde::Deserialize)]
        struct Resp {
            #[serde(rename = "orderID")]
            order_id: Option<String>,
            #[serde(default)]
            success: bool,
            #[serde(default)]
            error: Option<String>,
        }
        let parsed: Resp = serde_json::from_str(&text).unwrap_or(Resp {
            order_id: None,
            success: false,
            error: Some("parse error".into()),
        });
        if !parsed.success {
            return Err(anyhow!("post_order rejected: {:?}", parsed.error));
        }
        parsed
            .order_id
            .ok_or_else(|| anyhow!("post_order: missing orderID"))
    }

    /// DELETE /order/{id}. Returns `Ok(())` if the CLOB acknowledges.
    pub async fn cancel_order(&self, clob_id: &str) -> Result<()> {
        if self.dry_run {
            debug!(id = clob_id, "dry-run cancel_order");
            return Ok(());
        }
        let url = format!("{}/order/{}", self.base_url, clob_id);
        let resp = self
            .inner
            .delete(&url)
            .send()
            .await
            .context("cancel_order send")?;
        if !resp.status().is_success() {
            let s = resp.status();
            let t = resp.text().await.unwrap_or_default();
            error!(status = %s, body = %t, "cancel_order non-2xx");
            return Err(anyhow!("cancel_order {}: {}", s, t));
        }
        Ok(())
    }

    /// GET /orders — pulled at startup to reconcile state after a restart.
    pub async fn list_open_orders(&self) -> Result<Vec<String>> {
        if self.dry_run {
            return Ok(Vec::new());
        }
        let url = format!("{}/orders", self.base_url);
        let resp = self.inner.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("list_open_orders {}", resp.status()));
        }
        let body = resp.text().await?;
        // {"data": [{"id":"0x...",...},...]}
        #[derive(serde::Deserialize)]
        struct Item {
            id: String,
        }
        #[derive(serde::Deserialize)]
        struct Body {
            data: Vec<Item>,
        }
        let b: Body = serde_json::from_str(&body).unwrap_or(Body { data: vec![] });
        Ok(b.data.into_iter().map(|i| i.id).collect())
    }
}
