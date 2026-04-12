//! Web dashboard — axum HTTP server exposing JSON API + embedded SPA.
//!
//! Endpoints:
//!
//! * `GET  /`            — serves the HTML dashboard
//! * `GET  /api/status`  — top-level status (mode, btc, uptime, counters)
//! * `GET  /api/book`    — per-asset order book snapshots
//! * `GET  /api/orders`  — open / recent orders
//! * `GET  /api/latency` — pipeline latency percentiles
//! * `GET  /api/logs`    — recent log entries
//! * `POST /api/pause`   — toggle pause
//! * `POST /api/kill`    — trip the kill switch

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tower_http::cors::CorsLayer;
use tracing::info;

use sniper_executor::{LatencyStats, OrderManager};
use sniper_feed::MarketState;
use sniper_risk::RiskEngine;
use sniper_signal::EdgeEngine;

use crate::log_ring::LogRing;

// ── Shared state for handlers ───────────────────────────────────────────────

pub struct WebDeps {
    pub state: Arc<MarketState>,
    pub manager: Arc<OrderManager>,
    pub risk: Arc<RiskEngine>,
    pub stats: Arc<LatencyStats>,
    pub logs: LogRing,
    pub edge: EdgeEngine,
    pub started_at: Instant,
    pub maker_mode: bool,
    pub maker_half_spread_bps: i32,
    pub maker_quote_size_usdc: f64,
    pub maker_max_inventory_usdc: f64,
    pub dry_run: bool,
}

type AppState = Arc<WebDeps>;

// ── JSON response types ─────────────────────────────────────────────────────

#[derive(Serialize)]
struct StatusResponse {
    mode: &'static str,
    status: &'static str,
    dry_run: bool,
    uptime_secs: u64,
    btc_last: f64,
    btc_vwap_15s: f64,
    btc_vwap_60s: f64,
    btc_ema_fast: f64,
    btc_ema_slow: f64,
    btc_momentum: f64,
    btc_direction_prob: f64,
    btc_trades: u64,
    btc_source: String,
    ws_messages: u64,
    open_orders: usize,
    fills_total: u64,
    pnl_usdc: f64,
    daily_pnl_usdc: f64,
    open_positions: u64,
    max_positions: usize,
    paused: bool,
    killed: bool,
    // Strategy params
    min_edge_bps: i32,
    tx_cost_bps: i32,
    maker_half_spread_bps: i32,
    maker_quote_size_usdc: f64,
    maker_max_inventory_usdc: f64,
}

#[derive(Serialize)]
struct BookEntry {
    asset: String,
    bid_price: f64,
    bid_size: f64,
    ask_price: f64,
    ask_size: f64,
    mid: f64,
    spread_bps: i32,
    imbalance: f64,
    bid_depth: u32,
    ask_depth: u32,
}

#[derive(Serialize)]
struct OrderEntry {
    id: u64,
    asset: String,
    side: String,
    price: f64,
    size_usdc: f64,
    filled_usdc: f64,
    fill_pct: f64,
    state: String,
    clob_id: Option<String>,
}

#[derive(Serialize)]
struct LatencyEntry {
    stage: String,
    count: u64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    p999_us: u64,
    max_us: u64,
}

#[derive(Serialize)]
struct LogEntry {
    age_secs: u64,
    level: String,
    message: String,
}

#[derive(Serialize)]
struct ActionResponse {
    ok: bool,
    message: String,
}

// ── Handlers ────────────────────────────────────────────────────────────────

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Html(include_str!("web_ui.html")),
    )
}

async fn api_status(State(deps): State<AppState>) -> Json<StatusResponse> {
    let tape = deps.state.tape().snapshot();
    let source = tape
        .last_source
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|| "none".into());

    Json(StatusResponse {
        mode: if deps.maker_mode { "maker" } else { "taker" },
        status: if deps.risk.is_tripped() {
            "killed"
        } else if deps.manager.is_paused() {
            "paused"
        } else {
            "live"
        },
        dry_run: deps.dry_run,
        uptime_secs: deps.started_at.elapsed().as_secs(),
        btc_last: tape.last_price,
        btc_vwap_15s: tape.vwap_15s,
        btc_vwap_60s: tape.vwap_60s,
        btc_ema_fast: tape.ema_fast,
        btc_ema_slow: tape.ema_slow,
        btc_momentum: tape.momentum,
        btc_direction_prob: tape.directional_prob(),
        btc_trades: tape.trade_count,
        btc_source: source,
        ws_messages: deps.state.messages_total(),
        open_orders: deps.manager.open_count(),
        fills_total: deps.manager.fills_total(),
        pnl_usdc: deps.risk.realised_pnl_atoms() as f64 / 1_000_000.0,
        daily_pnl_usdc: deps.risk.daily_pnl_atoms() as f64 / 1_000_000.0,
        open_positions: deps.risk.open_positions(),
        max_positions: deps.risk.limits().max_open_positions,
        paused: deps.manager.is_paused(),
        killed: deps.risk.is_tripped(),
        min_edge_bps: deps.edge.params().min_edge_bps,
        tx_cost_bps: deps.edge.params().tx_cost_bps,
        maker_half_spread_bps: deps.maker_half_spread_bps,
        maker_quote_size_usdc: deps.maker_quote_size_usdc,
        maker_max_inventory_usdc: deps.maker_max_inventory_usdc,
    })
}

async fn api_book(State(deps): State<AppState>) -> Json<Vec<BookEntry>> {
    let mut entries = Vec::new();
    deps.state.for_each_book(|asset, book| {
        let snap = book.snapshot();
        entries.push(BookEntry {
            asset: hex_asset(asset),
            bid_price: snap.best_bid.map(|l| l.price.as_prob()).unwrap_or(0.0),
            bid_size: snap.best_bid.map(|l| l.size.as_usdc()).unwrap_or(0.0),
            ask_price: snap.best_ask.map(|l| l.price.as_prob()).unwrap_or(0.0),
            ask_size: snap.best_ask.map(|l| l.size.as_usdc()).unwrap_or(0.0),
            mid: snap.mid().map(|p| p.as_prob()).unwrap_or(0.0),
            spread_bps: snap.spread_bps().unwrap_or(0),
            imbalance: snap.imbalance(),
            bid_depth: snap.bid_depth,
            ask_depth: snap.ask_depth,
        });
    });
    Json(entries)
}

async fn api_orders(State(deps): State<AppState>) -> Json<Vec<OrderEntry>> {
    let orders = deps.manager.snapshot();
    let entries: Vec<OrderEntry> = orders
        .iter()
        .map(|o| OrderEntry {
            id: o.id.0,
            asset: hex_asset(o.asset),
            side: o.side.as_str().to_string(),
            price: o.price.as_prob(),
            size_usdc: o.size.as_usdc(),
            filled_usdc: o.filled.as_usdc(),
            fill_pct: if o.size.0 > 0 {
                o.filled.0 as f64 / o.size.0 as f64 * 100.0
            } else {
                0.0
            },
            state: format!("{:?}", o.state),
            clob_id: o.clob_id.clone(),
        })
        .collect();
    Json(entries)
}

async fn api_latency(State(deps): State<AppState>) -> Json<Vec<LatencyEntry>> {
    let snap = deps.stats.snapshot();
    let stages = [
        ("ws_parse", snap.ws_parse),
        ("signal_eval", snap.signal_eval),
        ("order_fired", snap.order_fired),
        ("order_acked", snap.order_acked),
    ];
    let entries: Vec<LatencyEntry> = stages
        .into_iter()
        .map(|(name, r)| LatencyEntry {
            stage: name.into(),
            count: r.count,
            p50_us: r.p50_ns / 1_000,
            p95_us: r.p95_ns / 1_000,
            p99_us: r.p99_ns / 1_000,
            p999_us: r.p999_ns / 1_000,
            max_us: r.max_ns / 1_000,
        })
        .collect();
    Json(entries)
}

async fn api_logs(State(deps): State<AppState>) -> Json<Vec<LogEntry>> {
    let entries = deps.logs.snapshot();
    let take = entries.len().saturating_sub(100);
    let logs: Vec<LogEntry> = entries[take..]
        .iter()
        .map(|e| LogEntry {
            age_secs: e.ts.elapsed().as_secs(),
            level: e.level.clone(),
            message: e.message.clone(),
        })
        .collect();
    Json(logs)
}

async fn api_pause(State(deps): State<AppState>) -> Json<ActionResponse> {
    let new_state = !deps.manager.is_paused();
    deps.manager.set_paused(new_state);
    deps.logs.push(
        "INFO",
        if new_state {
            "executor PAUSED via web"
        } else {
            "executor RESUMED via web"
        },
    );
    Json(ActionResponse {
        ok: true,
        message: format!("paused={new_state}"),
    })
}

async fn api_kill(State(deps): State<AppState>) -> Json<ActionResponse> {
    deps.risk.trip("operator kill via web dashboard");
    deps.logs
        .push("WARN", "KILL SWITCH latched via web dashboard");
    Json(ActionResponse {
        ok: true,
        message: "kill switch latched".into(),
    })
}

// ── Router ──────────────────────────────────────────────────────────────────

pub async fn run_web_dashboard(deps: WebDeps, bind: &str) -> anyhow::Result<()> {
    let state: AppState = Arc::new(deps);

    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(api_status))
        .route("/api/book", get(api_book))
        .route("/api/orders", get(api_orders))
        .route("/api/latency", get(api_latency))
        .route("/api/logs", get(api_logs))
        .route("/api/pause", post(api_pause))
        .route("/api/kill", post(api_kill))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!(bind, "web dashboard listening");
    axum::serve(listener, app).await?;
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn hex_asset(asset: [u8; 32]) -> String {
    let mut s = String::with_capacity(12);
    for &b in &asset[..6] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
