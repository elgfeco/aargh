# btc-sniper

Ultra-low-latency BTC prediction-market sniper bot for Polymarket's CLOB.
Written in Rust, targeting AWS us-east-1 next to Polymarket's NYC infra.

> **WARNING** — This is production-shaped code for a real trading strategy.
> It will submit real orders with real funds once `DRY_RUN=false` is set and
> a funded wallet is configured. Run in dry-run mode until you have read
> every line of `crates/signal`, `crates/executor`, and `crates/risk`.

## Quickstart

```bash
# 1. Set up the server (Ubuntu 24.04 LTS on AWS EC2 c7gn.16xlarge)
sudo bash setup.sh
# (reboot after first run to activate isolcpus)

# 2. Build the release binary
cd btc-sniper
cargo build --release

# 3. Configure
cp .env.example .env
$EDITOR .env           # at minimum set POLYMARKET_OWNER and DRY_RUN
$EDITOR config/markets.toml   # list the BTC condition IDs to watch

# 4. Run (dry run — safe)
./target/release/sniper

# 5. Go live (only after thorough paper-trading)
sed -i 's/DRY_RUN=true/DRY_RUN=false/' .env
sudo systemctl enable --now sniper.service
```

## Architecture

```
┌─────────────────┐      ┌──────────────┐      ┌──────────────┐
│   Polymarket    │      │   Binance    │      │   Coinbase   │
│   CLOB WS       │      │   trade WS   │      │   trade WS   │
└───────┬─────────┘      └───────┬──────┘      └───────┬──────┘
        │                        │                     │
        ▼                        ▼                     ▼
┌─────────────────────────────────────────────────────────────┐
│           sniper-feed (parse + local book + tape)          │
│                                                             │
│   OrderBook<asset> (RwLock<BTreeMap>)  │  BtcTape (EMAs)   │
└──────────────────┬──────────────────────────────────────────┘
                   │
                   ▼  5ms tick
┌─────────────────────────────┐
│   sniper-signal             │
│   edge_bps(book, tape)      │
│   Kelly lookup table        │
└─────────┬───────────────────┘
          │ Intent::Fire
          ▼
┌─────────────────────────────┐      ┌──────────────────────┐
│   sniper-risk               │─────▶│ kill-switch / limits │
│   check_and_reserve         │      └──────────────────────┘
└─────────┬───────────────────┘
          │ Allow
          ▼
┌─────────────────────────────┐      ┌──────────────────────┐
│   sniper-executor           │────▶│  Polymarket CLOB     │
│   pre-signed templates      │      │  POST /order (H2)    │
│   stale-order sweeper       │      └──────────────────────┘
└─────────┬───────────────────┘
          │
          ▼
┌─────────────────────────────┐
│   sniper-dashboard (Ratatui)│
└─────────────────────────────┘
```

## Crate map

| crate              | responsibility                                      |
|--------------------|-----------------------------------------------------|
| `sniper-feed`      | Polymarket + Binance WS consumers, local order book |
| `sniper-signal`    | Edge detection, momentum, Kelly sizing              |
| `sniper-executor`  | Order submission, cancellation, latency histograms  |
| `sniper-risk`      | Daily loss cap, position limits, kill switch        |
| `sniper-dashboard` | 20 FPS terminal UI (book / positions / signal / log)|
| `sniper` (bin)     | Tokio task orchestration, graceful shutdown         |

## Environment variables

See `.env.example` for the full list. Highlights:

| var                    | default                  | meaning                                |
|------------------------|--------------------------|----------------------------------------|
| `DRY_RUN`              | `true`                   | If true, do NOT submit orders          |
| `POLYMARKET_OWNER`     | —                        | Wallet address (0x…)                   |
| `POLYMARKET_HOST`      | https://clob.polymarket.com | CLOB REST base                      |
| `POLYMARKET_WS`        | wss://…/ws/market        | Market-data WS                         |
| `BINANCE_WS`           | wss://…/btcusdt@trade    | BTC reference feed                     |
| `MIN_EDGE_BPS`         | `250`                    | Edge threshold in basis points         |
| `KELLY_FRACTION`       | `0.15`                   | Fractional Kelly (0.15 = 15% of full)  |
| `BANKROLL_USDC`        | `10000`                  | Starting bankroll for Kelly sizing     |
| `MAX_POSITION_USDC`    | `500`                    | Hard per-trade size cap                |
| `MAX_OPEN_POSITIONS`   | `8`                      | Circuit breaker                        |
| `MAX_DAILY_LOSS_USDC`  | `250`                    | Kill-switch threshold                  |
| `STALE_ORDER_TTL_MS`   | `500`                    | Auto-cancel unfilled orders after this |
| `ENABLE_TUI`           | `true`                   | Ratatui dashboard on/off               |
| `RUST_LOG`             | `info,sniper=debug`      | Tracing filter                         |
| `TRADING_CORES`        | `4,5,6,7`                | CPU affinity for trading threads       |

## Operational notes

* **Logs** are JSON to stdout. systemd routes to journald — view with
  `journalctl -u sniper.service -f`.
* **Latency dump**: `pkill -USR1 sniper` prints the percentile table
  (see `docs/LATENCY.md`).
* **Graceful shutdown**: `systemctl stop sniper.service` sends SIGTERM →
  `kill_all` cancels every open order before exit.
* **Kill switch**: press `k` in the dashboard, or trip via `/kill` API (not
  yet exposed), or let the daily-loss cap latch it automatically.

## Tests

```bash
cargo test --workspace
```

All crates ship with unit tests for their hot-path logic. The signal and
risk crates are especially well-covered because they're the places a bug
becomes a lost dollar.

## Disclaimer

Automated trading bots against prediction markets involve substantial risk,
including total loss of the deposit. This repository is provided for
research and educational purposes. The authors accept no responsibility for
losses incurred while running this code. **Check your local laws** regarding
prediction-market trading — Polymarket access is restricted in some
jurisdictions.
