"""Main orchestrator wiring all components via asyncio."""

from __future__ import annotations

import asyncio
import logging

from aargh.config import Config
from aargh.dashboard import Dashboard
from aargh.feeds.base import BaseFeed
from aargh.feeds.events import GameEvent
from aargh.feeds.grid_feed import GridFeed
from aargh.feeds.pandascore_feed import PandaScoreFeed
from aargh.feeds.twitch_feed import TwitchFeed
from aargh.market.executor import Executor
from aargh.market.scanner import MarketScanner
from aargh.matching.linker import MatchLinker
from aargh.signals.base_model import Signal
from aargh.signals.engine import SignalEngine
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

        # If no API keys configured, still add feeds (they'll log warnings)
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
        self._dashboard.set_portfolio(self._portfolio)

        # Queues
        self._event_queue: asyncio.Queue[tuple[GameEvent, str]] = asyncio.Queue()
        self._signal_queue: asyncio.Queue[Signal] = asyncio.Queue()

        self._running = False
        self._subscribed_matches: set[str] = set()

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

        self._dashboard.print_status("Starting main loop...")

        try:
            async with asyncio.TaskGroup() as tg:
                tg.create_task(self._market_scan_loop())
                tg.create_task(self._event_processor())
                tg.create_task(self._signal_processor())
                tg.create_task(self._portfolio_saver())
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
                # Scan for markets
                markets = await self._scanner.scan_once()
                if markets:
                    self._dashboard.print_status(f"Found {len(markets)} esports markets")
                    self._dashboard.update_markets(markets)

                    # Link markets to live matches
                    linked = await self._linker.link_markets(markets)
                    linked_count = sum(1 for m in linked if m.linked_match_id)
                    if linked_count:
                        self._dashboard.print_status(f"Linked {linked_count} markets to live matches")

                    # Subscribe to new matches
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
                async for event in feed.subscribe(match_id):
                    await self._event_queue.put((event, market_id))
                    self._dashboard.add_event(
                        f"[{feed.name}] {event.event_type.value} "
                        f"R{event.round_number or '?'} "
                        f"{event.team_a_rounds or 0}-{event.team_b_rounds or 0}"
                    )
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

            # Find the tracked market
            tracked = self._scanner.tracked_markets.get(market_id)
            if not tracked:
                continue

            # Run through signal engine
            signal = self._signal_engine.process(event, tracked)
            if signal:
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

            # Risk check and sizing
            order = self._risk_manager.evaluate(signal)
            if not order:
                continue

            # Execute (paper trade)
            result = await self._executor.execute(order)
            if result.success:
                self._risk_manager.record_bet(order)
                self._dashboard.add_trade(order)
                self._dashboard.print_status(
                    f"[bold green]PAPER BET:[/bold green] {order.side} ${order.amount:.2f} "
                    f"@ {order.price:.3f} (edge={signal.edge:+.1%})"
                )

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

        await self._scanner.close()
        self._portfolio.save()

        # Print final summary
        p = self._portfolio
        self._dashboard.print_status(
            f"Session summary: {p.total_bets} bets, "
            f"{p.wins}W/{p.losses}L, "
            f"P&L: ${p.total_pnl:+.2f}"
        )
