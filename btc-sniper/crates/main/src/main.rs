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

use std::collections::HashSet;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use anyhow::{anyhow, Result};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use sniper_dashboard::{run_dashboard, DashboardDeps, LogRing};
use sniper_executor::{
    ClobClient, LatencyStage, LatencyStats, OrderManager, OrderManagerConfig, OrderTemplate,
};
use sniper_feed::{
    discover_btc_markets, AssetId, BinanceFeed, CoinbaseFeed,
    MarketState, PolymarketFeed, Side as FeedSide,
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


// ---------- signal loop ------------------------------------------------------

/// Shared list of assets the signal loop evaluates. Updated by the
/// discovery loop, read every 5ms by the signal loop.
type ActiveAssets = Arc<RwLock<Vec<AssetId>>>;

async fn signal_loop(
    state: Arc<MarketState>,
    manager: Arc<OrderManager>,
    risk: Arc<RiskEngine>,
    engine: EdgeEngine,
    stats: Arc<LatencyStats>,
    active_assets: ActiveAssets,
    shutdown: watch::Receiver<bool>,
) {
    let mut tick = interval(Duration::from_millis(5));
    let mut shutdown = shutdown;
    let mut last_status = Instant::now();
    let mut eval_count: u64 = 0;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
        }
        let tape = state.tape().snapshot();
        eval_count += 1;

        let assets = active_assets.read().clone();

        if last_status.elapsed() > Duration::from_secs(10) {
            let msgs = state.messages_total();
            info!(
                evals = eval_count,
                ws_messages = msgs,
                btc_trades = tape.trade_count,
                btc_price = format_args!("{:.2}", tape.last_price),
                momentum = format_args!("{:.4}", tape.momentum),
                active_markets = assets.len(),
                "signal loop status"
            );
            last_status = Instant::now();
        }

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

// ---------- market discovery loop --------------------------------------------

async fn discovery_loop(
    http: reqwest::Client,
    gamma_base: String,
    poly_ws_url: String,
    state: Arc<MarketState>,
    active_assets: ActiveAssets,
    asset_index: Arc<AssetIndex>,
    manager: Arc<OrderManager>,
    owner: String,
    shutdown: watch::Receiver<bool>,
) {
    let mut known: HashSet<AssetId> = HashSet::new();
    let mut feed_handle: Option<JoinHandle<()>> = None;
    let mut shutdown = shutdown;

    loop {
        // Check shutdown
        if *shutdown.borrow() {
            if let Some(h) = feed_handle.take() {
                h.abort();
            }
            return;
        }

        let markets = discover_btc_markets(&http, &gamma_base).await;
        let mut new_yes_tokens: Vec<AssetId> = Vec::new();
        let mut changed = false;

        for dm in &markets {
            if !dm.accepting_orders {
                continue;
            }
            let token = dm.yes_token;
            new_yes_tokens.push(token);

            if known.insert(token) {
                changed = true;
                info!(
                    slug = %dm.slug,
                    question = %dm.question,
                    "new market discovered"
                );

                // Register in asset index + order templates
                asset_index.insert(token, dm.condition_id, true);
                asset_index.insert(dm.no_token, dm.condition_id, false);
                for side in [sniper_feed::Side::Buy, sniper_feed::Side::Sell] {
                    manager.register_template(OrderTemplate {
                        asset: token,
                        side,
                        maker: owner.clone(),
                        signer: owner.clone(),
                        taker: "0x0000000000000000000000000000000000000000".into(),
                        nonce: 1,
                        expiration_secs: 0,
                        fee_rate_bps: 1000, // Crypto category per CLOB API
                    });
                }
            }
        }

        // Prune tokens no longer in the active set
        let active_set: HashSet<AssetId> = new_yes_tokens.iter().copied().collect();
        let removed: Vec<AssetId> = known.difference(&active_set).copied().collect();
        for r in &removed {
            known.remove(r);
            changed = true;
            debug!("market expired, removing token");
        }

        if changed && !new_yes_tokens.is_empty() {
            // Update the shared asset list for the signal loop
            *active_assets.write() = new_yes_tokens.clone();

            // Restart the Polymarket WS feed with the new asset set
            if let Some(h) = feed_handle.take() {
                h.abort();
            }
            match PolymarketFeed::new(&poly_ws_url, new_yes_tokens, state.clone()) {
                Ok(feed) => {
                    let feed = Arc::new(feed);
                    feed_handle = Some(tokio::spawn({
                        let f = feed.clone();
                        async move {
                            if let Err(e) = f.run_forever().await {
                                error!(error = %e, "polymarket feed exited");
                            }
                        }
                    }));
                    info!(
                        markets = active_assets.read().len(),
                        "polymarket feed restarted with updated market set"
                    );
                }
                Err(e) => {
                    error!(error = %e, "failed to create polymarket feed");
                }
            }
        } else if new_yes_tokens.is_empty() && feed_handle.is_some() {
            // No active markets — drop the feed
            if let Some(h) = feed_handle.take() {
                h.abort();
            }
            *active_assets.write() = Vec::new();
            info!("no active BTC markets — feed stopped");
        }

        // Poll every 30 seconds
        tokio::select! {
            _ = sleep(Duration::from_secs(30)) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    if let Some(h) = feed_handle.take() {
                        h.abort();
                    }
                    return;
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

    // --- shared active-market list (updated by discovery loop) -------------
    let active_assets: ActiveAssets = Arc::new(RwLock::new(Vec::new()));
    let asset_index = Arc::new(AssetIndex::new());

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

    // Templates are registered dynamically by the discovery loop.

    // --- signal engine ----------------------------------------------------
    let sizer = PositionSizer::new(
        env_parse::<f64>("BANKROLL_USDC", 10_000.0),
        env_parse::<f64>("MAX_POSITION_USDC", 500.0),
        env_parse::<f64>("KELLY_FRACTION", 0.15),
    );
    let edge = EdgeEngine::new(
        EdgeParams {
            min_edge_bps: env_parse("MIN_EDGE_BPS", 500i32),
            tx_cost_bps: env_parse("TX_COST_BPS", 360i32),
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

    // --- spawn BTC reference feeds ------------------------------------------
    // Coinbase is the primary feed (works on AWS; Binance blocks cloud IPs).
    // Both feeds write to the same BtcTape, so whichever connects first wins.
    let poly_url = env_str(
        "POLYMARKET_WS",
        "wss://ws-subscriptions-clob.polymarket.com/ws/market",
    );
    let coinbase_url = env_str("COINBASE_WS", "wss://advanced-trade-ws.coinbase.com");
    let binance_url = env_str("BINANCE_WS", "wss://stream.binance.com:9443/ws/btcusdt@trade");
    let gamma_base = env_str("GAMMA_API", "https://gamma-api.polymarket.com");

    let coinbase = Arc::new(CoinbaseFeed::new(&coinbase_url, state.clone())?);
    tokio::spawn({
        let c = coinbase.clone();
        async move {
            if let Err(e) = c.run_forever().await {
                error!(error = %e, "coinbase feed exited");
            }
        }
    });

    let binance = Arc::new(BinanceFeed::new(&binance_url, state.clone())?);
    tokio::spawn({
        let b = binance.clone();
        async move {
            if let Err(e) = b.run_forever().await {
                error!(error = %e, "binance feed exited");
            }
        }
    });

    // --- market discovery loop (manages Polymarket WS lifecycle) ----------
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    tokio::spawn(discovery_loop(
        http,
        gamma_base,
        poly_url,
        state.clone(),
        active_assets.clone(),
        asset_index.clone(),
        manager.clone(),
        owner.clone(),
        shutdown_rx.clone(),
    ));

    // --- signal loop ------------------------------------------------------
    let sig_handle = tokio::spawn(signal_loop(
        state.clone(),
        manager.clone(),
        risk.clone(),
        edge.clone(),
        stats.clone(),
        active_assets.clone(),
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
