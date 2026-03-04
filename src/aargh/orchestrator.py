"""Main orchestrator wiring all components via asyncio."""

from __future__ import annotations

import asyncio
import logging
import time

from aargh.config import Config
from aargh.dashboard import Dashboard
from aargh.feeds.base import BaseFeed
from aargh.feeds.events import GameEvent
from aargh.feeds.grid_feed import GridFeed
from aargh.feeds.pandascore_feed import PandaScoreFeed
from aargh.feeds.stream_monitor import StreamMonitor
from aargh.feeds.twitch_feed import TwitchFeed
from aargh.market.executor import Executor
from aargh.market.orderbook import OrderBookAnalyzer
from aargh.market.scanner import MarketScanner
from aargh.matching.linker import MatchLinker
from aargh.signals.arbitrage import ArbitrageDetector
from aargh.signals.base_model import Signal
from aargh.signals.engine import SignalEngine
from aargh.signals.timing import TimingTracker
from aargh.trading.portfolio import Portfolio
from aargh.trading.risk import RiskManager

logger = logging.getLogger(__name__)


class Orchestrator:
    """Wires all bot components and manages the async event loop."""

    def __init__(self, config: Config):
        self._config = config
        self._dashboard = Dashboard()

        # Data feeds
        self._feeds: list[BaseFeed] = []
        if config.grid_api_key:
            self._feeds.append(GridFeed(config))
        if config.pandascore_api_key:
            self._feeds.append(PandaScoreFeed(config))
        if config.twitch_client_id:
            self._feeds.append(TwitchFeed(config))

        if not self._feeds:
            logger.warning("No feed API keys configured - adding feeds with empty keys for demo")
            self._feeds.append(GridFeed(config))
            self._feeds.append(PandaScoreFeed(config))

        # Core components
        self._scanner = MarketScanner(config)
        self._linker = MatchLinker(self._feeds)
        self._signal_engine = SignalEngine(min_edge=config.min_edge)
        self._risk_manager = RiskManager(config)
        self._portfolio = Portfolio.load()
        self._executor = Executor(config, self._portfolio)

        # New components
        self._stream_monitor = StreamMonitor(config)
        self._arb_detector = ArbitrageDetector(
            min_profit_pct=config.arb_min_profit_pct,
            stale_threshold_s=config.stale_threshold_s,
        )
        self._timing_tracker = TimingTracker()
        self._orderbook = OrderBookAnalyzer(config)

        # Wire dashboard
        self._dashboard.set_portfolio(self._portfolio)
        self._dashboard.set_arb_detector(self._arb_detector)
        self._dashboard.set_timing_tracker(self._timing_tracker)

        # Queues
        self._event_queue: asyncio.Queue[tuple[GameEvent, str]] = asyncio.Queue()
        self._signal_queue: asyncio.Queue[Signal] = asyncio.Queue()

        self._running = False
        self._subscribed_matches: set[str] = set()
        self._recent_signals: list[Signal] = []

    async def run(self) -> None:
        """Start all bot tasks."""
        self._running = True
        self._dashboard.print_startup()
        self._dashboard.print_status("Starting bot components...")

        # Connect feeds
        for feed in self._feeds:
            try:
                await feed.connect()
                self._dashboard.print_status(f"Connected to {feed.name} feed")
            except Exception as e:
                logger.warning("Failed to connect %s: %s", feed.name, e)

        # Start stream monitor
        try:
            await self._stream_monitor.start()
            self._dashboard.print_status("Stream monitor started")
        except Exception as e:
            logger.warning("Failed to start stream monitor: %s", e)

        self._dashboard.print_status("Starting main loop...")

        try:
            async with asyncio.TaskGroup() as tg:
                tg.create_task(self._market_scan_loop())
                tg.create_task(self._event_processor())
                tg.create_task(self._signal_processor())
                tg.create_task(self._portfolio_saver())
                tg.create_task(self._stream_monitor_loop())
                tg.create_task(self._arbitrage_scan_loop())
                tg.create_task(self._orderbook_scan_loop())
        except* KeyboardInterrupt:
            logger.info("Shutting down...")
        except* Exception as eg:
            for e in eg.exceptions:
                logger.error("Task error: %s", e)
        finally:
            await self._shutdown()

    async def _market_scan_loop(self) -> None:
        """Periodically scan for esports markets and link to live matches."""
        while self._running:
            try:
                markets = await self._scanner.scan_once()
                if markets:
                    self._dashboard.print_status(f"Found {len(markets)} esports markets")
                    self._dashboard.update_markets(markets)

                    linked = await self._linker.link_markets(markets)
                    linked_count = sum(1 for m in linked if m.linked_match_id)
                    if linked_count:
                        self._dashboard.print_status(f"Linked {linked_count} markets to live matches")

                    for market in linked:
                        if market.linked_match_id and market.linked_match_id not in self._subscribed_matches:
                            self._subscribed_matches.add(market.linked_match_id)
                            asyncio.create_task(
                                self._feed_listener(market.linked_match_id, market.market.id)
                            )

            except Exception as e:
                logger.error("Market scan error: %s", e)

            await asyncio.sleep(self._config.scan_interval)

    async def _feed_listener(self, match_id: str, market_id: str) -> None:
        """Listen to a specific match from all feeds and enqueue events."""
        for feed in self._feeds:
            try:
                request_id = f"{feed.name}-{match_id}-{time.monotonic()}"
                self._timing_tracker.start_request(request_id)

                async for event in feed.subscribe(match_id):
                    # Record latency
                    self._timing_tracker.record_response(
                        request_id, feed.name, match_id,
                        event.event_type.value,
                        event.timestamp.isoformat() if event.timestamp else "",
                    )

                    # Record for arbitrage temporal analysis
                    self._arb_detector.record_event_time(match_id)
                    self._stream_monitor.record_api_event(match_id)

                    # Calculate edge window using stream delay
                    for ch, status in self._stream_monitor.live_streams.items():
                        delay = self._stream_monitor.get_delay_estimate(ch)
                        self._timing_tracker.calculate_edge_window(
                            match_id, feed.name,
                            delay.estimated_delay_s,
                            stream_channel=ch,
                            delay_confidence=delay.confidence,
                        )

                    await self._event_queue.put((event, market_id))
                    self._dashboard.add_event(
                        f"[{feed.name}] {event.event_type.value} "
                        f"R{event.round_number or '?'} "
                        f"{event.team_a_rounds or 0}-{event.team_b_rounds or 0}"
                    )

                    # New request for next event
                    request_id = f"{feed.name}-{match_id}-{time.monotonic()}"
                    self._timing_tracker.start_request(request_id)

            except Exception as e:
                logger.warning("Feed %s error for match %s: %s", feed.name, match_id, e)

    async def _event_processor(self) -> None:
        """Process game events through the signal engine."""
        while self._running:
            try:
                event, market_id = await asyncio.wait_for(
                    self._event_queue.get(), timeout=5.0
                )
            except asyncio.TimeoutError:
                continue

            tracked = self._scanner.tracked_markets.get(market_id)
            if not tracked:
                continue

            # Record price for arbitrage analysis
            self._arb_detector.record_price(
                market_id, tracked.yes_price, tracked.no_price
            )

            signal = self._signal_engine.process(event, tracked)
            if signal:
                self._recent_signals.append(signal)
                if len(self._recent_signals) > 50:
                    self._recent_signals = self._recent_signals[-50:]
                await self._signal_queue.put(signal)
                self._dashboard.add_signal(signal)

    async def _signal_processor(self) -> None:
        """Process signals through risk manager and executor."""
        while self._running:
            try:
                signal = await asyncio.wait_for(
                    self._signal_queue.get(), timeout=5.0
                )
            except asyncio.TimeoutError:
                continue

            order = self._risk_manager.evaluate(signal)
            if not order:
                continue

            result = await self._executor.execute(order)
            if result.success:
                self._risk_manager.record_bet(order)
                self._dashboard.add_trade(order)
                self._dashboard.print_status(
                    f"[bold green]PAPER BET:[/bold green] {order.side} ${order.amount:.2f} "
                    f"@ {order.price:.3f} (edge={signal.edge:+.1%})"
                )

    async def _stream_monitor_loop(self) -> None:
        """Monitor Twitch streams for state changes and delay estimation."""
        async for channel, status in self._stream_monitor.monitor_streams():
            self._dashboard.add_event(
                f"[twitch] Stream {channel} -> {status.state} "
                f"({status.viewer_count} viewers, delay~{status.estimated_delay_s:.0f}s)"
            )

    async def _arbitrage_scan_loop(self) -> None:
        """Periodically scan for arbitrage opportunities."""
        while self._running:
            await asyncio.sleep(15)
            try:
                markets = list(self._scanner.tracked_markets.values())
                if not markets:
                    continue

                opps = self._arb_detector.scan_all(markets, self._recent_signals)
                for opp in opps:
                    if opp.is_actionable:
                        self._dashboard.add_event(
                            f"[arb] {opp.arb_type}: {opp.description[:80]} "
                            f"({opp.expected_profit_pct:.1f}%)"
                        )
            except Exception as e:
                logger.warning("Arbitrage scan error: %s", e)

    async def _orderbook_scan_loop(self) -> None:
        """Periodically analyze order books for tracked markets."""
        while self._running:
            await asyncio.sleep(30)
            try:
                for market in list(self._scanner.tracked_markets.values())[:5]:
                    eff = await self._orderbook.analyze_market(market)
                    if eff and eff.inefficiency_score > 0.3:
                        self._dashboard.add_event(
                            f"[book] Inefficiency={eff.inefficiency_score:.2f} "
                            f"spread={eff.spread_pct:.1f}% "
                            f"liq=${eff.total_liquidity:.0f} "
                            f"'{market.market.question[:30]}'"
                        )
            except Exception as e:
                logger.warning("Order book scan error: %s", e)

    async def _portfolio_saver(self) -> None:
        """Periodically save portfolio state."""
        while self._running:
            await asyncio.sleep(30)
            try:
                self._portfolio.save()
            except Exception as e:
                logger.warning("Failed to save portfolio: %s", e)

    async def _shutdown(self) -> None:
        """Clean shutdown of all components."""
        self._running = False
        self._dashboard.print_status("Shutting down...")

        for feed in self._feeds:
            try:
                await feed.disconnect()
            except Exception:
                pass

        await self._stream_monitor.stop()
        await self._scanner.close()
        await self._orderbook.close()
        self._portfolio.save()

        p = self._portfolio
        self._dashboard.print_status(
            f"Session summary: {p.total_bets} bets, "
            f"{p.wins}W/{p.losses}L, "
            f"P&L: ${p.total_pnl:+.2f}"
        )

        # Print timing stats
        for feed_name, stats in self._timing_tracker.all_feed_stats.items():
            self._dashboard.print_status(
                f"Feed {feed_name}: avg={stats.avg_latency_ms:.0f}ms "
                f"p95={stats.p95_latency_ms:.0f}ms ({stats.sample_count} samples)"
            )

        # Print arbitrage summary
        opps = self._arb_detector.active_opportunities
        if opps:
            self._dashboard.print_status(f"Active arbitrage opportunities: {len(opps)}")
