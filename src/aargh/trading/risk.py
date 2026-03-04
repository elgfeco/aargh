"""Risk management: Kelly criterion position sizing and exposure limits."""

from __future__ import annotations

import logging
from dataclasses import dataclass

from aargh.config import Config
from aargh.signals.base_model import Signal

logger = logging.getLogger(__name__)


@dataclass
class BetOrder:
    """A sized bet order ready for execution."""

    token_id: str
    side: str           # "BUY" or "SELL"
    amount: float       # Dollar amount to bet
    price: float        # Current market price
    signal: Signal

    @property
    def expected_profit(self) -> float:
        """Expected profit if model probability is correct."""
        if self.side == "BUY":
            payout = self.amount / self.price  # shares received
            return self.signal.model_prob * payout - self.amount
        return 0.0


def kelly_stake(
    model_prob: float,
    market_price: float,
    kelly_fraction: float,
    bankroll: float,
) -> float:
    """Compute Kelly criterion bet size.

    For binary market at price p, buying Yes:
    - Net odds: b = (1 - p) / p
    - Kelly: f* = (model_prob * b - (1 - model_prob)) / b
    - Fractional Kelly: kelly_fraction * f* * bankroll

    Returns 0 if no edge.
    """
    if model_prob <= market_price:
        return 0.0
    if market_price <= 0 or market_price >= 1:
        return 0.0

    b = (1.0 - market_price) / market_price
    f_star = (model_prob * b - (1.0 - model_prob)) / b
    f_star = max(0.0, min(f_star, 1.0))
    return kelly_fraction * f_star * bankroll


class RiskManager:
    """Evaluates signals and produces sized bet orders within risk limits."""

    def __init__(self, config: Config):
        self._bankroll = config.bankroll
        self._max_bet_pct = config.max_bet_pct
        self._kelly_fraction = config.kelly_fraction
        self._max_exposure_pct = config.max_exposure_pct
        self._min_edge = config.min_edge
        self._current_exposure = 0.0

    def evaluate(self, signal: Signal) -> BetOrder | None:
        """Evaluate a signal and return a BetOrder if it passes risk checks."""
        # Check minimum edge
        if signal.abs_edge < self._min_edge:
            return None

        # Determine direction
        if signal.edge > 0:
            # Model says higher than market -> BUY Yes
            side = "BUY"
            price = signal.market_prob
            stake = kelly_stake(
                signal.model_prob, signal.market_prob,
                self._kelly_fraction, self._bankroll,
            )
            token_id = signal.market.yes_token
        else:
            # Model says lower than market -> BUY No (equivalent to SELL Yes)
            side = "BUY"
            price = signal.market.no_price
            stake = kelly_stake(
                1.0 - signal.model_prob, 1.0 - signal.market_prob,
                self._kelly_fraction, self._bankroll,
            )
            token_id = signal.market.no_token

        if stake <= 0:
            return None

        # Cap at max single bet
        max_bet = self._bankroll * self._max_bet_pct
        stake = min(stake, max_bet)

        # Check total exposure
        max_exposure = self._bankroll * self._max_exposure_pct
        if self._current_exposure + stake > max_exposure:
            remaining = max_exposure - self._current_exposure
            if remaining <= 0:
                logger.warning("Max exposure reached (%.2f), skipping bet", self._current_exposure)
                return None
            stake = remaining

        # Minimum bet size sanity check
        if stake < 0.10:
            return None

        logger.info(
            "Risk approved: %s $%.2f @ %.3f (edge=%+.1f%%, Kelly=%.1f%%)",
            side, stake, price, signal.edge * 100,
            kelly_stake(signal.model_prob, signal.market_prob, 1.0, 100) / 100 * 100,
        )

        return BetOrder(
            token_id=token_id,
            side=side,
            amount=round(stake, 2),
            price=price,
            signal=signal,
        )

    def record_bet(self, order: BetOrder) -> None:
        """Update exposure after a bet is placed."""
        self._current_exposure += order.amount

    def release_exposure(self, amount: float) -> None:
        """Release exposure when a market resolves."""
        self._current_exposure = max(0, self._current_exposure - amount)

    @property
    def current_exposure(self) -> float:
        return self._current_exposure

    @property
    def bankroll(self) -> float:
        return self._bankroll
