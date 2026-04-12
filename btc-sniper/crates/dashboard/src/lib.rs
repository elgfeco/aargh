//! # sniper-dashboard
//!
//! Dashboard for the BTC sniper bot. Two modes:
//!
//! * **TUI** — Ratatui terminal dashboard (ENABLE_TUI=true)
//! * **Web** — Axum HTTP server + embedded SPA (ENABLE_WEB=true, port 8080)
//!
//! ## Web layout
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │  HEADER — mode · status · BTC price · uptime · ws/fills count  │
//! ├────────────────────────────────────┬────────────────────────────┤
//! │  ORDER BOOK                        │  BTC FEED                 │
//! ├────────────────────────────────────┤  (tape stats + momentum)  │
//! │  ORDERS / POSITIONS                ├────────────────────────────┤
//! │                                    │  STRATEGY + LATENCY       │
//! ├────────────────────────────────────┴────────────────────────────┤
//! │  LOG STREAM                                                    │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  FOOTER — keybinds                                             │
//! └────────────────────────────────────────────────────────────────-┘
//! ```
//!
//! Keyboard (both modes):
//!
//! * `q` — quit (TUI only)
//! * `p` — pause / unpause the executor
//! * `k` — kill switch (trips RiskEngine, cancels all open orders)
//!
//! ## API endpoints (web)
//!
//! * `GET  /`            — HTML dashboard
//! * `GET  /api/status`  — JSON status blob
//! * `GET  /api/book`    — order book snapshots
//! * `GET  /api/orders`  — order list
//! * `GET  /api/latency` — pipeline timing
//! * `GET  /api/logs`    — log ring
//! * `POST /api/pause`   — toggle pause
//! * `POST /api/kill`    — trip kill switch

pub mod app;
pub mod layout;
pub mod log_ring;
pub mod web;

pub use app::{run_dashboard, DashboardDeps};
pub use log_ring::LogRing;
pub use web::{run_web_dashboard, WebDeps};
