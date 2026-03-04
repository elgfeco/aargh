"""Signal engine that routes game events to game-specific probability models."""

from __future__ import annotations

import logging

from aargh.feeds.events import GameEvent
from aargh.market.models import TrackedMarket
from aargh.signals.base_model import BaseSignalModel, Signal
from aargh.signals.cs2_model import CS2Model
from aargh.signals.dota2_model import Dota2Model

logger = logging.getLogger(__name__)


class SignalEngine:
    """Routes events to the appropriate game model and filters by edge threshold."""

    def __init__(self, min_edge: float = 0.05):
        self._min_edge = min_edge
        self._models: dict[str, BaseSignalModel] = {}
        # Register built-in models
        for model_cls in (CS2Model, Dota2Model):
            model = model_cls()
            self._models[model.game] = model

    def register_model(self, model: BaseSignalModel) -> None:
        self._models[model.game] = model

    def process(self, event: GameEvent, market: TrackedMarket) -> Signal | None:
        """Process an event against a market, returning a Signal if edge exceeds threshold."""
        model = self._models.get(market.game)
        if not model:
            logger.debug("No model for game '%s'", market.game)
            return None

        signal = model.evaluate(event, market)
        if signal is None:
            return None

        if signal.abs_edge < self._min_edge:
            logger.debug(
                "Edge %.3f below threshold %.3f for %s",
                signal.abs_edge, self._min_edge, market.market.question[:40],
            )
            return None

        logger.info(
            "SIGNAL: %s edge=%+.1f%% model=%.3f market=%.3f [%s]",
            signal.direction,
            signal.edge * 100,
            signal.model_prob,
            signal.market_prob,
            market.market.question[:50],
        )
        return signal
