//! # sniper-dashboard
//!
//! Ratatui terminal dashboard — 4-panel layout refreshing at 20 FPS (50ms):
//!
//! ```text
//! ┌─────────────────────────────┬──────────────┐
//! │  ORDER BOOK (YES / NO)      │  SIGNAL      │
//! ├─────────────────────────────┤              │
//! │  POSITIONS                  ├──────────────┤
//! ├─────────────────────────────┤  LATENCY     │
//! │  LOG STREAM                 │              │
//! └─────────────────────────────┴──────────────┘
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
