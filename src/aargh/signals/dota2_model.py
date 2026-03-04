"""Dota 2 win probability model based on gold/XP lead, game time, and objectives.

Uses a logistic regression-style model mapping net gold lead to win
probability. Enhanced with objective-based probability adjustments for:
- Roshan kills and Aegis possession
- Tower/barracks destruction
- XP lead divergence

Historical data shows gold lead combined with structural advantages
(towers, barracks, Aegis) is the strongest predictor of Dota 2 outcomes.
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

# Logistic model parameters
GOLD_SCALE = 15000.0

# Objective probability adjustments
ROSHAN_KILL_SHIFT = 0.05
AEGIS_SHIFT = 0.08
TOWER_SHIFT = {1: 0.02, 2: 0.03, 3: 0.05, 4: 0.08}
BARRACKS_SHIFT = 0.06

# XP lead provides additional signal beyond gold
XP_SCALE = 20000.0
XP_WEIGHT = 0.3


def _sigmoid(x: float) -> float:
    return 1.0 / (1.0 + math.exp(-x))


def gold_lead_to_win_prob(gold_lead: int, xp_lead: int | None = None) -> float:
    """Convert net gold lead (positive = team_a ahead) to win probability.

    Optionally incorporates XP lead as secondary signal.
    """
    gold_signal = gold_lead / GOLD_SCALE

    if xp_lead is not None:
        xp_signal = xp_lead / XP_SCALE
        combined = (1 - XP_WEIGHT) * gold_signal + XP_WEIGHT * xp_signal
    else:
        combined = gold_signal

    return _sigmoid(combined)


def objective_adjusted_prob(base_prob: float, event: GameEvent) -> float:
    """Adjust win probability based on Dota 2 objectives."""
    adjustment = 0.0

    if event.event_type == EventType.ROSHAN_KILL:
        shift = ROSHAN_KILL_SHIFT
        if event.roshan_number and event.roshan_number >= 3:
            shift *= 1.5
        if event.winning_side == "team_a":
            adjustment += shift
        else:
            adjustment -= shift

    if event.event_type == EventType.AEGIS_PICKUP:
        if event.aegis_holder == "team_a":
            adjustment += AEGIS_SHIFT
        elif event.aegis_holder == "team_b":
            adjustment -= AEGIS_SHIFT

    if event.event_type == EventType.TOWER_DESTROY:
        tier = event.objective_tier or 1
        shift = TOWER_SHIFT.get(tier, 0.02)
        if event.winning_side == "team_a":
            adjustment += shift
        else:
            adjustment -= shift

    if event.event_type == EventType.BARRACKS_DESTROY:
        if event.winning_side == "team_a":
            adjustment += BARRACKS_SHIFT
        else:
            adjustment -= BARRACKS_SHIFT

    return max(0.02, min(0.98, base_prob + adjustment))


class Dota2Model(BaseSignalModel):
    """Dota 2 probability model using gold/XP differential and objectives."""

    def __init__(self):
        self._last_base_prob: dict[str, float] = {}

    @property
    def game(self) -> str:
        return "dota2"

    def evaluate(self, event: GameEvent, market: TrackedMarket) -> Signal | None:
        if event.event_type == EventType.GOLD_UPDATE and event.gold_lead is not None:
            return self._on_gold_update(event, market)
        if event.event_type in (
            EventType.ROSHAN_KILL, EventType.AEGIS_PICKUP,
            EventType.TOWER_DESTROY, EventType.BARRACKS_DESTROY,
            EventType.OBJECTIVE,
        ):
            return self._on_objective(event, market)
        if event.event_type == EventType.MAP_ENDED:
            return self._on_map_ended(event, market)
        if event.event_type == EventType.SERIES_ENDED:
            return self._on_series_ended(event, market)
        return None

    def _on_gold_update(self, event: GameEvent, market: TrackedMarket) -> Signal:
        current_map_prob = gold_lead_to_win_prob(event.gold_lead or 0, event.xp_lead)
        self._last_base_prob[event.match_id] = current_map_prob

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

    def _on_objective(self, event: GameEvent, market: TrackedMarket) -> Signal:
        """Handle Roshan, tower, barracks destruction events."""
        maps_a = event.team_a_maps or 0
        maps_b = event.team_b_maps or 0

        base_prob = self._last_base_prob.get(event.match_id, 0.5)
        current_map_prob = objective_adjusted_prob(base_prob, event)

        model_prob = series_win_prob(maps_a, maps_b, current_map_prob, market.best_of)
        market_prob = market.yes_price

        confidence = 0.8
        if event.event_type in (EventType.BARRACKS_DESTROY, EventType.ROSHAN_KILL):
            confidence = 0.85

        return Signal(
            market=market,
            model_prob=model_prob,
            market_prob=market_prob,
            edge=model_prob - market_prob,
            confidence=confidence,
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
