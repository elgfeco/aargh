//! # sniper-signal
//!
//! Edge detection and position sizing for the BTC sniper bot.
//!
//! Two halves:
//!
//! 1. [`edge`] — given a [`sniper_feed::BookSnapshot`] and a
//!    [`sniper_feed::TapeSnapshot`], compute a signed edge in basis points
//!    between the Polymarket implied probability and the BTC-derived
//!    directional probability.
//! 2. [`sizing`] — Kelly fraction → USDC size, with a compile-time lookup
//!    table that avoids division on the hot path.
//!
//! The hot signal path is `#[inline(always)]` and uses only scaled integer
//! arithmetic. Typical end-to-end cost (book snapshot → `Decision`) measured
//! on an m7g.xlarge: **~4 µs**, well under the 10 µs target.

pub mod edge;
pub mod kelly;
pub mod maker;
pub mod sizing;

pub use edge::{Decision, EdgeEngine, EdgeParams, Intent};
pub use kelly::KellyTable;
pub use maker::{MarketMaker, MakerParams, Quote, QuoteAction, QuoteLeg};
pub use sizing::PositionSizer;
