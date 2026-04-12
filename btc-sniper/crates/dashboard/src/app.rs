//! Dashboard event loop.
//!
//! `run_dashboard` takes ownership of the terminal until the user presses
//! `q` (or a shutdown signal is delivered externally). Every 50ms it:
//!
//! 1. Reads keyboard events (non-blocking, 10ms poll)
//! 2. Snapshots state from feed / executor / risk
//! 3. Redraws all four panels

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
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
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

use crate::layout::four_panel_layout;
use crate::log_ring::LogRing;

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

        // Non-blocking keyboard poll
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
                        deps.logs.push("INFO", format!("pause toggled → {now}"));
                    }
                    KeyCode::Char('k') => {
                        deps.risk.trip("operator kill via dashboard");
                        deps.logs.push("WARN", "kill switch latched");
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

fn render(f: &mut Frame<'_>, d: &DashboardDeps) {
    let area = f.size();
    let [book_area, pos_area, log_area, right_area] = four_panel_layout(area);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(right_area);

    render_book(f, book_area, d);
    render_positions(f, pos_area, d);
    render_logs(f, log_area, d);
    render_signal(f, right[0], d);
    render_latency(f, right[1], d);
}

fn render_book(f: &mut Frame<'_>, area: Rect, d: &DashboardDeps) {
    let mut rows: Vec<Row> = Vec::new();
    d.state.for_each_book(|asset, book| {
        let snap = book.snapshot();
        let bid = snap
            .best_bid
            .map(|l| format!("{} @ {}", l.size, l.price))
            .unwrap_or_else(|| "—".into());
        let ask = snap
            .best_ask
            .map(|l| format!("{} @ {}", l.size, l.price))
            .unwrap_or_else(|| "—".into());
        let mid = snap
            .mid()
            .map(|p| p.to_string())
            .unwrap_or_else(|| "—".into());
        let spread = snap
            .spread_bps()
            .map(|b| format!("{b}bps"))
            .unwrap_or_else(|| "—".into());
        rows.push(Row::new(vec![
            short_asset(asset),
            bid,
            ask,
            mid,
            spread,
        ]));
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(18),
            Constraint::Length(18),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec!["asset", "best bid", "best ask", "mid", "spread"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title(" ORDER BOOK "));
    f.render_widget(table, area);
}

fn render_positions(f: &mut Frame<'_>, area: Rect, d: &DashboardDeps) {
    let orders = d.manager.snapshot();
    let rows: Vec<Row> = orders
        .iter()
        .map(|o| {
            let color = match o.state {
                sniper_executor::OrderState::Filled => Color::Green,
                sniper_executor::OrderState::Rejected | sniper_executor::OrderState::Cancelled => {
                    Color::Red
                }
                _ => Color::Yellow,
            };
            Row::new(vec![
                format!("{}", o.id.0),
                short_asset(o.asset),
                o.side.as_str().to_string(),
                format!("{}", o.price),
                format!("{}/{}", o.filled, o.size),
                format!("{:?}", o.state),
            ])
            .style(Style::default().fg(color))
        })
        .collect();

    let filled = d.manager.fills_total();
    let pnl = d.risk.realised_pnl_atoms() as f64 / 1_000_000.0;
    let title = format!(
        " POSITIONS — open={} fills={} pnl={:+.2} USDC ",
        d.manager.open_count(),
        filled,
        pnl
    );

    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(14),
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Length(14),
            Constraint::Length(12),
        ],
    )
    .header(
        Row::new(vec!["id", "asset", "side", "price", "filled/sz", "state"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(table, area);
}

fn render_logs(f: &mut Frame<'_>, area: Rect, d: &DashboardDeps) {
    let entries = d.logs.snapshot();
    let take = entries.len().saturating_sub(50);
    let lines: Vec<Line> = entries[take..]
        .iter()
        .map(|e| {
            let color = match e.level.as_str() {
                "ERROR" => Color::Red,
                "WARN" => Color::Yellow,
                "INFO" => Color::White,
                "DEBUG" => Color::Gray,
                _ => Color::DarkGray,
            };
            Line::from(vec![
                Span::styled(format!("{:5} ", e.level), Style::default().fg(color)),
                Span::raw(e.message.clone()),
            ])
        })
        .collect();

    let par = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" LOG STREAM "));
    f.render_widget(par, area);
}

fn render_signal(f: &mut Frame<'_>, area: Rect, d: &DashboardDeps) {
    let tape = d.state.tape().snapshot();
    let uptime = d.started_at.elapsed();
    let paused = d.manager.is_paused();
    let killed = d.risk.is_tripped();

    let header_color = if killed {
        Color::Red
    } else if paused {
        Color::Yellow
    } else {
        Color::Green
    };
    let status = if killed {
        "KILLED"
    } else if paused {
        "PAUSED"
    } else {
        "LIVE"
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {status} "),
                Style::default().bg(header_color).fg(Color::Black),
            ),
            Span::raw(format!(
                "  uptime {}s",
                uptime.as_secs()
            )),
        ]),
        Line::raw(""),
        Line::raw(format!("BTC last   : {:>10.2}", tape.last_price)),
        Line::raw(format!("VWAP 15s   : {:>10.2}", tape.vwap_15s)),
        Line::raw(format!("VWAP 60s   : {:>10.2}", tape.vwap_60s)),
        Line::raw(format!("EMA 5      : {:>10.2}", tape.ema_fast)),
        Line::raw(format!("EMA 20     : {:>10.2}", tape.ema_slow)),
        Line::raw(format!("momentum   : {:>+.3}", tape.momentum)),
        Line::raw(format!("btc prob   : {:>.3}", tape.directional_prob())),
        Line::raw(""),
        Line::raw(format!("ws msgs    : {}", d.state.messages_total())),
        Line::raw(format!(
            "min_edge   : {}bps",
            d.edge.params().min_edge_bps
        )),
    ];

    let par = Paragraph::new(lines)
        .alignment(Alignment::Left)
        .block(Block::default().borders(Borders::ALL).title(" SIGNAL "));
    f.render_widget(par, area);
}

fn render_latency(f: &mut Frame<'_>, area: Rect, d: &DashboardDeps) {
    let snap = d.stats.snapshot();
    let rows = [
        ("ws_parse", snap.ws_parse),
        ("sig_eval", snap.signal_eval),
        ("fired", snap.order_fired),
        ("acked", snap.order_acked),
    ];
    let lines: Vec<Line> = std::iter::once(Line::from(Span::styled(
        "stage     count    p50    p99   p999",
        Style::default().add_modifier(Modifier::BOLD),
    )))
    .chain(rows.into_iter().map(|(name, r)| {
        Line::raw(format!(
            "{name:9}{cnt:>6} {p50:>5}µs {p99:>5}µs {p999:>5}µs",
            cnt = r.count,
            p50 = r.p50_ns / 1_000,
            p99 = r.p99_ns / 1_000,
            p999 = r.p999_ns / 1_000,
        ))
    }))
    .collect();

    let par =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" LATENCY "));
    f.render_widget(par, area);
}

fn short_asset(asset: [u8; 32]) -> String {
    let mut s = String::with_capacity(12);
    for &b in &asset[..6] {
        s.push_str(&format!("{:02x}", b));
    }
    s
}
