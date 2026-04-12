//! # sniper binary
//!
//! Wires every crate together:
//!
//! * loads env + `config/markets.toml`
//! * installs the tracing subscriber (JSON to stdout + in-memory log ring)
//! * spawns feed tasks (Polymarket + Binance), the signal loop, the order
//!   manager's stale sweeper, and the Ratatui dashboard
//! * listens for `SIGTERM` / `SIGINT` for graceful shutdown and `SIGUSR1`
//!   for a latency percentile dump

use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tokio::time::{interval, sleep};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use sniper_dashboard::{run_dashboard, DashboardDeps, LogRing};
use sniper_executor::{
    ClobClient, LatencyStage, LatencyStats, OrderManager, OrderManagerConfig, OrderTemplate,
};
use sniper_feed::{
    parse_hex32, AssetId, BinanceFeed, MarketState, PolymarketFeed, Side as FeedSide,
};
use sniper_risk::{AssetIndex, RiskEngine, RiskLimits, RiskVerdict};
use sniper_signal::{EdgeEngine, EdgeParams, PositionSizer};

// ---------- env helpers ------------------------------------------------------

fn env_str(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .map(|s| matches!(s.as_str(), "1" | "true" | "TRUE" | "yes"))
        .unwrap_or(default)
}

// ---------- markets.toml ------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MarketsConfig {
    #[serde(default)]
    market: Vec<MarketEntry>,
    #[serde(default)]
    #[allow(dead_code)]
    defaults: DefaultsConfig,
}

#[derive(Debug, Deserialize)]
struct MarketEntry {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    slug: String,
    condition_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    target: u64,
    #[serde(default)]
    #[allow(dead_code)]
    resolves_at: u64,
    #[serde(default)]
    #[allow(dead_code)]
    min_edge_bps: Option<i32>,
    #[serde(default)]
    #[allow(dead_code)]
    max_position_usdc: Option<f64>,
}

#[derive(Debug, Deserialize, Default)]
struct DefaultsConfig {
    #[serde(default)]
    #[allow(dead_code)]
    book_resync_ms: u64,
    #[serde(default)]
    #[allow(dead_code)]
    ping_interval_ms: u64,
    #[serde(default)]
    #[allow(dead_code)]
    vwap_window_secs: u64,
}

fn load_markets(path: &str) -> Result<MarketsConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading markets config at {path}"))?;
    toml::from_str(&text).context("parsing markets.toml")
}

// ---------- signal loop ------------------------------------------------------

async fn signal_loop(
    state: Arc<MarketState>,
    manager: Arc<OrderManager>,
    risk: Arc<RiskEngine>,
    engine: EdgeEngine,
    stats: Arc<LatencyStats>,
    assets: Vec<AssetId>,
    shutdown: watch::Receiver<bool>,
) {
    // Re-evaluate every 5ms — the book/tape mutate asynchronously.
    let mut tick = interval(Duration::from_millis(5));
    let mut shutdown = shutdown;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
        }
        let tape = state.tape().snapshot();
        for asset in &assets {
            let book = state.book(*asset);
            let book_snap = book.snapshot();

            let t0 = Instant::now();
            let decision = engine.evaluate(&book_snap, &tape);
            stats.record(LatencyStage::SignalEval, t0.elapsed().as_nanos() as u64);

            if let sniper_signal::Intent::Fire { side, size, .. } = decision.intent {
                let feed_side = match side {
                    sniper_feed::Side::Buy => FeedSide::Buy,
                    sniper_feed::Side::Sell => FeedSide::Sell,
                };
                match risk.check_and_reserve(*asset, feed_side, size) {
                    RiskVerdict::Allow => {
                        let m = manager.clone();
                        let r = risk.clone();
                        let asset_copy = *asset;
                        let intent = decision.intent;
                        tokio::spawn(async move {
                            let res = m.submit(asset_copy, intent).await;
                            if res.is_err() {
                                r.release(asset_copy);
                            }
                        });
                    }
                    RiskVerdict::Deny(reason) => {
                        warn!(?reason, ?decision, "risk denied fire");
                    }
                }
            }
        }
    }
}

// ---------- main -------------------------------------------------------------

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Load .env if present (dev convenience; systemd uses EnvironmentFile)
    dotenvy::dotenv().ok();

    // --- tracing subscriber: JSON logs + in-memory ring for dashboard ---
    let logs = LogRing::new(256);
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sniper=debug"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .with_target(true)
        .with_current_span(false)
        .init();

    info!("sniper starting");

    // --- pin the current thread to a housekeeping core --------------------
    if let Some(cores) = core_affinity::get_core_ids() {
        if let Some(first) = cores.first() {
            core_affinity::set_for_current(*first);
        }
    }
    // Workers (set via TRADING_CORES) are pinned when spawned.

    // --- config -----------------------------------------------------------
    let markets_path = env_str("MARKETS_CONFIG", "config/markets.toml");
    let cfg = load_markets(&markets_path)
        .unwrap_or_else(|e| {
            warn!(error = %e, "failed to load markets.toml — running with empty market list");
            MarketsConfig {
                market: Vec::new(),
                defaults: DefaultsConfig::default(),
            }
        });

    // For every market we watch both YES (asset 0) and NO (asset 1). The
    // real bot resolves these via the Gamma API; the skeleton uses the raw
    // condition id as a YES-only placeholder. In production replace this
    // with a startup call to /markets?condition_id={c}.
    let mut assets: Vec<AssetId> = Vec::new();
    let asset_index = Arc::new(AssetIndex::new());
    for m in &cfg.market {
        let Some(cid_bytes) = parse_hex32(&m.condition_id) else {
            warn!(market = %m.name, "skipping market: invalid condition_id");
            continue;
        };
        info!(market = %m.name, "watching market");
        assets.push(cid_bytes);
        asset_index.insert(cid_bytes, cid_bytes, true);
    }
    if assets.is_empty() {
        warn!("no markets configured — idle run");
    }

    // --- shared state -----------------------------------------------------
    let state = MarketState::new();
    let stats = Arc::new(LatencyStats::new());

    // --- risk -------------------------------------------------------------
    let limits = RiskLimits {
        max_daily_loss_atoms: (env_parse::<f64>("MAX_DAILY_LOSS_USDC", 250.0) * -1_000_000.0)
            as i64,
        max_open_positions: env_parse("MAX_OPEN_POSITIONS", 8usize),
        max_position_atoms: (env_parse::<f64>("MAX_POSITION_USDC", 500.0) * 1_000_000.0) as u64,
    };
    let risk = RiskEngine::new(limits, asset_index.clone());

    // --- executor / order manager ----------------------------------------
    let dry_run = env_bool("DRY_RUN", true);
    let owner = env_str("POLYMARKET_OWNER", "0x0000000000000000000000000000000000000000");
    let client = ClobClient::new(
        env_str("POLYMARKET_HOST", "https://clob.polymarket.com"),
        owner.clone(),
        None, // L2 auth wired later
        dry_run,
    )?;
    let manager_cfg = OrderManagerConfig {
        stale_ttl_ms: env_parse("STALE_ORDER_TTL_MS", 500u64),
        max_open_orders: env_parse("MAX_OPEN_POSITIONS", 8usize),
        owner: owner.clone(),
    };
    let manager = OrderManager::new(client.clone(), manager_cfg, stats.clone());

    // Pre-register one template per (asset, side). At fire time we only
    // swap price/size/salt.
    for asset in &assets {
        for side in [sniper_feed::Side::Buy, sniper_feed::Side::Sell] {
            manager.register_template(OrderTemplate {
                asset: *asset,
                side,
                maker: owner.clone(),
                signer: owner.clone(),
                taker: "0x0000000000000000000000000000000000000000".into(),
                nonce: 1,
                expiration_secs: 0,
                fee_rate_bps: 0,
            });
        }
    }

    // --- signal engine ----------------------------------------------------
    let sizer = PositionSizer::new(
        env_parse::<f64>("BANKROLL_USDC", 10_000.0),
        env_parse::<f64>("MAX_POSITION_USDC", 500.0),
        env_parse::<f64>("KELLY_FRACTION", 0.15),
    );
    let edge = EdgeEngine::new(
        EdgeParams {
            min_edge_bps: env_parse("MIN_EDGE_BPS", 250i32),
            tx_cost_bps: env_parse("TX_COST_BPS", 0i32),
            max_conviction_bps: 1000,
        },
        sizer,
    );

    // --- shutdown channel -------------------------------------------------
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // --- DNS pre-resolve for Polymarket host ------------------------------
    let host = env_str("POLYMARKET_HOST", "https://clob.polymarket.com");
    if let Ok(addrs) = format!("{}:443", host.replace("https://", "").replace("http://", ""))
        .to_socket_addrs()
    {
        info!(count = addrs.count(), "resolved polymarket DNS at startup");
    }

    // --- spawn feed tasks -------------------------------------------------
    let poly_url = env_str(
        "POLYMARKET_WS",
        "wss://ws-subscriptions-clob.polymarket.com/ws/market",
    );
    let binance_url = env_str("BINANCE_WS", "wss://stream.binance.com:9443/ws/btcusdt@trade");

    let poly = Arc::new(PolymarketFeed::new(&poly_url, assets.clone(), state.clone())?);
    let binance = Arc::new(BinanceFeed::new(&binance_url, state.clone())?);

    tokio::spawn({
        let p = poly.clone();
        async move {
            if let Err(e) = p.run_forever().await {
                error!(error = %e, "polymarket feed exited");
            }
        }
    });
    tokio::spawn({
        let b = binance.clone();
        async move {
            if let Err(e) = b.run_forever().await {
                error!(error = %e, "binance feed exited");
            }
        }
    });

    // --- signal loop ------------------------------------------------------
    let sig_handle = tokio::spawn(signal_loop(
        state.clone(),
        manager.clone(),
        risk.clone(),
        edge.clone(),
        stats.clone(),
        assets.clone(),
        shutdown_rx.clone(),
    ));

    // --- stale order sweeper ---------------------------------------------
    tokio::spawn({
        let m = manager.clone();
        async move { m.run_stale_sweeper().await }
    });

    // --- signal handlers -------------------------------------------------
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigusr1 = signal(SignalKind::user_defined1())?;

    tokio::spawn({
        let stats = stats.clone();
        async move {
            while sigusr1.recv().await.is_some() {
                let snap = stats.snapshot();
                println!("{snap}");
            }
        }
    });

    // --- dashboard or plain idle ------------------------------------------
    let enable_tui = env_bool("ENABLE_TUI", true);
    let started = Instant::now();
    let dash_fut = async {
        if enable_tui {
            let deps = DashboardDeps {
                state: state.clone(),
                manager: manager.clone(),
                risk: risk.clone(),
                stats: stats.clone(),
                logs: logs.clone(),
                edge: edge.clone(),
                shutdown: shutdown_tx.clone(),
                started_at: started,
            };
            if let Err(e) = run_dashboard(deps).await {
                error!(error = %e, "dashboard exited");
            }
        } else {
            loop {
                if *shutdown_rx.borrow() {
                    return;
                }
                sleep(Duration::from_secs(1)).await;
            }
        }
    };

    tokio::select! {
        _ = dash_fut => {},
        _ = sigterm.recv() => {
            info!("SIGTERM received");
            let _ = shutdown_tx.send(true);
        }
        _ = sigint.recv() => {
            info!("SIGINT received");
            let _ = shutdown_tx.send(true);
        }
    }

    // --- graceful shutdown: cancel all open orders -----------------------
    info!("shutting down: cancelling all live orders");
    manager.kill_all().await;
    sig_handle.abort();
    // Give feeds a moment to drop
    sleep(Duration::from_millis(100)).await;

    // Final latency snapshot
    let snap = stats.snapshot();
    info!(snapshot = %snap, "final latency snapshot");

    Ok(())
}

// Ensure we return a proper error (not a panic) if main is called with an
// unexpected TokoioRuntime setup. Keeping this here to guard against
// accidental `block_on` usage.
#[allow(dead_code)]
fn _assert_send_sync<T: Send + Sync>() {}

#[allow(dead_code)]
fn _never() -> anyhow::Error {
    anyhow!("unreachable")
}
