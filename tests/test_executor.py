"""Tests for risk management and Kelly criterion."""

from aargh.config import Config
from aargh.feeds.events import EventType, GameEvent
from aargh.market.models import GammaMarket, TrackedMarket
from aargh.signals.base_model import Signal
from aargh.trading.risk import RiskManager, kelly_stake
from datetime import datetime, timezone


def _make_signal(model_prob: float, market_prob: float) -> Signal:
    market = TrackedMarket(
        market=GammaMarket(
            id="m1", question="Test?", condition_id="c1", slug="test",
            outcomes=["Yes", "No"],
            outcome_prices=[market_prob, 1.0 - market_prob],
            clob_token_ids=["yes-tok", "no-tok"],
            active=True, closed=False,
        ),
        team_a="A", team_b="B", game="cs2", best_of=3,
    )
    event = GameEvent(event_type=EventType.ROUND_ENDED, match_id="m1")
    return Signal(
        market=market,
        model_prob=model_prob,
        market_prob=market_prob,
        edge=model_prob - market_prob,
        confidence=0.8,
        event=event,
        timestamp=datetime.now(timezone.utc),
    )


class TestKellyStake:
    def test_no_edge_returns_zero(self):
        assert kelly_stake(0.5, 0.5, 0.25, 1000) == 0.0
        assert kelly_stake(0.3, 0.5, 0.25, 1000) == 0.0

    def test_positive_edge_returns_positive(self):
        stake = kelly_stake(0.7, 0.5, 0.25, 1000)
        assert stake > 0

    def test_larger_edge_larger_stake(self):
        small = kelly_stake(0.55, 0.5, 0.25, 1000)
        big = kelly_stake(0.8, 0.5, 0.25, 1000)
        assert big > small

    def test_fraction_reduces_stake(self):
        full = kelly_stake(0.7, 0.5, 1.0, 1000)
        quarter = kelly_stake(0.7, 0.5, 0.25, 1000)
        assert abs(quarter - full * 0.25) < 0.01

    def test_boundary_prices(self):
        assert kelly_stake(0.5, 0.0, 0.25, 1000) == 0.0
        assert kelly_stake(0.5, 1.0, 0.25, 1000) == 0.0


class TestRiskManager:
    def _config(self, **overrides) -> Config:
        defaults = dict(
            bankroll=1000.0,
            max_bet_pct=0.05,
            kelly_fraction=0.25,
            max_exposure_pct=0.25,
            min_edge=0.05,
            dry_run=True,
        )
        defaults.update(overrides)
        return Config(**defaults)

    def test_rejects_small_edge(self):
        rm = RiskManager(self._config(min_edge=0.10))
        signal = _make_signal(model_prob=0.55, market_prob=0.50)
        assert rm.evaluate(signal) is None

    def test_approves_large_edge(self):
        rm = RiskManager(self._config(min_edge=0.05))
        signal = _make_signal(model_prob=0.75, market_prob=0.50)
        order = rm.evaluate(signal)
        assert order is not None
        assert order.amount > 0
        assert order.side == "BUY"

    def test_caps_at_max_bet(self):
        rm = RiskManager(self._config(max_bet_pct=0.02, kelly_fraction=1.0))
        signal = _make_signal(model_prob=0.95, market_prob=0.50)
        order = rm.evaluate(signal)
        assert order is not None
        assert order.amount <= 1000 * 0.02

    def test_respects_exposure_limit(self):
        rm = RiskManager(self._config(max_exposure_pct=0.10))
        # Place first bet
        signal = _make_signal(model_prob=0.75, market_prob=0.50)
        order1 = rm.evaluate(signal)
        assert order1 is not None
        rm.record_bet(order1)

        # Keep placing bets until exposure limit hit
        total = order1.amount
        for _ in range(50):
            order = rm.evaluate(signal)
            if order is None:
                break
            rm.record_bet(order)
            total += order.amount

        assert total <= 1000 * 0.10 + 1  # Allow small rounding
