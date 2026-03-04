"""CS2 win probability model based on round scores, economy, and mid-round state.

Uses dynamic programming to compute P(team A wins map) given current round
scores in MR12 format (first to 13 rounds). Adjusts probabilities for:
- Bomb plant situations (post-plant advantage)
- Economy state (eco/force vs full buy)
- Player count advantage (clutch situations)
- Pistol round significance

Combines map probabilities into series (Bo1/Bo3/Bo5) probabilities.
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

# Economy thresholds (approximate team equipment value in $)
ECO_THRESHOLD = 10000  # Below this, team is on eco
FORCE_THRESHOLD = 18000  # Below this, team is on a force buy
FULL_BUY_THRESHOLD = 25000  # Above this, team has a full buy

# Per-round win probability adjustments
ECO_DISADVANTAGE = 0.25  # Team on eco wins ~25% of rounds
FORCE_DISADVANTAGE = 0.38  # Team on force wins ~38% of rounds
PISTOL_VOLATILITY = 0.50  # Pistol rounds are close to 50/50 regardless

# Bomb plant shifts
BOMB_PLANTED_SHIFT = 0.12  # Planting team gains ~12% win probability for the round

# Player count advantage (per player advantage)
PLAYER_ADVANTAGE_PER_PLAYER = 0.10  # Each extra player adds ~10% round win probability


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
            diff = score_a - score_b
            return 0.5 + diff * 0.1
        diff = score_b - score_a
        return max(0.0, 0.5 - diff * 0.1)
    # Normal play: recurse
    return p * map_win_prob(score_a + 1, score_b, p) + (1 - p) * map_win_prob(score_a, score_b + 1, p)


@lru_cache(maxsize=256)
def series_win_prob(maps_a: int, maps_b: int, current_map_prob: float, best_of: int) -> float:
    """Probability that team A wins a best-of-N series."""
    maps_needed = (best_of + 1) // 2

    if maps_a >= maps_needed:
        return 1.0
    if maps_b >= maps_needed:
        return 0.0

    future_map_prob = 0.5
    p_win_current = current_map_prob
    p_lose_current = 1.0 - current_map_prob

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


def _economy_adjusted_p(event: GameEvent) -> float:
    """Adjust per-round win probability based on economy state."""
    p = 0.5

    if event.is_pistol_round:
        return PISTOL_VOLATILITY

    eco_a = event.team_a_economy or 0
    eco_b = event.team_b_economy or 0

    if eco_a > 0 and eco_b > 0:
        if eco_a < ECO_THRESHOLD and eco_b >= FULL_BUY_THRESHOLD:
            p = ECO_DISADVANTAGE
        elif eco_a < FORCE_THRESHOLD and eco_b >= FULL_BUY_THRESHOLD:
            p = FORCE_DISADVANTAGE
        elif eco_b < ECO_THRESHOLD and eco_a >= FULL_BUY_THRESHOLD:
            p = 1.0 - ECO_DISADVANTAGE
        elif eco_b < FORCE_THRESHOLD and eco_a >= FULL_BUY_THRESHOLD:
            p = 1.0 - FORCE_DISADVANTAGE
    elif event.is_eco_round:
        p = ECO_DISADVANTAGE

    return p


def _player_count_adjusted_p(base_p: float, alive_a: int, alive_b: int) -> float:
    """Adjust round win probability based on players alive."""
    if alive_a <= 0 and alive_b <= 0:
        return base_p
    if alive_a <= 0:
        return 0.0
    if alive_b <= 0:
        return 1.0

    advantage = alive_a - alive_b
    adjustment = advantage * PLAYER_ADVANTAGE_PER_PLAYER
    return max(0.02, min(0.98, base_p + adjustment))


def _bomb_adjusted_p(base_p: float, bomb_planted: bool, planting_side: str | None) -> float:
    """Adjust round win probability based on bomb plant status."""
    if not bomb_planted:
        return base_p
    if planting_side == "team_a":
        return min(0.98, base_p + BOMB_PLANTED_SHIFT)
    elif planting_side == "team_b":
        return max(0.02, base_p - BOMB_PLANTED_SHIFT)
    return base_p


class CS2Model(BaseSignalModel):
    """CS2-specific probability model using round scores, economy, and mid-round state."""

    def __init__(self):
        self._last_round_p: dict[str, float] = {}

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
        if event.event_type == EventType.ECONOMY_UPDATE:
            return self._on_economy_update(event, market)
        if event.event_type == EventType.BOMB_PLANTED:
            return self._on_bomb_planted(event, market)
        if event.event_type in (EventType.PLAYER_COUNT_UPDATE, EventType.CLUTCH_SITUATION):
            return self._on_player_count(event, market)
        return None

    def _on_round_ended(self, event: GameEvent, market: TrackedMarket) -> Signal:
        score_a = event.team_a_rounds or 0
        score_b = event.team_b_rounds or 0
        maps_a = event.team_a_maps or 0
        maps_b = event.team_b_maps or 0

        current_map_prob = map_win_prob(score_a, score_b)
        model_prob = series_win_prob(maps_a, maps_b, current_map_prob, market.best_of)
        market_prob = market.yes_price

        return Signal(
            market=market,
            model_prob=model_prob,
            market_prob=market_prob,
            edge=model_prob - market_prob,
            confidence=0.8,
            event=event,
            timestamp=datetime.now(timezone.utc),
        )

    def _on_economy_update(self, event: GameEvent, market: TrackedMarket) -> Signal:
        """Economy differential signal - eco rounds won only ~25% of the time."""
        score_a = event.team_a_rounds or 0
        score_b = event.team_b_rounds or 0
        maps_a = event.team_a_maps or 0
        maps_b = event.team_b_maps or 0

        p = _economy_adjusted_p(event)
        self._last_round_p[event.match_id] = p

        current_map_prob = map_win_prob(score_a, score_b, p)
        model_prob = series_win_prob(maps_a, maps_b, current_map_prob, market.best_of)
        market_prob = market.yes_price

        confidence = 0.7 if abs(p - 0.5) > 0.1 else 0.5

        return Signal(
            market=market,
            model_prob=model_prob,
            market_prob=market_prob,
            edge=model_prob - market_prob,
            confidence=confidence,
            event=event,
            timestamp=datetime.now(timezone.utc),
        )

    def _on_bomb_planted(self, event: GameEvent, market: TrackedMarket) -> Signal:
        """Bomb plant gives planting team ~62% chance to win round."""
        score_a = event.team_a_rounds or 0
        score_b = event.team_b_rounds or 0
        maps_a = event.team_a_maps or 0
        maps_b = event.team_b_maps or 0

        base_p = self._last_round_p.get(event.match_id, 0.5)
        p = _bomb_adjusted_p(base_p, True, event.winning_side)

        current_map_prob = map_win_prob(score_a, score_b, p)
        model_prob = series_win_prob(maps_a, maps_b, current_map_prob, market.best_of)
        market_prob = market.yes_price

        return Signal(
            market=market,
            model_prob=model_prob,
            market_prob=market_prob,
            edge=model_prob - market_prob,
            confidence=0.65,
            event=event,
            timestamp=datetime.now(timezone.utc),
        )

    def _on_player_count(self, event: GameEvent, market: TrackedMarket) -> Signal:
        """Player count differential mid-round. 5v3 shifts probability significantly."""
        score_a = event.team_a_rounds or 0
        score_b = event.team_b_rounds or 0
        maps_a = event.team_a_maps or 0
        maps_b = event.team_b_maps or 0
        alive_a = event.team_a_alive if event.team_a_alive is not None else 5
        alive_b = event.team_b_alive if event.team_b_alive is not None else 5

        base_p = self._last_round_p.get(event.match_id, 0.5)
        p = _player_count_adjusted_p(base_p, alive_a, alive_b)

        if event.bomb_planted:
            p = _bomb_adjusted_p(p, True, event.winning_side)

        current_map_prob = map_win_prob(score_a, score_b, p)
        model_prob = series_win_prob(maps_a, maps_b, current_map_prob, market.best_of)
        market_prob = market.yes_price

        player_diff = abs(alive_a - alive_b)
        confidence = min(0.85, 0.5 + player_diff * 0.1)

        if event.event_type == EventType.CLUTCH_SITUATION:
            confidence = min(0.9, confidence + 0.1)

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
