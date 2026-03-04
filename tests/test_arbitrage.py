"""Tests for arbitrage detection."""

import time

from aargh.feeds.events import EventType, GameEvent
from aargh.market.models import GammaMarket, TrackedMarket
from aargh.signals.arbitrage import ArbitrageDetector, ArbitrageOpportunity
from aargh.signals.base_model import Signal
from datetime import datetime, timezone


def _make_market(
    market_id: str = "m1",
    question: str = "Will NaVi win?",
    yes_price: float = 0.5,
    no_price: float = 0.5,
    team_a: str = "NaVi",
    team_b: str = "FaZe",
) -> TrackedMarket:
    return TrackedMarket(
        market=GammaMarket(
            id=market_id, question=question, condition_id="c1",
            slug="test", outcomes=["Yes", "No"],
            outcome_prices=[yes_price, no_price],
            clob_token_ids=["yes-tok", "no-tok"],
            active=True, closed=False,
        ),
        team_a=team_a, team_b=team_b, game="cs2", best_of=3,
    )


def _make_signal(market: TrackedMarket, model_prob: float) -> Signal:
    event = GameEvent(event_type=EventType.ROUND_ENDED, match_id="match-1")
    return Signal(
        market=market, model_prob=model_prob,
        market_prob=market.yes_price,
        edge=model_prob - market.yes_price,
        confidence=0.8, event=event,
        timestamp=datetime.now(timezone.utc),
    )


class TestSameMarketArbitrage:
    def test_detects_underpriced_market(self):
        """Yes + No < 1.0 should be detected as arbitrage."""
        detector = ArbitrageDetector(min_profit_pct=0.1)
        market = _make_market(yes_price=0.45, no_price=0.45)  # Sum = 0.90
        opps = detector.scan_same_market([market])
        assert len(opps) == 1
        assert opps[0].arb_type == "same_market"
        assert opps[0].expected_profit_pct > 10  # ~11% profit

    def test_no_arb_when_prices_sum_to_one(self):
        detector = ArbitrageDetector(min_profit_pct=0.1)
        market = _make_market(yes_price=0.60, no_price=0.40)  # Sum = 1.0
        opps = detector.scan_same_market([market])
        assert len(opps) == 0

    def test_no_arb_when_overpriced(self):
        detector = ArbitrageDetector(min_profit_pct=0.1)
        market = _make_market(yes_price=0.55, no_price=0.50)  # Sum = 1.05
        opps = detector.scan_same_market([market])
        assert len(opps) == 0

    def test_respects_min_profit_threshold(self):
        detector = ArbitrageDetector(min_profit_pct=5.0)
        # Sum = 0.98, profit = ~2% which is below 5% threshold
        market = _make_market(yes_price=0.49, no_price=0.49)
        opps = detector.scan_same_market([market])
        assert len(opps) == 0


class TestTemporalArbitrage:
    def test_detects_stale_price(self):
        detector = ArbitrageDetector(min_profit_pct=0.5, stale_threshold_s=0.01)
        market = _make_market(yes_price=0.50)
        # Record event time in the past
        detector.record_event_time("match-1", time.monotonic() - 60)
        signal = _make_signal(market, model_prob=0.80)
        opps = detector.scan_temporal([market], [signal])
        assert len(opps) >= 1
        assert opps[0].arb_type == "temporal"
        assert opps[0].stale_duration_s >= 30

    def test_no_temporal_arb_for_fresh_price(self):
        detector = ArbitrageDetector(min_profit_pct=0.5, stale_threshold_s=60)
        market = _make_market(yes_price=0.50)
        # Event happened just now
        detector.record_event_time("match-1", time.monotonic())
        signal = _make_signal(market, model_prob=0.80)
        opps = detector.scan_temporal([market], [signal])
        assert len(opps) == 0


class TestCrossMarketArbitrage:
    def test_detects_tournament_vs_match_inconsistency(self):
        detector = ArbitrageDetector(min_profit_pct=0.5)
        match_market = _make_market(
            market_id="m1",
            question="Will NaVi win vs FaZe?",
            yes_price=0.60, no_price=0.40,
            team_a="NaVi", team_b="FaZe",
        )
        tournament_market = _make_market(
            market_id="m2",
            question="Will NaVi win the tournament championship?",
            yes_price=0.70, no_price=0.30,  # Higher than match price!
            team_a="NaVi", team_b="Others",
        )
        opps = detector.scan_cross_market([match_market, tournament_market])
        assert len(opps) >= 1
        assert opps[0].arb_type == "cross_market"

    def test_no_arb_when_consistent(self):
        detector = ArbitrageDetector(min_profit_pct=0.5)
        match_market = _make_market(
            market_id="m1",
            question="Will NaVi win vs FaZe?",
            yes_price=0.70, no_price=0.30,
            team_a="NaVi", team_b="FaZe",
        )
        tournament_market = _make_market(
            market_id="m2",
            question="Will NaVi win the tournament?",
            yes_price=0.40, no_price=0.60,  # Lower than match = consistent
            team_a="NaVi", team_b="Others",
        )
        opps = detector.scan_cross_market([match_market, tournament_market])
        assert len(opps) == 0


class TestScanAll:
    def test_combines_all_arb_types(self):
        detector = ArbitrageDetector(min_profit_pct=0.1, stale_threshold_s=0.01)
        underpriced = _make_market("m1", "Will A win?", 0.45, 0.45)
        detector.record_event_time("match-1", time.monotonic() - 60)
        signal = _make_signal(
            _make_market("m2", "Will B win?", 0.50, 0.50), model_prob=0.80
        )
        opps = detector.scan_all([underpriced], [signal])
        assert len(opps) >= 1  # At least same-market arb

    def test_price_staleness_tracking(self):
        detector = ArbitrageDetector()
        detector.record_price("m1", 0.60, 0.40)
        detector.record_price("m1", 0.60, 0.40)  # Same price
        staleness = detector.get_price_staleness("m1")
        assert staleness >= 0
