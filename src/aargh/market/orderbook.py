"""Order book analysis, liquidity monitoring, and market inefficiency detection.

Uses the Polymarket CLOB API to fetch order book data and analyze:
- Bid/ask spread and depth
- Liquidity at various price levels
- Price impact estimation (how much does buying X$ move the price?)
- Market efficiency metrics
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone

import aiohttp

from aargh.config import Config
from aargh.market.models import TrackedMarket

logger = logging.getLogger(__name__)


@dataclass
class OrderBookLevel:
    """A single price level in the order book."""

    price: float
    size: float  # In USDC


@dataclass
class OrderBookSnapshot:
    """Snapshot of an order book at a point in time."""

    token_id: str
    market_id: str
    bids: list[OrderBookLevel]  # Sorted descending by price
    asks: list[OrderBookLevel]  # Sorted ascending by price
    timestamp: float = field(default_factory=time.monotonic)

    @property
    def best_bid(self) -> float:
        return self.bids[0].price if self.bids else 0.0

    @property
    def best_ask(self) -> float:
        return self.asks[0].price if self.asks else 1.0

    @property
    def spread(self) -> float:
        return self.best_ask - self.best_bid

    @property
    def spread_pct(self) -> float:
        mid = self.mid_price
        return (self.spread / mid * 100) if mid > 0 else 0

    @property
    def mid_price(self) -> float:
        return (self.best_bid + self.best_ask) / 2 if self.bids and self.asks else 0.5

    @property
    def total_bid_liquidity(self) -> float:
        return sum(l.size for l in self.bids)

    @property
    def total_ask_liquidity(self) -> float:
        return sum(l.size for l in self.asks)

    @property
    def bid_ask_imbalance(self) -> float:
        """Positive = more buy pressure, negative = more sell pressure."""
        total = self.total_bid_liquidity + self.total_ask_liquidity
        if total == 0:
            return 0.0
        return (self.total_bid_liquidity - self.total_ask_liquidity) / total


@dataclass
class PriceImpact:
    """Estimated price impact of a hypothetical order."""

    side: str  # "buy" or "sell"
    order_size: float  # In USDC
    avg_fill_price: float
    worst_fill_price: float
    slippage_pct: float  # vs mid price
    levels_consumed: int


@dataclass
class MarketEfficiency:
    """Efficiency metrics for a market."""

    market_id: str
    market_question: str
    spread_pct: float
    bid_ask_imbalance: float
    total_liquidity: float
    bid_depth_at_1pct: float  # Liquidity within 1% of best bid
    ask_depth_at_1pct: float  # Liquidity within 1% of best ask
    price_impact_100: float  # Slippage for $100 order
    price_impact_500: float  # Slippage for $500 order
    yes_no_sum: float  # Should be ~1.0
    inefficiency_score: float  # 0-1, higher = more inefficient
    timestamp: datetime = field(default_factory=lambda: datetime.now(timezone.utc))


class OrderBookAnalyzer:
    """Fetches and analyzes Polymarket CLOB order books."""

    def __init__(self, config: Config, session: aiohttp.ClientSession | None = None):
        self._config = config
        self._host = config.polymarket_host
        self._session = session
        self._owns_session = session is None
        self._snapshots: dict[str, OrderBookSnapshot] = {}  # token_id -> latest
        self._efficiency_cache: dict[str, MarketEfficiency] = {}

    async def _ensure_session(self) -> aiohttp.ClientSession:
        if self._session is None or self._session.closed:
            self._session = aiohttp.ClientSession()
            self._owns_session = True
        return self._session

    async def close(self) -> None:
        if self._owns_session and self._session and not self._session.closed:
            await self._session.close()

    async def fetch_book(self, token_id: str, market_id: str = "") -> OrderBookSnapshot | None:
        """Fetch the order book for a specific token from Polymarket CLOB."""
        session = await self._ensure_session()
        try:
            url = f"{self._host}/book"
            params = {"token_id": token_id}
            async with session.get(url, params=params) as resp:
                if resp.status != 200:
                    logger.debug("Book fetch failed for %s: %d", token_id, resp.status)
                    return None
                data = await resp.json()

                bids = [
                    OrderBookLevel(price=float(o["price"]), size=float(o["size"]))
                    for o in data.get("bids", [])
                ]
                asks = [
                    OrderBookLevel(price=float(o["price"]), size=float(o["size"]))
                    for o in data.get("asks", [])
                ]

                # Sort: bids descending, asks ascending
                bids.sort(key=lambda x: x.price, reverse=True)
                asks.sort(key=lambda x: x.price)

                snapshot = OrderBookSnapshot(
                    token_id=token_id,
                    market_id=market_id,
                    bids=bids,
                    asks=asks,
                )
                self._snapshots[token_id] = snapshot
                return snapshot

        except Exception as e:
            logger.warning("Error fetching book for %s: %s", token_id, e)
            return None

    def estimate_price_impact(
        self, snapshot: OrderBookSnapshot, side: str, order_size: float
    ) -> PriceImpact:
        """Estimate the price impact of placing an order of given size.

        Walks the order book to simulate fills and calculates average
        fill price and slippage vs mid price.
        """
        mid = snapshot.mid_price
        levels = snapshot.asks if side == "buy" else snapshot.bids
        remaining = order_size
        total_cost = 0.0
        total_shares = 0.0
        worst_price = mid
        levels_consumed = 0

        for level in levels:
            if remaining <= 0:
                break
            fill_amount = min(remaining, level.size)
            shares = fill_amount / level.price if level.price > 0 else 0
            total_cost += fill_amount
            total_shares += shares
            remaining -= fill_amount
            worst_price = level.price
            levels_consumed += 1

        avg_price = total_cost / total_shares if total_shares > 0 else mid
        slippage = abs(avg_price - mid) / mid * 100 if mid > 0 else 0

        return PriceImpact(
            side=side,
            order_size=order_size,
            avg_fill_price=avg_price,
            worst_fill_price=worst_price,
            slippage_pct=slippage,
            levels_consumed=levels_consumed,
        )

    def _depth_within_pct(self, levels: list[OrderBookLevel], best_price: float, pct: float) -> float:
        """Sum liquidity within pct% of best price."""
        if not levels or best_price <= 0:
            return 0.0
        threshold = best_price * pct / 100
        total = 0.0
        for level in levels:
            if abs(level.price - best_price) <= threshold:
                total += level.size
        return total

    async def analyze_market(self, market: TrackedMarket) -> MarketEfficiency | None:
        """Full efficiency analysis of a market."""
        yes_token = market.yes_token
        no_token = market.no_token
        if not yes_token:
            return None

        # Fetch both sides of the order book
        yes_book = await self.fetch_book(yes_token, market.market.id)
        no_book = await self.fetch_book(no_token, market.market.id) if no_token else None

        if not yes_book:
            return None

        # Price impact estimates
        impact_100 = self.estimate_price_impact(yes_book, "buy", 100)
        impact_500 = self.estimate_price_impact(yes_book, "buy", 500)

        # Yes + No sum
        yes_mid = yes_book.mid_price
        no_mid = no_book.mid_price if no_book else (1.0 - yes_mid)
        yes_no_sum = yes_mid + no_mid

        # Liquidity depth
        bid_depth = self._depth_within_pct(yes_book.bids, yes_book.best_bid, 1.0)
        ask_depth = self._depth_within_pct(yes_book.asks, yes_book.best_ask, 1.0)

        # Calculate inefficiency score (0-1, higher = more inefficient = more opportunity)
        inefficiency = 0.0
        # Wide spread contributes
        inefficiency += min(0.3, yes_book.spread_pct / 10)
        # Yes+No deviation from 1.0
        inefficiency += min(0.3, abs(yes_no_sum - 1.0) * 5)
        # Low liquidity
        total_liq = yes_book.total_bid_liquidity + yes_book.total_ask_liquidity
        inefficiency += min(0.2, max(0, (1000 - total_liq) / 5000))
        # High price impact
        inefficiency += min(0.2, impact_100.slippage_pct / 5)

        efficiency = MarketEfficiency(
            market_id=market.market.id,
            market_question=market.market.question,
            spread_pct=yes_book.spread_pct,
            bid_ask_imbalance=yes_book.bid_ask_imbalance,
            total_liquidity=total_liq,
            bid_depth_at_1pct=bid_depth,
            ask_depth_at_1pct=ask_depth,
            price_impact_100=impact_100.slippage_pct,
            price_impact_500=impact_500.slippage_pct,
            yes_no_sum=yes_no_sum,
            inefficiency_score=min(1.0, inefficiency),
        )

        self._efficiency_cache[market.market.id] = efficiency
        logger.info(
            "Market efficiency %s: spread=%.2f%%, liq=$%.0f, impact100=%.2f%%, ineff=%.2f",
            market.market.question[:30], efficiency.spread_pct,
            total_liq, efficiency.price_impact_100, efficiency.inefficiency_score,
        )
        return efficiency

    @property
    def efficiency_cache(self) -> dict[str, MarketEfficiency]:
        return dict(self._efficiency_cache)
