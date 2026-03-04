"""Real-time console dashboard using Rich."""

from __future__ import annotations

import logging
from datetime import datetime, timezone

from rich.console import Console
from rich.layout import Layout
from rich.live import Live
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

from aargh.market.models import TrackedMarket
from aargh.signals.base_model import Signal
from aargh.trading.portfolio import Portfolio
from aargh.trading.risk import BetOrder

logger = logging.getLogger(__name__)


class Dashboard:
    """Rich console dashboard displaying bot state in real-time."""

    def __init__(self):
        self._console = Console()
        self._markets: list[TrackedMarket] = []
        self._recent_events: list[str] = []
        self._recent_signals: list[Signal] = []
        self._recent_trades: list[BetOrder] = []
        self._portfolio: Portfolio | None = None
        self._arb_detector = None
        self._timing_tracker = None
        self._live: Live | None = None
        self._max_recent = 15

    def set_portfolio(self, portfolio: Portfolio) -> None:
        self._portfolio = portfolio

    def set_arb_detector(self, arb_detector) -> None:
        self._arb_detector = arb_detector

    def set_timing_tracker(self, timing_tracker) -> None:
        self._timing_tracker = timing_tracker

    def update_markets(self, markets: list[TrackedMarket]) -> None:
        self._markets = markets

    def add_event(self, description: str) -> None:
        ts = datetime.now(timezone.utc).strftime("%H:%M:%S")
        self._recent_events.append(f"[{ts}] {description}")
        if len(self._recent_events) > self._max_recent:
            self._recent_events = self._recent_events[-self._max_recent:]

    def add_signal(self, signal: Signal) -> None:
        self._recent_signals.append(signal)
        if len(self._recent_signals) > self._max_recent:
            self._recent_signals = self._recent_signals[-self._max_recent:]

    def add_trade(self, order: BetOrder) -> None:
        self._recent_trades.append(order)
        if len(self._recent_trades) > self._max_recent:
            self._recent_trades = self._recent_trades[-self._max_recent:]

    def render(self) -> Layout:
        """Build the full dashboard layout."""
        layout = Layout()
        layout.split_column(
            Layout(self._render_header(), size=3),
            Layout(name="main"),
            Layout(name="bottom", size=9),
        )
        layout["main"].split_row(
            Layout(self._render_markets(), ratio=1),
            Layout(name="right", ratio=1),
        )
        layout["main"]["right"].split_column(
            Layout(self._render_events(), ratio=1),
            Layout(self._render_signals(), ratio=1),
        )
        layout["bottom"].split_row(
            Layout(self._render_portfolio(), ratio=1),
            Layout(self._render_arbitrage(), ratio=1),
            Layout(self._render_timing(), ratio=1),
        )
        return layout

    def _render_header(self) -> Panel:
        now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC")
        status = Text(f"  AARGH - Polymarket Esports Arbitrage Bot  |  {now}  |  PAPER TRADING", style="bold white on blue")
        return Panel(status, style="blue")

    def _render_markets(self) -> Panel:
        table = Table(title="Tracked Markets", expand=True, show_lines=True)
        table.add_column("Market", style="cyan", max_width=40)
        table.add_column("Game", style="yellow", width=6)
        table.add_column("Yes", style="green", width=6)
        table.add_column("No", style="red", width=6)
        table.add_column("Linked", style="magenta", width=8)

        for m in self._markets[:10]:
            linked = "YES" if m.linked_match_id else "-"
            table.add_row(
                m.market.question[:40],
                m.game.upper(),
                f"{m.yes_price:.2f}",
                f"{m.no_price:.2f}",
                linked,
            )

        if not self._markets:
            table.add_row("Scanning for markets...", "", "", "", "")

        return Panel(table, title="Markets", border_style="cyan")

    def _render_events(self) -> Panel:
        if self._recent_events:
            text = "\n".join(self._recent_events[-8:])
        else:
            text = "Waiting for match events..."
        return Panel(text, title="Live Events", border_style="yellow")

    def _render_signals(self) -> Panel:
        table = Table(expand=True)
        table.add_column("Dir", width=4)
        table.add_column("Edge", width=7)
        table.add_column("Model", width=6)
        table.add_column("Mkt", width=6)
        table.add_column("Market", max_width=30)

        for sig in reversed(self._recent_signals[-6:]):
            color = "green" if sig.edge > 0 else "red"
            table.add_row(
                Text(sig.direction, style=f"bold {color}"),
                f"{sig.edge:+.1%}",
                f"{sig.model_prob:.3f}",
                f"{sig.market_prob:.3f}",
                sig.market.market.question[:30],
            )

        if not self._recent_signals:
            table.add_row("", "", "", "", "No signals yet")

        return Panel(table, title="Signals", border_style="green")

    def _render_portfolio(self) -> Panel:
        if not self._portfolio:
            return Panel("No portfolio data", title="Portfolio")

        p = self._portfolio
        lines = [
            f"Bets: {p.total_bets}  W/L: {p.wins}/{p.losses}  Rate: {p.win_rate:.0%}",
            f"Realized: ${p.total_pnl:+.2f}  Unrealized: ${p.unrealized_pnl:+.2f}",
            f"Open positions: {len(p.open_positions)}",
        ]
        color = "green" if p.total_pnl >= 0 else "red"
        return Panel(
            Text("\n".join(lines), style=f"bold {color}"),
            title="Portfolio",
            border_style=color,
        )

    def _render_arbitrage(self) -> Panel:
        lines: list[str] = []
        if self._arb_detector:
            opps = self._arb_detector.active_opportunities
            if opps:
                for opp in opps[:4]:
                    color = "green" if opp.is_actionable else "yellow"
                    lines.append(
                        f"[{color}]{opp.arb_type}[/{color}] "
                        f"{opp.expected_profit_pct:.1f}% "
                        f"(conf={opp.confidence:.0%}) "
                        f"{opp.description[:40]}"
                    )
            else:
                lines.append("[dim]No arbitrage opportunities detected[/dim]")
        else:
            lines.append("[dim]Arbitrage detector not active[/dim]")

        return Panel("\n".join(lines), title="Arbitrage", border_style="magenta")

    def _render_timing(self) -> Panel:
        lines: list[str] = []
        if self._timing_tracker:
            # Feed latency stats
            for name, stats in self._timing_tracker.all_feed_stats.items():
                lines.append(
                    f"[cyan]{name}[/cyan] "
                    f"avg={stats.avg_latency_ms:.0f}ms "
                    f"p95={stats.p95_latency_ms:.0f}ms "
                    f"({stats.sample_count}x)"
                )
            # Edge windows
            for match_id, edge in self._timing_tracker.all_edge_windows.items():
                color = "green" if edge.has_edge else "red"
                lines.append(
                    f"[{color}]Edge {edge.edge_window_s:.1f}s[/{color}] "
                    f"({edge.edge_quality}) "
                    f"API={edge.api_latency_s:.1f}s "
                    f"stream={edge.stream_delay_s:.0f}s"
                )
            if not lines:
                lines.append("[dim]Collecting timing data...[/dim]")
        else:
            lines.append("[dim]Timing tracker not active[/dim]")

        return Panel("\n".join(lines[:5]), title="Timing Edge", border_style="blue")

    def print_startup(self) -> None:
        """Print startup banner."""
        self._console.print()
        self._console.print(Panel.fit(
            "[bold blue]AARGH[/bold blue] - Polymarket Esports Arbitrage Bot\n"
            "[dim]Paper trading mode - no real bets placed[/dim]",
            border_style="blue",
        ))
        self._console.print()

    def print_status(self, msg: str, style: str = "") -> None:
        """Print a status message."""
        ts = datetime.now(timezone.utc).strftime("%H:%M:%S")
        self._console.print(f"[dim][{ts}][/dim] {msg}", style=style)
