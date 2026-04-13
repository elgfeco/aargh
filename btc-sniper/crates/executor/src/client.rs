//! HTTPS client to the Polymarket CLOB REST API.
//!
//! We maintain a long-lived `reqwest::Client` with HTTP/2 multiplexing and
//! aggressive keep-alive so every `POST /order` reuses the same TCP
//! connection. Authentication headers (L2 API key / secret / passphrase)
//! are computed per-request via HMAC-SHA256.
//!
//! The client is `Arc<...>`-wrapped and cheap to clone.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::URL_SAFE;
use base64::Engine;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::Client;
use serde::Serialize;
use sha2::Sha256;
use tracing::{debug, error, info, warn};

use crate::order::SignedOrder;

type HmacSha256 = Hmac<Sha256>;

/// L2 auth credentials. Built once at startup.
#[derive(Clone, Debug)]
pub struct L2Auth {
    pub api_key: String,
    pub api_secret: String,
    pub api_passphrase: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
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
    auth: Option<L2Auth>,
    dry_run: bool,
}

impl ClobClient {
    /// Build a new client. `base_url` is the CLOB host (e.g.
    /// `https://clob.polymarket.com`).
    pub fn new(
        base_url: String,
        owner: String,
        auth: Option<L2Auth>,
        dry_run: bool,
        proxy_url: Option<&str>,
    ) -> Result<Arc<Self>> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let mut builder = Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(8)
            .tcp_keepalive(Duration::from_secs(10))
            .tcp_nodelay(true)
            .connect_timeout(Duration::from_millis(2000))
            .timeout(Duration::from_secs(5))
            .default_headers(headers)
            .user_agent("btc-sniper/0.1");

        if let Some(proxy) = proxy_url {
            info!(proxy, "CLOB client using proxy");
            let p = reqwest::Proxy::all(proxy).context("invalid proxy URL")?;
            builder = builder.proxy(p);
        } else {
            builder = builder
                .http2_prior_knowledge()
                .http2_keep_alive_interval(Duration::from_secs(5))
                .http2_keep_alive_timeout(Duration::from_secs(20))
                .http2_keep_alive_while_idle(true);
        }

        let inner = builder.build().context("reqwest client build")?;

        info!(
            base_url,
            dry_run,
            has_auth = auth.is_some(),
            "CLOB client ready"
        );

        Ok(Arc::new(Self {
            inner,
            base_url,
            owner,
            auth,
            dry_run,
        }))
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    /// Build L2 auth headers for a request.
    /// Returns empty headers if no auth is configured.
    fn l2_headers(&self, method: &str, path: &str, body: Option<&str>) -> HeaderMap {
        let auth = match &self.auth {
            Some(a) => a,
            None => return HeaderMap::new(),
        };

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let ts_str = timestamp.to_string();

        // Build HMAC message: timestamp + method + path [+ body]
        let mut message = format!("{}{}{}", ts_str, method, path);
        if let Some(b) = body {
            message.push_str(b);
        }

        // Decode base64 secret, compute HMAC-SHA256, encode result as base64
        let secret_bytes = URL_SAFE
            .decode(&auth.api_secret)
            .unwrap_or_default();
        let signature = if let Ok(mut mac) = HmacSha256::new_from_slice(&secret_bytes) {
            mac.update(message.as_bytes());
            URL_SAFE.encode(mac.finalize().into_bytes())
        } else {
            warn!("failed to create HMAC from secret");
            String::new()
        };

        let mut headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&self.owner) {
            headers.insert("POLY_ADDRESS", v);
        }
        if let Ok(v) = HeaderValue::from_str(&auth.api_key) {
            headers.insert("POLY_API_KEY", v);
        }
        if let Ok(v) = HeaderValue::from_str(&auth.api_passphrase) {
            headers.insert("POLY_PASSPHRASE", v);
        }
        if let Ok(v) = HeaderValue::from_str(&ts_str) {
            headers.insert("POLY_TIMESTAMP", v);
        }
        if let Ok(v) = HeaderValue::from_str(&signature) {
            headers.insert("POLY_SIGNATURE", v);
        }
        headers
    }

    /// POST /order. Returns the CLOB-assigned order id on success.
    pub async fn post_order(&self, order: &SignedOrder) -> Result<String> {
        if self.dry_run {
            debug!(asset = %order.token_id, side = %order.side, "dry-run post_order");
            return Ok(format!("dryrun-{}", order.salt));
        }

        let path = if order.neg_risk { "/neg-risk/order" } else { "/order" };
        let url = format!("{}{}", self.base_url, path);
        let body = PostOrderBody {
            order,
            owner: &self.owner,
            order_type: "GTC",
        };
        let body_json = serde_json::to_string(&body).context("serialize order body")?;
        debug!(body = %body_json, path, "posting order to CLOB");

        let headers = self.l2_headers("POST", path, Some(&body_json));

        let resp = self
            .inner
            .post(&url)
            .headers(headers)
            .body(body_json)
            .send()
            .await
            .context("http post failed")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            warn!(%status, body = %text, "post_order non-2xx");
            return Err(anyhow!("post_order {}: {}", status, text));
        }

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
        let path = format!("/order/{}", clob_id);
        let url = format!("{}{}", self.base_url, path);
        let headers = self.l2_headers("DELETE", &path, None);

        let resp = self
            .inner
            .delete(&url)
            .headers(headers)
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
        let path = "/orders";
        let url = format!("{}{}", self.base_url, path);
        let headers = self.l2_headers("GET", path, None);

        let resp = self.inner.get(&url).headers(headers).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("list_open_orders {}", resp.status()));
        }
        let body = resp.text().await?;
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
