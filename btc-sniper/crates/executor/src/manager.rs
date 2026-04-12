//! `OrderManager` — stateful wrapper around [`ClobClient`].
//!
//! Responsibilities:
//!
//! * Own the in-memory `DashMap<OrderId, Order>` of open / recent orders
//! * Provide a `submit` method that takes a signal [`Intent`] and fires the
//!   HTTP request (dry-run aware)
//! * Spawn a background tokio task that cancels orders older than
//!   `stale_ttl_ms` (default 500ms)
//! * Expose a `snapshot()` for the dashboard and a `kill_all()` for SIGTERM
//!
//! The hot path (`submit`) uses an Arc<Self> + async HTTP call. The tokio
//! task for TTL sweeping runs every 50 ms.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use dashmap::DashMap;
use rand::Rng;
use sniper_feed::{AssetId, Price, Side, Size};
use sniper_signal::Intent;
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::client::ClobClient;
use crate::latency::{LatencyStage, LatencyStats};
use crate::order::{Order, OrderId, OrderState, OrderTemplate};

/// Runtime config for the order manager — wired from env in `main.rs`.
#[derive(Clone, Debug)]
pub struct OrderManagerConfig {
    /// Maximum age before an unfilled order is auto-cancelled.
    pub stale_ttl_ms: u64,
    /// Max simultaneous open orders (circuit breaker).
    pub max_open_orders: usize,
    /// Wallet address used as `owner`.
    pub owner: String,
}

impl Default for OrderManagerConfig {
    fn default() -> Self {
        Self {
            stale_ttl_ms: 500,
            max_open_orders: 8,
            owner: "0x0000000000000000000000000000000000000000".into(),
        }
    }
}

/// Stateful order lifecycle manager. Cloneable via `Arc<Self>`.
pub struct OrderManager {
    client: Arc<ClobClient>,
    config: OrderManagerConfig,
    orders: DashMap<OrderId, Order>,
    templates: DashMap<(AssetId, Side), OrderTemplate>,
    stats: Arc<LatencyStats>,
    paused: AtomicBool,
    fills_total: AtomicU64,
}

impl OrderManager {
    pub fn new(
        client: Arc<ClobClient>,
        config: OrderManagerConfig,
        stats: Arc<LatencyStats>,
    ) -> Arc<Self> {
        Arc::new(Self {
            client,
            config,
            orders: DashMap::new(),
            templates: DashMap::new(),
            stats,
            paused: AtomicBool::new(false),
            fills_total: AtomicU64::new(0),
        })
    }

    pub fn register_template(&self, tpl: OrderTemplate) {
        self.templates.insert((tpl.asset, tpl.side), tpl);
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
        info!(paused, "executor pause toggled");
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn open_count(&self) -> usize {
        self.orders
            .iter()
            .filter(|e| !e.value().is_terminal())
            .count()
    }

    pub fn fills_total(&self) -> u64 {
        self.fills_total.load(Ordering::Relaxed)
    }

    /// Snapshot all orders. Allocates — call at human-speed (dashboard),
    /// not on the hot path.
    pub fn snapshot(&self) -> Vec<Order> {
        self.orders.iter().map(|e| e.value().clone()).collect()
    }

    /// Fire an order derived from a signal [`Intent`].
    ///
    /// Returns `Ok(Some(OrderId))` on success, `Ok(None)` if the intent is
    /// `Hold`, or `Err` on network / auth failure.
    pub async fn submit(self: &Arc<Self>, asset: AssetId, intent: Intent) -> Result<Option<OrderId>> {
        let (side, price, size) = match intent {
            Intent::Hold => return Ok(None),
            Intent::Fire { side, price, size } => (side, price, size),
        };

        if self.is_paused() {
            debug!("submit blocked: executor paused");
            return Ok(None);
        }
        if self.open_count() >= self.config.max_open_orders {
            warn!(
                max = self.config.max_open_orders,
                "submit blocked: max_open_orders reached"
            );
            return Ok(None);
        }
        if size == Size::ZERO {
            debug!("submit blocked: zero size");
            return Ok(None);
        }

        let t_eval = std::time::Instant::now();
        let tpl = match self.templates.get(&(asset, side)) {
            Some(t) => t.clone(),
            None => {
                warn!("submit blocked: no template for asset/side");
                return Ok(None);
            }
        };

        let salt: u64 = rand::thread_rng().gen();
        let signed = tpl.sign(price, size, salt);
        self.stats.record(
            LatencyStage::SignalEval,
            t_eval.elapsed().as_nanos() as u64,
        );

        let mut order = Order::new(asset, side, price, size);
        self.orders.insert(order.id, order.clone());

        let t_fire = std::time::Instant::now();
        let result = self.client.post_order(&signed).await;
        let fire_ns = t_fire.elapsed().as_nanos() as u64;
        self.stats.record(LatencyStage::OrderFired, fire_ns);

        match result {
            Ok(clob_id) => {
                order.clob_id = Some(clob_id.clone());
                order.state = OrderState::Acked;
                self.orders.insert(order.id, order.clone());
                self.stats.record(LatencyStage::OrderAcked, fire_ns);
                info!(id = order.id.0, %clob_id, side = side.as_str(),
                      price = %price, size = %size, "order acked");
                Ok(Some(order.id))
            }
            Err(e) => {
                order.state = OrderState::Rejected;
                self.orders.insert(order.id, order);
                warn!(error = %e, "order rejected");
                Err(e)
            }
        }
    }

    /// Background task — cancels orders older than `stale_ttl_ms`.
    pub async fn run_stale_sweeper(self: Arc<Self>) {
        let mut tick = interval(Duration::from_millis(50));
        loop {
            tick.tick().await;
            let now = now_nanos();
            let ttl_ns = self.config.stale_ttl_ms * 1_000_000;
            let mut to_cancel: Vec<(OrderId, String)> = Vec::new();
            for entry in self.orders.iter() {
                let o = entry.value();
                if matches!(o.state, OrderState::Acked)
                    && now.saturating_sub(o.submitted_at_nanos) > ttl_ns
                {
                    if let Some(id) = &o.clob_id {
                        to_cancel.push((o.id, id.clone()));
                    }
                }
            }
            for (oid, clob_id) in to_cancel {
                match self.client.cancel_order(&clob_id).await {
                    Ok(()) => {
                        if let Some(mut e) = self.orders.get_mut(&oid) {
                            e.state = OrderState::Cancelled;
                        }
                        info!(id = oid.0, %clob_id, "stale order cancelled");
                    }
                    Err(e) => warn!(error = %e, "stale cancel failed"),
                }
            }
        }
    }

    /// Cancel every live order. Called on SIGTERM before exiting.
    pub async fn kill_all(self: &Arc<Self>) {
        let ids: Vec<(OrderId, String)> = self
            .orders
            .iter()
            .filter_map(|e| {
                let o = e.value();
                if !o.is_terminal() {
                    o.clob_id.clone().map(|c| (o.id, c))
                } else {
                    None
                }
            })
            .collect();
        info!(count = ids.len(), "kill_all: cancelling live orders");
        for (oid, clob_id) in ids {
            if let Err(e) = self.client.cancel_order(&clob_id).await {
                warn!(error = %e, id = oid.0, "kill_all cancel failed");
            }
        }
    }

    /// Record a fill arriving from the user WS channel. `filled_delta` adds
    /// to the existing `filled` count; when `filled >= size` we mark the
    /// order `Filled`.
    pub fn record_fill(&self, oid: OrderId, filled_delta: Size) {
        if let Some(mut entry) = self.orders.get_mut(&oid) {
            entry.filled = entry
                .filled
                .checked_add(filled_delta)
                .unwrap_or(entry.filled);
            if entry.filled.0 >= entry.size.0 {
                entry.state = OrderState::Filled;
            } else {
                entry.state = OrderState::PartialFill;
            }
        }
        self.fills_total.fetch_add(1, Ordering::Relaxed);
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

// Silence unused imports in dev builds.
#[allow(dead_code)]
fn _unused(_p: Price) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk() -> Arc<OrderManager> {
        let client = ClobClient::new(
            "https://example.invalid".into(),
            "0xdead".into(),
            None,
            true, // dry run
        )
        .unwrap();
        OrderManager::new(
            client,
            OrderManagerConfig {
                stale_ttl_ms: 1,
                max_open_orders: 2,
                owner: "0xdead".into(),
            },
            Arc::new(LatencyStats::new()),
        )
    }

    #[tokio::test]
    async fn submit_hold_returns_none() {
        let m = mk();
        let r = m.submit([0; 32], Intent::Hold).await.unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn submit_without_template_is_nop() {
        let m = mk();
        let r = m
            .submit(
                [1; 32],
                Intent::Fire {
                    side: Side::Buy,
                    price: Price::from_prob(0.5),
                    size: Size::from_usdc(100.0),
                },
            )
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn submit_with_template_dry_runs() {
        let m = mk();
        m.register_template(OrderTemplate {
            asset: [1; 32],
            side: Side::Buy,
            maker: "0xdead".into(),
            signer: "0xdead".into(),
            taker: "0x0000000000000000000000000000000000000000".into(),
            nonce: 1,
            expiration_secs: 0,
            fee_rate_bps: 0,
        });
        let r = m
            .submit(
                [1; 32],
                Intent::Fire {
                    side: Side::Buy,
                    price: Price::from_prob(0.5),
                    size: Size::from_usdc(100.0),
                },
            )
            .await
            .unwrap();
        assert!(r.is_some());
        assert_eq!(m.open_count(), 1);
    }

    #[tokio::test]
    async fn pause_blocks_submission() {
        let m = mk();
        m.set_paused(true);
        let r = m
            .submit(
                [1; 32],
                Intent::Fire {
                    side: Side::Buy,
                    price: Price::from_prob(0.5),
                    size: Size::from_usdc(100.0),
                },
            )
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn record_fill_transitions_state() {
        let m = mk();
        let asset = [1; 32];
        let size = Size::from_usdc(100.0);
        let mut o = Order::new(asset, Side::Buy, Price::from_prob(0.5), size);
        let oid = o.id;
        o.state = OrderState::Acked;
        m.orders.insert(oid, o);
        m.record_fill(oid, Size::from_usdc(40.0));
        assert_eq!(m.orders.get(&oid).unwrap().state, OrderState::PartialFill);
        m.record_fill(oid, Size::from_usdc(60.0));
        assert_eq!(m.orders.get(&oid).unwrap().state, OrderState::Filled);
    }
}
