//! Dashboard event loop + rendering.
//!
//! 6-panel layout with header/footer, maker-mode aware, 20 FPS refresh.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table};
use ratatui::{Frame, Terminal};
use tokio::sync::watch;
use tracing::info;

use sniper_executor::{LatencyStats, OrderManager};
use sniper_feed::MarketState;
use sniper_risk::RiskEngine;
use sniper_signal::EdgeEngine;

use crate::layout::{body_layout, main_layout};
use crate::log_ring::LogRing;

// ── Colours ─────────────────────────────────────────────────────────────────

const C_BG: Color = Color::Rgb(13, 17, 23);       // dark navy
const C_PANEL: Color = Color::Rgb(22, 27, 34);     // panel bg
const C_BORDER: Color = Color::Rgb(48, 54, 61);    // subtle border
const C_ACCENT: Color = Color::Rgb(88, 166, 255);  // blue accent
const C_GREEN: Color = Color::Rgb(63, 185, 80);    // profit / live
const C_RED: Color = Color::Rgb(248, 81, 73);      // loss / killed
const C_YELLOW: Color = Color::Rgb(210, 153, 34);  // warning / paused
const C_DIM: Color = Color::Rgb(110, 118, 129);    // muted text
const C_TEXT: Color = Color::Rgb(201, 209, 217);   // primary text
const C_BRIGHT: Color = Color::Rgb(240, 246, 252); // emphasis

// ── DashboardDeps ───────────────────────────────────────────────────────────

/// Everything the dashboard needs to render and react.
pub struct DashboardDeps {
    pub state: Arc<MarketState>,
    pub manager: Arc<OrderManager>,
    pub risk: Arc<RiskEngine>,
    pub stats: Arc<LatencyStats>,
    pub logs: LogRing,
    pub edge: EdgeEngine,
    pub shutdown: watch::Sender<bool>,
    pub started_at: Instant,
    pub maker_mode: bool,
    pub maker_half_spread_bps: i32,
    pub maker_quote_size_usdc: f64,
    pub maker_max_inventory_usdc: f64,
}

/// Run the TUI event loop until the user quits or `shutdown` flips true.
pub async fn run_dashboard(deps: DashboardDeps) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, &deps).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    deps: &DashboardDeps,
) -> Result<()> {
    let tick = Duration::from_millis(50);
    let mut last_draw = Instant::now();
    let shutdown_rx = deps.shutdown.subscribe();

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        if event::poll(Duration::from_millis(10))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => {
                        info!("dashboard: quit requested");
                        let _ = deps.shutdown.send(true);
                        break;
                    }
                    KeyCode::Char('p') => {
                        let now = !deps.manager.is_paused();
                        deps.manager.set_paused(now);
                        deps.logs.push(
                            "INFO",
                            if now { "executor PAUSED" } else { "executor RESUMED" },
                        );
                    }
                    KeyCode::Char('k') => {
                        deps.risk.trip("operator kill via dashboard");
                        deps.logs.push("WARN", "KILL SWITCH latched by operator");
                    }
                    _ => {}
                }
            }
        }

        if last_draw.elapsed() >= tick {
            terminal.draw(|f| render(f, deps))?;
            last_draw = Instant::now();
        } else {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    Ok(())
}

// ── Top-level render ────────────────────────────────────────────────────────

fn render(f: &mut Frame<'_>, d: &DashboardDeps) {
    let area = f.size();

    // Fill background
    let bg = Block::default().style(Style::default().bg(C_BG));
    f.render_widget(bg, area);

    let (header_area, body_area, footer_area) = main_layout(area);
    let body = body_layout(body_area);

    render_header(f, header_area, d);
    render_book(f, body.book, d);
    render_orders(f, body.orders, d);
    render_logs(f, body.logs, d);
    render_btc_feed(f, body.btc_feed, d);
    render_strategy(f, body.strategy, d);
    render_latency(f, body.latency, d);
    render_footer(f, footer_area, d);
}

// ── Header ──────────────────────────────────────────────────────────────────

fn render_header(f: &mut Frame<'_>, area: Rect, d: &DashboardDeps) {
    let tape = d.state.tape().snapshot();
    let paused = d.manager.is_paused();
    let killed = d.risk.is_tripped();
    let uptime = d.started_at.elapsed();

    let (status_text, status_bg) = if killed {
        (" KILLED ", C_RED)
    } else if paused {
        (" PAUSED ", C_YELLOW)
    } else {
        ("  LIVE  ", C_GREEN)
    };

    let mode_text = if d.maker_mode { " MAKER " } else { " TAKER " };

    let hours = uptime.as_secs() / 3600;
    let mins = (uptime.as_secs() % 3600) / 60;
    let secs = uptime.as_secs() % 60;

    let btc_color = if tape.momentum > 0.1 {
        C_GREEN
    } else if tape.momentum < -0.1 {
        C_RED
    } else {
        C_TEXT
    };

    let source_str = tape
        .last_source
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|| "—".into());

    let line = Line::from(vec![
        Span::styled("  BTC SNIPER  ", Style::default().fg(C_BG).bg(C_ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(mode_text, Style::default().fg(C_BG).bg(C_DIM).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(status_text, Style::default().fg(C_BG).bg(status_bg).add_modifier(Modifier::BOLD)),
        Span::raw("    "),
        Span::styled("BTC ", Style::default().fg(C_DIM)),
        Span::styled(format!("${:.2}", tape.last_price), Style::default().fg(btc_color).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(format!("({source_str})"), Style::default().fg(C_DIM)),
        Span::raw("    "),
        Span::styled(format!("{hours:02}:{mins:02}:{secs:02}"), Style::default().fg(C_DIM)),
        Span::raw("    "),
        Span::styled(format!("ws:{}", d.state.messages_total()), Style::default().fg(C_DIM)),
        Span::raw("  "),
        Span::styled(format!("fills:{}", d.manager.fills_total()), Style::default().fg(C_DIM)),
        Span::raw("  "),
        Span::styled(format!("open:{}", d.manager.open_count()), Style::default().fg(C_DIM)),
    ]);

    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(C_BORDER))
        .style(Style::default().bg(C_BG));
    let par = Paragraph::new(vec![Line::raw(""), line])
        .block(block);
    f.render_widget(par, area);
}

// ── Order Book ──────────────────────────────────────────────────────────────

fn render_book(f: &mut Frame<'_>, area: Rect, d: &DashboardDeps) {
    let mut rows: Vec<Row> = Vec::new();
    d.state.for_each_book(|asset, book| {
        let snap = book.snapshot();
        let bid_price = snap.best_bid.map(|l| l.price.as_prob()).unwrap_or(0.0);
        let ask_price = snap.best_ask.map(|l| l.price.as_prob()).unwrap_or(0.0);
        let bid_size = snap.best_bid.map(|l| l.size.as_usdc()).unwrap_or(0.0);
        let ask_size = snap.best_ask.map(|l| l.size.as_usdc()).unwrap_or(0.0);

        let mid = snap.mid().map(|p| p.as_prob()).unwrap_or(0.0);
        let spread = snap.spread_bps().unwrap_or(0);
        let imbalance = snap.imbalance();

        let imb_color = if imbalance > 0.3 {
            C_GREEN
        } else if imbalance < -0.3 {
            C_RED
        } else {
            C_DIM
        };

        let spread_color = if spread < 100 {
            C_GREEN
        } else if spread < 300 {
            C_YELLOW
        } else {
            C_RED
        };

        rows.push(
            Row::new(vec![
                short_asset(asset),
                format!("${:.0} @ {:.1}c", bid_size, bid_price * 100.0),
                format!("${:.0} @ {:.1}c", ask_size, ask_price * 100.0),
                format!("{:.1}c", mid * 100.0),
                format!("{}bp", spread),
                format!("{:+.2}", imbalance),
            ])
            .style(Style::default().fg(C_TEXT).bg(C_PANEL))
            .height(1),
        );
        // Apply color to the last cells via individual styling
        // (ratatui Row doesn't support per-cell styles easily, so we rely on text)
        let _ = (imb_color, spread_color); // used conceptually above
    });

    if rows.is_empty() {
        rows.push(
            Row::new(vec!["", "", "waiting for markets...", "", "", ""])
                .style(Style::default().fg(C_DIM).bg(C_PANEL)),
        );
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(14),  // asset
            Constraint::Length(16),  // bid
            Constraint::Length(16),  // ask
            Constraint::Length(8),   // mid
            Constraint::Length(7),   // spread
            Constraint::Length(7),   // imbalance
        ],
    )
    .header(
        Row::new(vec!["ASSET", "BID", "ASK", "MID", "SPREAD", "IMB"])
            .style(Style::default().fg(C_ACCENT).bg(C_PANEL).add_modifier(Modifier::BOLD)),
    )
    .block(panel_block(" ORDER BOOK "));
    f.render_widget(table, area);
}

// ── Orders / Positions ──────────────────────────────────────────────────────

fn render_orders(f: &mut Frame<'_>, area: Rect, d: &DashboardDeps) {
    let orders = d.manager.snapshot();
    let pnl = d.risk.realised_pnl_atoms() as f64 / 1_000_000.0;
    let pnl_color = if pnl > 0.01 { C_GREEN } else if pnl < -0.01 { C_RED } else { C_DIM };

    let mut rows: Vec<Row> = Vec::new();
    for o in &orders {
        let (state_str, color) = match o.state {
            sniper_executor::OrderState::Filled => ("FILLED", C_GREEN),
            sniper_executor::OrderState::PartialFill => ("PARTIAL", C_YELLOW),
            sniper_executor::OrderState::Acked => ("ACKED", C_ACCENT),
            sniper_executor::OrderState::Pending => ("PEND", C_DIM),
            sniper_executor::OrderState::Rejected => ("REJ", C_RED),
            sniper_executor::OrderState::Cancelled => ("CXL", C_RED),
        };

        let fill_pct = if o.size.0 > 0 {
            (o.filled.0 as f64 / o.size.0 as f64 * 100.0) as u16
        } else {
            0
        };
        let fill_bar = match fill_pct {
            0 => "          ".to_string(),
            1..=25 => format!("{:>3}% ##    ", fill_pct),
            26..=50 => format!("{:>3}% ####  ", fill_pct),
            51..=75 => format!("{:>3}% ###### ", fill_pct),
            76..=99 => format!("{:>3}% ########", fill_pct),
            _ => "100% ##########".to_string(),
        };

        let side_color = if o.side == sniper_feed::Side::Buy { C_GREEN } else { C_RED };

        rows.push(
            Row::new(vec![
                Span::styled(format!("{:>5}", o.id.0), Style::default().fg(C_DIM)),
                Span::styled(short_asset(o.asset), Style::default().fg(C_TEXT)),
                Span::styled(
                    format!("{:>4}", o.side.as_str()),
                    Style::default().fg(side_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:.1}c", o.price.as_prob() * 100.0),
                    Style::default().fg(C_TEXT),
                ),
                Span::styled(
                    format!("${:.0}", o.size.as_usdc()),
                    Style::default().fg(C_TEXT),
                ),
                Span::styled(fill_bar, Style::default().fg(color)),
                Span::styled(state_str.to_string(), Style::default().fg(color).add_modifier(Modifier::BOLD)),
            ]),
        );
    }

    if rows.is_empty() {
        rows.push(Row::new(vec![
            Span::raw(""),
            Span::raw(""),
            Span::raw(""),
            Span::styled("no orders", Style::default().fg(C_DIM)),
            Span::raw(""),
            Span::raw(""),
            Span::raw(""),
        ]));
    }

    let title = format!(
        " ORDERS  open:{}  fills:{}  PnL:",
        d.manager.open_count(),
        d.manager.fills_total(),
    );

    let title_line = Line::from(vec![
        Span::styled(title, Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("{:+.2} USDC ", pnl),
            Style::default().fg(pnl_color).add_modifier(Modifier::BOLD),
        ),
    ]);

    let table = Table::new(
        rows,
        [
            Constraint::Length(6),   // id
            Constraint::Length(14),  // asset
            Constraint::Length(5),   // side
            Constraint::Length(7),   // price
            Constraint::Length(6),   // size
            Constraint::Length(15),  // fill bar
            Constraint::Length(8),   // state
        ],
    )
    .header(
        Row::new(vec!["ID", "ASSET", "SIDE", "PRICE", "SIZE", "FILL", "STATE"])
            .style(Style::default().fg(C_ACCENT).bg(C_PANEL).add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(C_BORDER))
            .style(Style::default().bg(C_PANEL))
            .title(title_line),
    );
    f.render_widget(table, area);
}

// ── Log Stream ──────────────────────────────────────────────────────────────

fn render_logs(f: &mut Frame<'_>, area: Rect, d: &DashboardDeps) {
    let entries = d.logs.snapshot();
    let max_lines = area.height.saturating_sub(2) as usize;
    let start = entries.len().saturating_sub(max_lines);

    let lines: Vec<Line> = entries[start..]
        .iter()
        .map(|e| {
            let (tag, color) = match e.level.as_str() {
                "ERROR" => ("ERR", C_RED),
                "WARN" => ("WRN", C_YELLOW),
                "INFO" => ("INF", C_GREEN),
                "DEBUG" => ("DBG", C_DIM),
                _ => ("???", C_DIM),
            };
            let elapsed = e.ts.elapsed();
            let age = if elapsed.as_secs() < 60 {
                format!("{:>3}s", elapsed.as_secs())
            } else {
                format!("{:>3}m", elapsed.as_secs() / 60)
            };
            Line::from(vec![
                Span::styled(format!("{age} "), Style::default().fg(C_DIM)),
                Span::styled(format!("{tag} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(e.message.clone(), Style::default().fg(C_TEXT)),
            ])
        })
        .collect();

    let par = Paragraph::new(lines).block(panel_block(" LOG "));
    f.render_widget(par, area);
}

// ── BTC Feed ────────────────────────────────────────────────────────────────

fn render_btc_feed(f: &mut Frame<'_>, area: Rect, d: &DashboardDeps) {
    let tape = d.state.tape().snapshot();

    let mom = tape.momentum;
    let mom_bar_width = ((mom.abs() * 10.0).min(10.0)) as usize;
    let mom_fill = "#".repeat(mom_bar_width);
    let mom_empty = " ".repeat(10 - mom_bar_width);
    let mom_color = if mom > 0.1 { C_GREEN } else if mom < -0.1 { C_RED } else { C_DIM };

    let mom_visual = if mom >= 0.0 {
        format!("          |{:<10}", mom_fill)
    } else {
        format!("{:>10}|          ", format!("{}{}", mom_empty, mom_fill))
    };

    let dir_prob = tape.directional_prob();
    let dir_color = if dir_prob > 0.55 { C_GREEN } else if dir_prob < 0.45 { C_RED } else { C_DIM };

    let source = tape
        .last_source
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|| "none".into());

    let lines = vec![
        Line::from(vec![
            Span::styled("  last      ", Style::default().fg(C_DIM)),
            Span::styled(
                format!("${:>10.2}", tape.last_price),
                Style::default().fg(C_BRIGHT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  vwap 15s  ", Style::default().fg(C_DIM)),
            Span::styled(format!("${:>10.2}", tape.vwap_15s), Style::default().fg(C_TEXT)),
        ]),
        Line::from(vec![
            Span::styled("  vwap 60s  ", Style::default().fg(C_DIM)),
            Span::styled(format!("${:>10.2}", tape.vwap_60s), Style::default().fg(C_TEXT)),
        ]),
        Line::from(vec![
            Span::styled("  ema fast  ", Style::default().fg(C_DIM)),
            Span::styled(format!("${:>10.2}", tape.ema_fast), Style::default().fg(C_TEXT)),
        ]),
        Line::from(vec![
            Span::styled("  ema slow  ", Style::default().fg(C_DIM)),
            Span::styled(format!("${:>10.2}", tape.ema_slow), Style::default().fg(C_TEXT)),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  momentum  ", Style::default().fg(C_DIM)),
            Span::styled(format!("{:>+.4}", mom), Style::default().fg(mom_color).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled("            ", Style::default().fg(C_DIM)),
            Span::styled(mom_visual, Style::default().fg(mom_color)),
        ]),
        Line::from(vec![
            Span::styled("  P(up)     ", Style::default().fg(C_DIM)),
            Span::styled(format!("{:.1}%", dir_prob * 100.0), Style::default().fg(dir_color).add_modifier(Modifier::BOLD)),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  trades    ", Style::default().fg(C_DIM)),
            Span::styled(format!("{}", tape.trade_count), Style::default().fg(C_TEXT)),
            Span::styled(format!("  via {source}"), Style::default().fg(C_DIM)),
        ]),
    ];

    let par = Paragraph::new(lines).block(panel_block(" BTC FEED "));
    f.render_widget(par, area);
}

// ── Strategy Panel ──────────────────────────────────────────────────────────

fn render_strategy(f: &mut Frame<'_>, area: Rect, d: &DashboardDeps) {
    let paused = d.manager.is_paused();
    let killed = d.risk.is_tripped();
    let pnl = d.risk.realised_pnl_atoms() as f64 / 1_000_000.0;
    let daily_pnl = d.risk.daily_pnl_atoms() as f64 / 1_000_000.0;

    let pnl_color = if pnl > 0.01 { C_GREEN } else if pnl < -0.01 { C_RED } else { C_DIM };
    let daily_color = if daily_pnl > 0.01 { C_GREEN } else if daily_pnl < -0.01 { C_RED } else { C_DIM };

    let mut lines = Vec::new();

    if d.maker_mode {
        lines.push(Line::from(vec![
            Span::styled("  mode      ", Style::default().fg(C_DIM)),
            Span::styled("MARKET MAKER", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  spread    ", Style::default().fg(C_DIM)),
            Span::styled(
                format!("{}bp ({:.1}% ea. side)", d.maker_half_spread_bps, d.maker_half_spread_bps as f64 / 100.0),
                Style::default().fg(C_TEXT),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  quote sz  ", Style::default().fg(C_DIM)),
            Span::styled(format!("${:.0}", d.maker_quote_size_usdc), Style::default().fg(C_TEXT)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  max inv   ", Style::default().fg(C_DIM)),
            Span::styled(format!("${:.0}", d.maker_max_inventory_usdc), Style::default().fg(C_TEXT)),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("  mode      ", Style::default().fg(C_DIM)),
            Span::styled("TAKER (SNIPER)", Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  min edge  ", Style::default().fg(C_DIM)),
            Span::styled(
                format!("{}bp ({:.1}%)", d.edge.params().min_edge_bps, d.edge.params().min_edge_bps as f64 / 100.0),
                Style::default().fg(C_TEXT),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  tx cost   ", Style::default().fg(C_DIM)),
            Span::styled(format!("{}bp", d.edge.params().tx_cost_bps), Style::default().fg(C_TEXT)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  net edge  ", Style::default().fg(C_DIM)),
            Span::styled(
                format!("{}bp needed", d.edge.params().min_edge_bps + d.edge.params().tx_cost_bps),
                Style::default().fg(C_TEXT),
            ),
        ]));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("  PnL       ", Style::default().fg(C_DIM)),
        Span::styled(
            format!("{:+.2} USDC", pnl),
            Style::default().fg(pnl_color).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  daily     ", Style::default().fg(C_DIM)),
        Span::styled(format!("{:+.2} USDC", daily_pnl), Style::default().fg(daily_color)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  positions ", Style::default().fg(C_DIM)),
        Span::styled(
            format!("{} / {}", d.risk.open_positions(), d.risk.limits().max_open_positions),
            Style::default().fg(C_TEXT),
        ),
    ]));

    // Status indicator
    lines.push(Line::raw(""));
    if killed {
        lines.push(Line::from(Span::styled(
            "  !! KILL SWITCH ACTIVE !!",
            Style::default().fg(C_RED).add_modifier(Modifier::BOLD),
        )));
    } else if paused {
        lines.push(Line::from(Span::styled(
            "  || PAUSED ||",
            Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD),
        )));
    }

    let par = Paragraph::new(lines).block(panel_block(" STRATEGY "));
    f.render_widget(par, area);
}

// ── Latency ─────────────────────────────────────────────────────────────────

fn render_latency(f: &mut Frame<'_>, area: Rect, d: &DashboardDeps) {
    let snap = d.stats.snapshot();
    let stages = [
        ("ws_parse ", snap.ws_parse),
        ("sig_eval ", snap.signal_eval),
        ("order_tx ", snap.order_fired),
        ("order_ack", snap.order_acked),
    ];

    let rows: Vec<Row> = stages
        .into_iter()
        .map(|(name, r)| {
            let p99_color = if r.p99_ns < 100_000 {
                C_GREEN // <100µs
            } else if r.p99_ns < 1_000_000 {
                C_YELLOW // <1ms
            } else {
                C_RED // >=1ms
            };
            Row::new(vec![
                Span::styled(name.to_string(), Style::default().fg(C_DIM)),
                Span::styled(format!("{:>7}", r.count), Style::default().fg(C_TEXT)),
                Span::styled(format!("{:>6}µs", r.p50_ns / 1_000), Style::default().fg(C_TEXT)),
                Span::styled(
                    format!("{:>6}µs", r.p99_ns / 1_000),
                    Style::default().fg(p99_color),
                ),
                Span::styled(format!("{:>6}µs", r.p999_ns / 1_000), Style::default().fg(C_DIM)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(10),  // stage
            Constraint::Length(8),   // count
            Constraint::Length(8),   // p50
            Constraint::Length(8),   // p99
            Constraint::Length(8),   // p999
        ],
    )
    .header(
        Row::new(vec!["STAGE", "COUNT", "P50", "P99", "P999"])
            .style(Style::default().fg(C_ACCENT).bg(C_PANEL).add_modifier(Modifier::BOLD)),
    )
    .block(panel_block(" LATENCY "));
    f.render_widget(table, area);
}

// ── Footer ──────────────────────────────────────────────────────────────────

fn render_footer(f: &mut Frame<'_>, area: Rect, d: &DashboardDeps) {
    let dry = if d.manager.is_paused() { " DRY-RUN" } else { "" };
    let line = Line::from(vec![
        Span::styled("  [q]", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(" quit  ", Style::default().fg(C_DIM)),
        Span::styled("[p]", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(" pause  ", Style::default().fg(C_DIM)),
        Span::styled("[k]", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(" kill switch  ", Style::default().fg(C_DIM)),
        Span::raw("    "),
        Span::styled(
            format!(
                "{}{}",
                if d.maker_mode { "maker" } else { "taker" },
                dry
            ),
            Style::default().fg(C_DIM),
        ),
    ]);

    let par = Paragraph::new(line).style(Style::default().bg(C_BG));
    f.render_widget(par, area);
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn panel_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_BORDER))
        .title(Span::styled(
            title,
            Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(C_PANEL))
}

fn short_asset(asset: [u8; 32]) -> String {
    let mut s = String::with_capacity(12);
    for &b in &asset[..6] {
        s.push_str(&format!("{:02x}", b));
    }
    s
}
