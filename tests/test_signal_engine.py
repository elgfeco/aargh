"""Tests for the signal engine routing and edge filtering."""

from aargh.feeds.events import EventType, GameEvent
from aargh.market.models import GammaMarket, TrackedMarket
from aargh.signals.engine import SignalEngine


def _make_market(game: str = "cs2", yes_price: float = 0.5) -> TrackedMarket:
    return TrackedMarket(
        market=GammaMarket(
            id="test-1",
            question="Will NaVi win vs FaZe?",
            condition_id="cond-1",
            slug="navi-vs-faze",
            outcomes=["Yes", "No"],
            outcome_prices=[yes_price, 1.0 - yes_price],
            clob_token_ids=["token-yes", "token-no"],
            active=True,
            closed=False,
        ),
        team_a="NaVi",
        team_b="FaZe",
        game=game,
        best_of=3,
    )


class TestSignalEngine:
    def test_processes_cs2_event_with_edge(self):
        engine = SignalEngine(min_edge=0.05)
        market = _make_market("cs2", yes_price=0.5)
        event = GameEvent(
            event_type=EventType.ROUND_ENDED,
            match_id="m1",
            team_a_rounds=12,
            team_b_rounds=3,
            team_a_maps=0,
            team_b_maps=0,
        )
        signal = engine.process(event, market)
        assert signal is not None
        assert signal.abs_edge >= 0.05

    def test_filters_small_edge(self):
        engine = SignalEngine(min_edge=0.05)
        # Price already reflects reality, small edge
        market = _make_market("cs2", yes_price=0.50)
        event = GameEvent(
            event_type=EventType.ROUND_ENDED,
            match_id="m1",
            team_a_rounds=6,
            team_b_rounds=6,
            team_a_maps=0,
            team_b_maps=0,
        )
        signal = engine.process(event, market)
        # At 6-6, model prob ~0.5, market price 0.5 -> edge ~0 -> filtered
        assert signal is None

    def test_unknown_game_returns_none(self):
        engine = SignalEngine(min_edge=0.05)
        market = _make_market("starcraft2", yes_price=0.5)
        event = GameEvent(
            event_type=EventType.ROUND_ENDED,
            match_id="m1",
        )
        signal = engine.process(event, market)
        assert signal is None

    def test_processes_dota2_event(self):
        engine = SignalEngine(min_edge=0.05)
        market = _make_market("dota2", yes_price=0.5)
        event = GameEvent(
            event_type=EventType.GOLD_UPDATE,
            match_id="m1",
            gold_lead=20000,
            team_a_maps=0,
            team_b_maps=0,
        )
        signal = engine.process(event, market)
        assert signal is not None
        assert signal.model_prob > 0.5
