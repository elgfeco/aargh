//! Layout engine for the 6-panel dashboard.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │  HEADER — mode · status · BTC price · uptime · ws/fills count  │
//! ├────────────────────────────────────┬────────────────────────────┤
//! │  ORDER BOOK (per-asset)            │  BTC FEED                 │
//! │                                    │  (tape stats + momentum)  │
//! ├────────────────────────────────────┤                           │
//! │  ORDERS / POSITIONS                ├─��──────────────────────────┤
//! │                                    │  STRATEGY                 │
//! ├────────────────────────────────────┤  (maker or taker params)  │
//! │  LOG STREAM                        ├────────────────────────────┤
//! │                                    │  LATENCY                  │
//! ├────────────────────────────────────┴────────────────────────────┤
//! │  FOOTER — keybinds                                             │
//! └────────────────────────────────────────────────────────────────-┘
//! ```

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Top-level split: header (3 rows) | body | footer (1 row).
pub fn main_layout(area: Rect) -> (Rect, Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),       // header
            Constraint::Min(10),         // body
            Constraint::Length(1),       // footer
        ])
        .split(area);
    (chunks[0], chunks[1], chunks[2])
}

/// Body split: left 65% | right 35%.
/// Left: book / orders / logs.
/// Right: btc feed / strategy / latency.
pub struct BodyLayout {
    pub book: Rect,
    pub orders: Rect,
    pub logs: Rect,
    pub btc_feed: Rect,
    pub strategy: Rect,
    pub latency: Rect,
}

pub fn body_layout(area: Rect) -> BodyLayout {
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(area);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(30),  // book
            Constraint::Percentage(35),  // orders
            Constraint::Percentage(35),  // logs
        ])
        .split(horizontal[0]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(38),  // btc feed
            Constraint::Percentage(35),  // strategy
            Constraint::Percentage(27),  // latency
        ])
        .split(horizontal[1]);

    BodyLayout {
        book: left[0],
        orders: left[1],
        logs: left[2],
        btc_feed: right[0],
        strategy: right[1],
        latency: right[2],
    }
}

// Keep the old function signature so any external caller doesn't break.
pub fn four_panel_layout(area: Rect) -> [Rect; 4] {
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(area);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(35),
            Constraint::Percentage(25),
            Constraint::Percentage(40),
        ])
        .split(horizontal[0]);

    [left[0], left[1], left[2], horizontal[1]]
}
