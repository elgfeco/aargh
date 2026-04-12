//! # sniper-feed
//!
//! Market-data ingestion for the BTC sniper bot.
//!
//! This crate owns two families of feeds:
//!
//! 1. **Polymarket CLOB** (`polymarket`) — persistent WS subscription to the
//!    `market` channel, parsing book snapshots and diffs into a local
//!    [`OrderBook`] per asset/token id.
//! 2. **BTC reference** (`binance`) — Binance trade feed (with Coinbase /
//!    Kraken slots reserved for failover) updating the global [`BtcTape`].
//!
//! The hot path NEVER allocates on each message: book diffs are parsed with
//! [`simd_json`] into pre-allocated scratch buffers and applied via the
//! `parking_lot::RwLock`-protected BTreeMap. The resulting [`MarketState`]
//! is held in an `Arc<MarketState>` that every downstream task can clone
//! freely.

pub mod binance;
pub mod book;
pub mod btc;
pub mod discovery;
pub mod polymarket;
pub mod state;
pub mod types;

pub use binance::BinanceFeed;
pub use book::{BookSnapshot, OrderBook, PriceLevel};
pub use btc::{BtcTape, BtcTick, Source, TapeSnapshot};
pub use discovery::{decimal_to_bytes32, discover_btc_markets, DiscoveredMarket};
pub use polymarket::{parse_message, PolymarketFeed, PolymarketMessage};
pub use state::MarketState;
pub use types::{parse_hex32, AssetId, MarketId, Price, Side, Size};
