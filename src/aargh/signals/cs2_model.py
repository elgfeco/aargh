"""CS2 win probability model based on round scores.

Uses dynamic programming to compute P(team A wins map) given current round
scores in MR12 format (first to 13 rounds). Combines map probabilities
into series (Bo1/Bo3/Bo5) probabilities.
"""

from __future__ import annotations

import logging
from datetime import datetime, timezone
from functools import lru_cache

from aargh.feeds.events import EventType, GameEvent
from aargh.market.models import TrackedMarket
from aargh.signals.base_model import BaseSignalModel, Signal

logger = logging.getLogger(__name__)

ROUNDS_TO_WIN = 13  # MR12 format


@lru_cache(maxsize=1024)
def map_win_prob(score_a: int, score_b: int, p: float = 0.5) -> float:
    """Probability that team A wins the map from score (score_a, score_b).

    Uses recursive dynamic programming. Each remaining round is won by
    team A with probability p (default 0.5 = no skill difference).

    MR12: first team to reach 13 rounds wins. If 12-12, overtime where
    we treat it as a coin flip.

    Args:
        score_a: Rounds won by team A.
        score_b: Rounds won by team B.
        p: Per-round win probability for team A (0.5 = equal skill).
    """
    if score_a >= ROUNDS_TO_WIN and score_a > score_b:
        return 1.0
    if score_b >= ROUNDS_TO_WIN and score_b > score_a:
        return 0.0
    # Overtime: if both at or above 12, approximate as 50/50 per OT round pair
    if score_a >= ROUNDS_TO_WIN - 1 and score_b >= ROUNDS_TO_WIN - 1:
        if score_a == score_b:
            return 0.5
        if score_a > score_b:
            # Team A needs fewer rounds - slight advantage
            diff = score_a - score_b
            return 0.5 + diff * 0.1  # rough approximation, capped at 1.0
        diff = score_b - score_a
        return max(0.0, 0.5 - diff * 0.1)
    # Normal play: recurse
    return p * map_win_prob(score_a + 1, score_b, p) + (1 - p) * map_win_prob(score_a, score_b + 1, p)


@lru_cache(maxsize=256)
def series_win_prob(maps_a: int, maps_b: int, current_map_prob: float, best_of: int) -> float:
    """Probability that team A wins a best-of-N series.

    Args:
        maps_a: Maps won by team A so far.
        maps_b: Maps won by team B so far.
        current_map_prob: Probability team A wins the current map.
        best_of: Series format (1, 3, or 5).
    """
    maps_needed = (best_of + 1) // 2  # Maps needed to win (2 for Bo3, 3 for Bo5)

    if maps_a >= maps_needed:
        return 1.0
    if maps_b >= maps_needed:
        return 0.0

    # If team A wins current map: maps_a + 1
    # If team A loses current map: maps_b + 1
    # For future maps, assume 50/50 (no map-by-map prediction)
    future_map_prob = 0.5

    p_win_current = current_map_prob
    p_lose_current = 1.0 - current_map_prob

    # After current map is decided, compute remaining series probability
    prob_if_win = _remaining_series_prob(maps_a + 1, maps_b, maps_needed, future_map_prob)
    prob_if_lose = _remaining_series_prob(maps_a, maps_b + 1, maps_needed, future_map_prob)

    return p_win_current * prob_if_win + p_lose_current * prob_if_lose


@lru_cache(maxsize=256)
def _remaining_series_prob(maps_a: int, maps_b: int, maps_needed: int, p: float) -> float:
    """Probability team A wins from (maps_a, maps_b) with equal per-map probability p."""
    if maps_a >= maps_needed:
        return 1.0
    if maps_b >= maps_needed:
        return 0.0
    return p * _remaining_series_prob(maps_a + 1, maps_b, maps_needed, p) + \
           (1 - p) * _remaining_series_prob(maps_a, maps_b + 1, maps_needed, p)


class CS2Model(BaseSignalModel):
    """CS2-specific probability model using round score tables."""

    @property
    def game(self) -> str:
        return "cs2"

    def evaluate(self, event: GameEvent, market: TrackedMarket) -> Signal | None:
        if event.event_type == EventType.ROUND_ENDED:
            return self._on_round_ended(event, market)
        if event.event_type == EventType.MAP_ENDED:
            return self._on_map_ended(event, market)
        if event.event_type == EventType.SERIES_ENDED:
            return self._on_series_ended(event, market)
        return None

    def _on_round_ended(self, event: GameEvent, market: TrackedMarket) -> Signal:
        score_a = event.team_a_rounds or 0
        score_b = event.team_b_rounds or 0
        maps_a = event.team_a_maps or 0
        maps_b = event.team_b_maps or 0

        # Current map win probability
        current_map_prob = map_win_prob(score_a, score_b)

        # Overall series probability
        model_prob = series_win_prob(maps_a, maps_b, current_map_prob, market.best_of)

        market_prob = market.yes_price
        edge = model_prob - market_prob

        logger.debug(
            "CS2 signal: %s [%d-%d] map %d-%d | model=%.3f market=%.3f edge=%+.3f",
            market.market.question[:40], score_a, score_b, maps_a, maps_b,
            model_prob, market_prob, edge,
        )

        return Signal(
            market=market,
            model_prob=model_prob,
            market_prob=market_prob,
            edge=edge,
            confidence=0.8,
            event=event,
            timestamp=datetime.now(timezone.utc),
        )

    def _on_map_ended(self, event: GameEvent, market: TrackedMarket) -> Signal:
        maps_a = event.team_a_maps or 0
        maps_b = event.team_b_maps or 0

        # Map just ended, use 50/50 for next map
        model_prob = series_win_prob(maps_a, maps_b, 0.5, market.best_of)
        market_prob = market.yes_price

        return Signal(
            market=market,
            model_prob=model_prob,
            market_prob=market_prob,
            edge=model_prob - market_prob,
            confidence=0.9,
            event=event,
            timestamp=datetime.now(timezone.utc),
        )

    def _on_series_ended(self, event: GameEvent, market: TrackedMarket) -> Signal:
        # Series is over - probability is 1.0 or 0.0
        model_prob = 1.0 if event.winning_side == "team_a" else 0.0
        market_prob = market.yes_price

        return Signal(
            market=market,
            model_prob=model_prob,
            market_prob=market_prob,
            edge=model_prob - market_prob,
            confidence=1.0,
            event=event,
            timestamp=datetime.now(timezone.utc),
        )
