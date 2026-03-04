"""Arbitrage detection across markets and temporal price analysis.

Detects three types of arbitrage:
1. Same-market: Yes + No prices sum to < 1.0 (guaranteed profit)
2. Temporal: Market price hasn't updated to reflect known events (stale)
3. Cross-market: Same event priced differently on correlated markets
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone

from aargh.market.models import GammaMarket, TrackedMarket
from aargh.signals.base_model import Signal

logger = logging.getLogger(__name__)


@dataclass
class ArbitrageOpportunity:
    """A detected arbitrage opportunity."""

    arb_type: str  # "same_market", "temporal", "cross_market"
    description: str
    expected_profit_pct: float  # Expected profit as percentage
    confidence: float  # 0-1
    markets: list[TrackedMarket]
    timestamp: datetime = field(default_factory=lambda: datetime.now(timezone.utc))

    # Same-market specific
    yes_price: float = 0.0
    no_price: float = 0.0
    spread: float = 0.0

    # Temporal specific
    stale_duration_s: float = 0.0
    expected_fair_price: float = 0.0
    current_price: float = 0.0

    # Cross-market specific
    market_a_price: float = 0.0
    market_b_price: float = 0.0

    @property
    def is_actionable(self) -> bool:
        return self.expected_profit_pct > 0.5 and self.confidence > 0.3


class ArbitrageDetector:
    """Detects arbitrage opportunities across and within markets."""

    def __init__(self, min_profit_pct: float = 0.5, stale_threshold_s: float = 30.0):
        self._min_profit_pct = min_profit_pct
        self._stale_threshold_s = stale_threshold_s
        self._price_history: dict[str, list[tuple[float, float, float]]] = {}  # market_id -> [(time, yes, no)]
        self._last_event_time: dict[str, float] = {}  # match_id -> time of last known event
        self._active_opps: list[ArbitrageOpportunity] = []

    def scan_same_market(self, markets: list[TrackedMarket]) -> list[ArbitrageOpportunity]:
        """Find markets where Yes + No < 1.0 (risk-free profit)."""
        opps: list[ArbitrageOpportunity] = []
        for market in markets:
            yes_p = market.yes_price
            no_p = market.no_price
            total = yes_p + no_p

            if total <= 0 or yes_p <= 0 or no_p <= 0:
                continue

            if total < 1.0:
                # Buy both sides: pay `total`, guaranteed payout of $1
                profit_pct = ((1.0 / total) - 1.0) * 100
                if profit_pct >= self._min_profit_pct:
                    opp = ArbitrageOpportunity(
                        arb_type="same_market",
                        description=(
                            f"Yes+No={total:.4f} < 1.0 on '{market.market.question[:50]}' "
                            f"-> {profit_pct:.2f}% guaranteed profit"
                        ),
                        expected_profit_pct=profit_pct,
                        confidence=0.95,  # High confidence - pure arithmetic
                        markets=[market],
                        yes_price=yes_p,
                        no_price=no_p,
                        spread=1.0 - total,
                    )
                    opps.append(opp)
                    logger.info("SAME-MARKET ARB: %s", opp.description)

            # Also detect overpriced markets (total > 1.0 = sell both sides)
            if total > 1.0:
                overcharge_pct = (total - 1.0) * 100
                # This means market makers are extracting a spread
                if overcharge_pct > 5.0:
                    logger.debug(
                        "Market spread %.2f%% on '%s' (Yes=%.3f No=%.3f)",
                        overcharge_pct, market.market.question[:40], yes_p, no_p,
                    )

        return opps

    def scan_temporal(
        self, markets: list[TrackedMarket], signals: list[Signal]
    ) -> list[ArbitrageOpportunity]:
        """Find markets with stale prices that haven't reacted to known events.

        A temporal arbitrage exists when:
        1. We received a game event via API (GRID/PandaScore)
        2. The market price hasn't moved to reflect this event
        3. The delay exceeds our stale threshold
        """
        opps: list[ArbitrageOpportunity] = []
        now = time.monotonic()

        for signal in signals:
            market = signal.market
            if signal.abs_edge < 0.03:
                continue

            # Check if we have a timestamp for when the event was received
            event_time = self._last_event_time.get(signal.event.match_id)
            if event_time is None:
                continue

            stale_duration = now - event_time
            if stale_duration < self._stale_threshold_s:
                continue  # Not stale enough yet

            profit_pct = signal.abs_edge * 100
            if profit_pct < self._min_profit_pct:
                continue

            opp = ArbitrageOpportunity(
                arb_type="temporal",
                description=(
                    f"Stale price on '{market.market.question[:50]}': "
                    f"model={signal.model_prob:.3f} vs market={signal.market_prob:.3f} "
                    f"({stale_duration:.0f}s since event)"
                ),
                expected_profit_pct=profit_pct,
                confidence=min(0.9, signal.confidence * (1 - 1 / (stale_duration + 1))),
                markets=[market],
                stale_duration_s=stale_duration,
                expected_fair_price=signal.model_prob,
                current_price=signal.market_prob,
            )
            opps.append(opp)
            logger.info("TEMPORAL ARB: %s", opp.description)

        return opps

    def scan_cross_market(self, markets: list[TrackedMarket]) -> list[ArbitrageOpportunity]:
        """Find correlated markets with inconsistent pricing.

        Detects when two markets that should have related outcomes are
        priced inconsistently, e.g.:
        - "Will NaVi win vs FaZe?" at 0.70
        - "Will NaVi win the tournament?" at 0.30
          (NaVi winning tournament requires winning this match first)
        """
        opps: list[ArbitrageOpportunity] = []

        # Group markets by team
        team_markets: dict[str, list[TrackedMarket]] = {}
        for m in markets:
            for team in (m.team_a, m.team_b):
                team_lower = team.lower()
                if team_lower not in team_markets:
                    team_markets[team_lower] = []
                team_markets[team_lower].append(m)

        # Check for pricing inconsistencies within each team's markets
        for team, team_mkts in team_markets.items():
            if len(team_mkts) < 2:
                continue

            for i, m1 in enumerate(team_mkts):
                for m2 in team_mkts[i + 1:]:
                    # Check if one market's outcome is a prerequisite for the other
                    q1 = m1.market.question.lower()
                    q2 = m2.market.question.lower()

                    # Tournament win requires match win
                    is_match_vs_tournament = (
                        ("tournament" in q2 or "winner" in q2 or "champion" in q2)
                        and ("vs" in q1 or "beat" in q1 or "win" in q1)
                    )
                    is_tournament_vs_match = (
                        ("tournament" in q1 or "winner" in q1 or "champion" in q1)
                        and ("vs" in q2 or "beat" in q2 or "win" in q2)
                    )

                    if is_match_vs_tournament:
                        match_market, tourn_market = m1, m2
                    elif is_tournament_vs_match:
                        match_market, tourn_market = m2, m1
                    else:
                        continue

                    # Determine prices for the team in each market
                    match_yes = match_market.yes_price if team in match_market.team_a.lower() else match_market.no_price
                    tourn_yes = tourn_market.yes_price if team in tourn_market.team_a.lower() else tourn_market.no_price

                    # Tournament win probability can't exceed match win probability
                    # (winning tournament requires winning this match)
                    if tourn_yes > match_yes + 0.02:
                        profit_pct = (tourn_yes - match_yes) * 100
                        if profit_pct >= self._min_profit_pct:
                            opp = ArbitrageOpportunity(
                                arb_type="cross_market",
                                description=(
                                    f"'{team}' tournament P={tourn_yes:.3f} > match P={match_yes:.3f}: "
                                    f"tournament win requires match win but is priced higher"
                                ),
                                expected_profit_pct=profit_pct,
                                confidence=0.7,
                                markets=[match_market, tourn_market],
                                market_a_price=match_yes,
                                market_b_price=tourn_yes,
                            )
                            opps.append(opp)
                            logger.info("CROSS-MARKET ARB: %s", opp.description)

        return opps

    def scan_all(
        self, markets: list[TrackedMarket], signals: list[Signal] | None = None
    ) -> list[ArbitrageOpportunity]:
        """Run all arbitrage scans and return combined results."""
        opps: list[ArbitrageOpportunity] = []
        opps.extend(self.scan_same_market(markets))
        opps.extend(self.scan_cross_market(markets))
        if signals:
            opps.extend(self.scan_temporal(markets, signals))

        # Sort by expected profit
        opps.sort(key=lambda o: o.expected_profit_pct, reverse=True)
        self._active_opps = opps
        return opps

    def record_event_time(self, match_id: str, event_time: float | None = None) -> None:
        """Record when a game event was received from the API."""
        self._last_event_time[match_id] = event_time or time.monotonic()

    def record_price(self, market_id: str, yes_price: float, no_price: float) -> None:
        """Record a price snapshot for temporal analysis."""
        now = time.monotonic()
        if market_id not in self._price_history:
            self._price_history[market_id] = []
        self._price_history[market_id].append((now, yes_price, no_price))
        # Keep last 100 samples
        self._price_history[market_id] = self._price_history[market_id][-100:]

    def get_price_staleness(self, market_id: str) -> float:
        """How many seconds since the price last changed significantly."""
        history = self._price_history.get(market_id, [])
        if len(history) < 2:
            return 0.0
        current = history[-1]
        for t, yes, no in reversed(history[:-1]):
            if abs(yes - current[1]) > 0.005 or abs(no - current[2]) > 0.005:
                return current[0] - t
        return current[0] - history[0][0]

    @property
    def active_opportunities(self) -> list[ArbitrageOpportunity]:
        return list(self._active_opps)
