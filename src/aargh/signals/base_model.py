"""Abstract interface for game-specific signal models."""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from datetime import datetime, timezone

from aargh.feeds.events import GameEvent
from aargh.market.models import TrackedMarket


@dataclass
class Signal:
    """A trading signal emitted when model probability diverges from market price."""

    market: TrackedMarket
    model_prob: float       # Our estimated probability for team_a / Yes
    market_prob: float      # Current Polymarket price
    edge: float             # model_prob - market_prob
    confidence: float       # 0.0 to 1.0
    event: GameEvent
    timestamp: datetime

    @property
    def direction(self) -> str:
        """BUY if model says higher probability than market, SELL otherwise."""
        return "BUY" if self.edge > 0 else "SELL"

    @property
    def abs_edge(self) -> float:
        return abs(self.edge)


class BaseSignalModel(ABC):
    """Interface for game-specific probability models."""

    @property
    @abstractmethod
    def game(self) -> str:
        """Which game this model handles (e.g., 'cs2', 'dota2')."""
        ...

    @abstractmethod
    def evaluate(self, event: GameEvent, market: TrackedMarket) -> Signal | None:
        """Evaluate an event and return a Signal if probability changed significantly."""
        ...
