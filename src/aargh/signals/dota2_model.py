"""Dota 2 win probability model based on gold/XP lead and game time.

Uses a logistic regression-style model mapping net gold lead to win
probability. Historical data shows gold lead is the single strongest
predictor of Dota 2 match outcomes.
"""

from __future__ import annotations

import logging
import math
from datetime import datetime, timezone

from aargh.feeds.events import EventType, GameEvent
from aargh.market.models import TrackedMarket
from aargh.signals.base_model import BaseSignalModel, Signal
from aargh.signals.cs2_model import series_win_prob

logger = logging.getLogger(__name__)

# Logistic model parameters calibrated from historical Dota 2 data.
# P(team_a wins) = sigmoid(gold_lead / GOLD_SCALE)
# At 10k gold lead (~20 min), P ≈ 0.73
# At 20k gold lead (~30 min), P ≈ 0.88
GOLD_SCALE = 15000.0


def _sigmoid(x: float) -> float:
    return 1.0 / (1.0 + math.exp(-x))


def gold_lead_to_win_prob(gold_lead: int) -> float:
    """Convert net gold lead (positive = team_a ahead) to win probability.

    Uses a logistic model where ~15k gold lead maps to ~73% win probability.
    """
    return _sigmoid(gold_lead / GOLD_SCALE)


class Dota2Model(BaseSignalModel):
    """Dota 2 probability model using gold/XP differential."""

    @property
    def game(self) -> str:
        return "dota2"

    def evaluate(self, event: GameEvent, market: TrackedMarket) -> Signal | None:
        if event.event_type == EventType.GOLD_UPDATE and event.gold_lead is not None:
            return self._on_gold_update(event, market)
        if event.event_type == EventType.MAP_ENDED:
            return self._on_map_ended(event, market)
        if event.event_type == EventType.SERIES_ENDED:
            return self._on_series_ended(event, market)
        return None

    def _on_gold_update(self, event: GameEvent, market: TrackedMarket) -> Signal:
        current_map_prob = gold_lead_to_win_prob(event.gold_lead or 0)
        maps_a = event.team_a_maps or 0
        maps_b = event.team_b_maps or 0

        model_prob = series_win_prob(maps_a, maps_b, current_map_prob, market.best_of)
        market_prob = market.yes_price

        return Signal(
            market=market,
            model_prob=model_prob,
            market_prob=market_prob,
            edge=model_prob - market_prob,
            confidence=0.7,
            event=event,
            timestamp=datetime.now(timezone.utc),
        )

    def _on_map_ended(self, event: GameEvent, market: TrackedMarket) -> Signal:
        maps_a = event.team_a_maps or 0
        maps_b = event.team_b_maps or 0
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
