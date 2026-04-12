//! # sniper-dashboard
//!
//! Ratatui terminal dashboard — 6-panel layout with header/footer, 20 FPS:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │  HEADER — mode · status · BTC price · uptime · ws/fills count  │
//! ├────────────────────────────────────┬────────────────────────────┤
//! │  ORDER BOOK                        │  BTC FEED                 │
//! ├────────────────────────────────────┤  (tape stats + momentum)  │
//! │  ORDERS / POSITIONS                ├────────────────────────────┤
//! │                                    │  STRATEGY                 │
//! ├────────────────────────────────────┤  (maker/taker + PnL)      │
//! │  LOG STREAM                        ├────────────────────────────┤
//! │                                    │  LATENCY                  │
//! ├────────────────────────────────────┴────────────────────────────┤
//! │  FOOTER — [q] quit  [p] pause  [k] kill switch                 │
//! └────────────────────────────────────────────────────────────────-┘
//! ```
//!
//! Keyboard:
//!
//! * `q` — quit (sends shutdown signal to main)
//! * `p` — pause / unpause the executor
//! * `k` — kill switch (trips RiskEngine, cancels all open orders)

pub mod app;
pub mod layout;
pub mod log_ring;

pub use app::{run_dashboard, DashboardDeps};
pub use log_ring::LogRing;
