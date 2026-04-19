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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;

use anyhow::{anyhow, Result};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use sniper_dashboard::{run_dashboard, run_web_dashboard, DashboardDeps, LogRing, WebDeps};
use sniper_executor::{
    ClobClient, LatencyStage, LatencyStats, OrderManager, OrderManagerConfig, OrderTemplate,
};
use sniper_feed::{
    discover_btc_markets, AssetId, BinanceFeed, CoinbaseFeed,
    MarketState, PolymarketFeed, Side as FeedSide,
};
use sniper_risk::{AssetIndex, RiskEngine, RiskLimits, RiskVerdict};
use sniper_signal::{EdgeEngine, EdgeParams, MarketMaker, MakerParams, PositionSizer, QuoteAction};

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

// ---------- maker loop -------------------------------------------------------

/// Per-asset state tracked by the maker loop.
struct AssetQuoteState {
    fair_price: Option<sniper_feed::Price>,
    inventory_net_atoms: i64,
}

/// Market-making loop: continuously quotes both sides of each active market.
/// Replaces the sniper signal_loop when MAKER_MODE=true.
async fn maker_loop(
    state: Arc<MarketState>,
    manager: Arc<OrderManager>,
    mm: MarketMaker,
    stats: Arc<LatencyStats>,
    active_assets: ActiveAssets,
    mut shutdown: watch::Receiver<bool>,
) {
    use std::collections::HashMap;

    let mut tick = interval(Duration::from_millis(50)); // 50ms quote refresh
    let mut asset_states: HashMap<AssetId, AssetQuoteState> = HashMap::new();
    let mut last_status = Instant::now();
    let mut eval_count: u64 = 0;
    let mut requote_count: u64 = 0;

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
        }

        let tape = state.tape().snapshot();
        let assets = active_assets.read().clone();
        eval_count += 1;

        if last_status.elapsed() > Duration::from_secs(10) {
            info!(
                evals = eval_count,
                requotes = requote_count,
                btc_price = format_args!("{:.2}", tape.last_price),
                momentum = format_args!("{:.4}", tape.momentum),
                active_markets = assets.len(),
                open_orders = manager.open_count(),
                "maker loop status"
            );
            last_status = Instant::now();
        }

        for asset in &assets {
            let book = state.book(*asset);
            let book_snap = book.snapshot();

            let astate = asset_states.entry(*asset).or_insert(AssetQuoteState {
                fair_price: None,
                inventory_net_atoms: 0,
            });

            // If we have no open orders, force a fresh quote by clearing
            // the cached fair price so the maker engine won't return Hold.
            if manager.open_count() == 0 {
                astate.fair_price = None;
            }

            let t0 = Instant::now();
            let action = mm.evaluate(
                &book_snap,
                &tape,
                astate.inventory_net_atoms,
                astate.fair_price,
            );
            stats.record(LatencyStage::SignalEval, t0.elapsed().as_nanos() as u64);

            match action {
                QuoteAction::Hold => {}
                QuoteAction::CancelAll => {
                    let m = manager.clone();
                    let a = *asset;
                    tokio::spawn(async move { m.cancel_for_asset(a).await; });
                    astate.fair_price = None;
                }
                QuoteAction::Requote(quote) => {
                    requote_count += 1;
                    astate.fair_price = Some(quote.fair_price);

                    let m = manager.clone();
                    let a = *asset;
                    let stats2 = stats.clone();
                    tokio::spawn(async move {
                        // Cancel existing quotes for this asset
                        m.cancel_for_asset(a).await;

                        // Post new bid
                        if let Some(leg) = quote.bid {
                            let t0 = Instant::now();
                            match m.submit_limit(
                                a,
                                sniper_feed::Side::Buy,
                                leg.price,
                                leg.size,
                            ).await {
                                Ok(_) => {
                                    stats2.record(LatencyStage::OrderFired, t0.elapsed().as_nanos() as u64);
                                    info!(
                                        side = "BUY",
                                        price = leg.price.as_prob(),
                                        size_usdc = leg.size.as_usdc(),
                                        fair = quote.fair_price.as_prob(),
                                        "maker order posted"
                                    );
                                }
                                Err(e) => warn!(error = %e, "maker bid failed"),
                            }
                        }

                        // Post new ask
                        if let Some(leg) = quote.ask {
                            let t0 = Instant::now();
                            match m.submit_limit(
                                a,
                                sniper_feed::Side::Sell,
                                leg.price,
                                leg.size,
                            ).await {
                                Ok(_) => {
                                    stats2.record(LatencyStage::OrderFired, t0.elapsed().as_nanos() as u64);
                                    info!(
                                        side = "SELL",
                                        price = leg.price.as_prob(),
                                        size_usdc = leg.size.as_usdc(),
                                        fair = quote.fair_price.as_prob(),
                                        "maker order posted"
                                    );
                                }
                                Err(e) => warn!(error = %e, "maker ask failed"),
                            }
                        }
                    });
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
                        nonce: 0,
                        expiration_secs: 0,
                        fee_rate_bps: 0, // Maker orders: 0% fee
                        neg_risk: dm.neg_risk,
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

// ---------- derive-api-key subcommand ----------------------------------------

/// Derive Polymarket L2 CLOB API credentials from wallet private key.
///
/// Calls POST /auth/derive-api-key with an EIP-712 signed message proving
/// wallet ownership. Prints the resulting credentials to stdout and optionally
/// appends them to .env.
async fn derive_api_key() -> Result<()> {
    use ethers::abi::encode;
    use ethers::types::{Address, U256};
    use ethers::utils::keccak256;
    use k256::ecdsa::{SigningKey, VerifyingKey};

    let key_hex = std::env::var("POLYMARKET_PRIVATE_KEY")
        .map_err(|_| anyhow!("POLYMARKET_PRIVATE_KEY not set in .env"))?;
    let key_hex = key_hex.strip_prefix("0x").unwrap_or(&key_hex);
    if key_hex.chars().all(|c| c == '0') {
        return Err(anyhow!(
            "POLYMARKET_PRIVATE_KEY is the zero placeholder — set your real key in .env"
        ));
    }
    let key_bytes = hex::decode(key_hex).map_err(|e| anyhow!("bad hex in private key: {}", e))?;
    let sk = SigningKey::from_slice(&key_bytes).map_err(|e| anyhow!("invalid key: {}", e))?;

    // Derive wallet address from signing key
    let vk = VerifyingKey::from(&sk);
    let pk = vk.to_encoded_point(false);
    let hash = keccak256(&pk.as_bytes()[1..]);
    let address = format!("0x{}", hex::encode(&hash[12..]));
    eprintln!("Wallet address: {}", address);

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let ts_str = timestamp.to_string();
    let nonce: u64 = 0;

    // EIP-712 domain separator for ClobAuthDomain
    let domain_type = "EIP712Domain(string name,string version,uint256 chainId)";
    let domain_sep = keccak256(encode(&[
        ethers::abi::Token::FixedBytes(keccak256(domain_type).to_vec()),
        ethers::abi::Token::FixedBytes(keccak256("ClobAuthDomain").to_vec()),
        ethers::abi::Token::FixedBytes(keccak256("1").to_vec()),
        ethers::abi::Token::Uint(U256::from(137u64)),
    ]));

    // EIP-712 struct hash for ClobAuth
    let struct_type =
        "ClobAuth(address address,string timestamp,uint256 nonce,string message)";
    let addr: Address = address.parse().map_err(|_| anyhow!("bad derived address"))?;
    let struct_hash = keccak256(encode(&[
        ethers::abi::Token::FixedBytes(keccak256(struct_type).to_vec()),
        ethers::abi::Token::Address(addr),
        ethers::abi::Token::FixedBytes(keccak256(ts_str.as_bytes()).to_vec()),
        ethers::abi::Token::Uint(U256::from(nonce)),
        ethers::abi::Token::FixedBytes(keccak256(b"").to_vec()),
    ]));

    // EIP-712 digest: \x19\x01 + domainSep + structHash
    let mut digest_input = Vec::with_capacity(66);
    digest_input.extend_from_slice(b"\x19\x01");
    digest_input.extend_from_slice(&domain_sep);
    digest_input.extend_from_slice(&struct_hash);
    let digest = keccak256(&digest_input);

    // Sign with k256 (same pattern as order.rs)
    let (ecdsa_sig, rec_id) = sk
        .sign_prehash_recoverable(&digest)
        .map_err(|e| anyhow!("signing failed: {}", e))?;
    let (r_bytes, s_bytes) = ecdsa_sig.split_bytes();
    let v = u8::from(rec_id) + 27;
    let mut sig_bytes = Vec::with_capacity(65);
    sig_bytes.extend_from_slice(r_bytes.as_ref());
    sig_bytes.extend_from_slice(s_bytes.as_ref());
    sig_bytes.push(v);
    let sig_hex = format!("0x{}", hex::encode(&sig_bytes));

    // POST to CLOB
    let host = env_str("POLYMARKET_HOST", "https://clob.polymarket.com");
    let url = format!("{}/auth/derive-api-key", host);
    eprintln!("Requesting L2 API key from {} ...", url);

    let body = serde_json::json!({
        "address": address,
        "timestamp": ts_str,
        "nonce": nonce,
        "message": "",
        "signature": sig_hex,
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("HTTP request failed: {}", e))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("CLOB returned {}: {}", status, text));
    }

    let creds: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| anyhow!("bad JSON response: {}: {}", e, text))?;

    let api_key = creds["apiKey"].as_str().unwrap_or("");
    let api_secret = creds["secret"].as_str().unwrap_or("");
    let api_passphrase = creds["passphrase"].as_str().unwrap_or("");

    if api_key.is_empty() {
        return Err(anyhow!("empty apiKey in response: {}", text));
    }

    println!();
    println!("=== Polymarket L2 API Credentials ===");
    println!("CLOB_API_KEY={}", api_key);
    println!("CLOB_SECRET={}", api_secret);
    println!("CLOB_PASSPHRASE={}", api_passphrase);
    println!();

    // Append to .env if it exists
    let env_path = std::path::Path::new(".env");
    if env_path.exists() {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(env_path)?;
        writeln!(f)?;
        writeln!(f, "# L2 credentials (derived {})", ts_str)?;
        writeln!(f, "CLOB_API_KEY={}", api_key)?;
        writeln!(f, "CLOB_SECRET={}", api_secret)?;
        writeln!(f, "CLOB_PASSPHRASE={}", api_passphrase)?;
        writeln!(f, "POLYMARKET_OWNER={}", address)?;
        eprintln!("Appended credentials to .env");
    } else {
        eprintln!("No .env file found — copy the values above manually.");
    }

    Ok(())
}

// ---------- main -------------------------------------------------------------

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Load .env if present (dev convenience; systemd uses EnvironmentFile)
    dotenvy::dotenv().ok();

    // --- subcommands (run before tracing init) --------------------------------
    if std::env::args().nth(1).as_deref() == Some("derive-api-key") {
        return derive_api_key().await;
    }

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
    let proxy_url = std::env::var("CLOB_PROXY").ok();
    let l2_auth = {
        let key = std::env::var("CLOB_API_KEY").ok();
        let secret = std::env::var("CLOB_SECRET").ok();
        let pass = std::env::var("CLOB_PASSPHRASE").ok();
        match (key, secret, pass) {
            (Some(k), Some(s), Some(p)) if !k.is_empty() => {
                info!("L2 auth configured (API key present)");
                Some(sniper_executor::L2Auth {
                    api_key: k,
                    api_secret: s,
                    api_passphrase: p,
                })
            }
            _ => {
                warn!("L2 auth NOT configured — orders will be rejected by CLOB");
                None
            }
        }
    };
    let client = ClobClient::new(
        env_str("POLYMARKET_HOST", "https://clob.polymarket.com"),
        owner.clone(),
        l2_auth,
        dry_run,
        proxy_url.as_deref(),
    )?;
    let manager_cfg = OrderManagerConfig {
        stale_ttl_ms: env_parse("STALE_ORDER_TTL_MS", 500u64),
        max_open_orders: env_parse("MAX_OPEN_POSITIONS", 8usize),
        owner: owner.clone(),
    };
    // --- EIP-712 signing key (for real order signatures) --------------------
    let signing_key = {
        let key_hex = std::env::var("POLYMARKET_PRIVATE_KEY").ok();
        match key_hex {
            Some(h)
                if !h.is_empty()
                    && h != "0x0000000000000000000000000000000000000000000000000000000000000000" =>
            {
                let h = h.strip_prefix("0x").unwrap_or(&h);
                let key_bytes =
                    hex::decode(h).map_err(|e| anyhow!("decode private key hex: {}", e))?;
                let sk = sniper_executor::SigningKey::from_slice(&key_bytes)
                    .map_err(|e| anyhow!("invalid signing key: {}", e))?;
                info!("EIP-712 signing key loaded");
                Some(Arc::new(sk))
            }
            _ => {
                warn!("POLYMARKET_PRIVATE_KEY not set — orders will use stub signatures");
                None
            }
        }
    };

    // Verify signing key matches POLYMARKET_OWNER
    if let Some(ref sk) = signing_key {
        use k256::ecdsa::VerifyingKey;
        let vk = VerifyingKey::from(sk.as_ref());
        let pk = vk.to_encoded_point(false);
        let hash = ethers::utils::keccak256(&pk.as_bytes()[1..]);
        let derived = format!("0x{}", hex::encode(&hash[12..]));
        info!(derived_address = %derived, configured_owner = %owner, "wallet address check");
        if derived.to_lowercase() != owner.to_lowercase() {
            warn!("POLYMARKET_OWNER does not match signing key! Orders will be rejected.");
        }
    }

    let manager = OrderManager::new(client.clone(), manager_cfg, stats.clone(), signing_key);

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
        http.clone(),
        gamma_base,
        poly_url,
        state.clone(),
        active_assets.clone(),
        asset_index.clone(),
        manager.clone(),
        owner.clone(),
        shutdown_rx.clone(),
    ));

    // --- maker or taker mode ------------------------------------------------
    let maker_mode = env_bool("MAKER_MODE", false);

    let sig_handle = if maker_mode {
        let mm = MarketMaker::new(MakerParams {
            half_spread_bps: env_parse("HALF_SPREAD_BPS", 150i32),
            quote_size_atoms: (env_parse::<f64>("QUOTE_SIZE_USDC", 100.0) * 1_000_000.0) as u64,
            requote_threshold_bps: env_parse("REQUOTE_THRESHOLD_BPS", 50i32),
            max_inventory_atoms: (env_parse::<f64>("MAX_INVENTORY_USDC", 500.0) * 1_000_000.0)
                as u64,
            inventory_skew_bps_per_100usdc: env_parse("INVENTORY_SKEW_BPS", 30i32),
            max_conviction_bps: env_parse("MAX_CONVICTION_BPS", 1000i32),
        });
        info!(
            half_spread_bps = mm.params().half_spread_bps,
            quote_size = mm.params().quote_size_atoms,
            requote_threshold = mm.params().requote_threshold_bps,
            max_inventory = mm.params().max_inventory_atoms,
            "MAKER MODE enabled"
        );
        tokio::spawn(maker_loop(
            state.clone(),
            manager.clone(),
            mm,
            stats.clone(),
            active_assets.clone(),
            shutdown_rx.clone(),
        ))
    } else {
        info!("TAKER (sniper) mode enabled");
        tokio::spawn(signal_loop(
            state.clone(),
            manager.clone(),
            risk.clone(),
            edge.clone(),
            stats.clone(),
            active_assets.clone(),
            shutdown_rx.clone(),
        ))
    };

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

    // --- web dashboard (headless, for remote access) -----------------------
    let enable_web = env_bool("ENABLE_WEB", false);
    let web_bind = env_str("WEB_BIND", "0.0.0.0:8080");
    let started = Instant::now();

    if enable_web {
        let web_deps = WebDeps {
            state: state.clone(),
            manager: manager.clone(),
            risk: risk.clone(),
            stats: stats.clone(),
            logs: logs.clone(),
            edge: edge.clone(),
            started_at: started,
            maker_mode,
            maker_half_spread_bps: env_parse("HALF_SPREAD_BPS", 150i32),
            maker_quote_size_usdc: env_parse("QUOTE_SIZE_USDC", 100.0f64),
            maker_max_inventory_usdc: env_parse("MAX_INVENTORY_USDC", 500.0f64),
            dry_run,
            wallet_address: owner.clone(),
            clob_host: env_str("POLYMARKET_HOST", "https://clob.polymarket.com"),
            http: http.clone(),
        };
        let bind = web_bind.clone();
        tokio::spawn(async move {
            if let Err(e) = run_web_dashboard(web_deps, &bind).await {
                error!(error = %e, "web dashboard exited");
            }
        });
    }

    // --- TUI dashboard or plain idle -------------------------------------
    let enable_tui = env_bool("ENABLE_TUI", false);
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
                maker_mode,
                maker_half_spread_bps: env_parse("HALF_SPREAD_BPS", 150i32),
                maker_quote_size_usdc: env_parse("QUOTE_SIZE_USDC", 100.0f64),
                maker_max_inventory_usdc: env_parse("MAX_INVENTORY_USDC", 500.0f64),
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
