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

    def test_cs2_bomb_plant_signal(self):
        engine = SignalEngine(min_edge=0.01)
        market = _make_market("cs2", yes_price=0.5)
        event = GameEvent(
            event_type=EventType.BOMB_PLANTED,
            match_id="m1",
            team_a_rounds=6,
            team_b_rounds=6,
            winning_side="team_a",
        )
        signal = engine.process(event, market)
        assert signal is not None

    def test_cs2_economy_signal(self):
        engine = SignalEngine(min_edge=0.01)
        market = _make_market("cs2", yes_price=0.5)
        event = GameEvent(
            event_type=EventType.ECONOMY_UPDATE,
            match_id="m1",
            team_a_rounds=5,
            team_b_rounds=5,
            team_a_economy=5000,
            team_b_economy=30000,
        )
        signal = engine.process(event, market)
        assert signal is not None
        assert signal.model_prob < 0.5  # Team A on eco

    def test_dota2_roshan_signal(self):
        engine = SignalEngine(min_edge=0.01)
        market = _make_market("dota2", yes_price=0.5)
        # First set a gold lead baseline
        engine.process(GameEvent(
            event_type=EventType.GOLD_UPDATE,
            match_id="m1",
            gold_lead=5000,
            team_a_maps=0, team_b_maps=0,
        ), market)
        # Then Roshan kill
        event = GameEvent(
            event_type=EventType.ROSHAN_KILL,
            match_id="m1",
            winning_side="team_a",
            roshan_number=1,
            team_a_maps=0, team_b_maps=0,
        )
        signal = engine.process(event, market)
        assert signal is not None
        assert signal.model_prob > 0.5

    def test_dota2_tower_destroy_signal(self):
        engine = SignalEngine(min_edge=0.01)
        market = _make_market("dota2", yes_price=0.5)
        event = GameEvent(
            event_type=EventType.TOWER_DESTROY,
            match_id="m1",
            winning_side="team_a",
            objective_tier=3,
            team_a_maps=0, team_b_maps=0,
        )
        signal = engine.process(event, market)
        assert signal is not None

    def test_cs2_player_count_signal(self):
        engine = SignalEngine(min_edge=0.01)
        market = _make_market("cs2", yes_price=0.5)
        event = GameEvent(
            event_type=EventType.PLAYER_COUNT_UPDATE,
            match_id="m1",
            team_a_rounds=6,
            team_b_rounds=6,
            team_a_alive=5,
            team_b_alive=1,
        )
        signal = engine.process(event, market)
        assert signal is not None
        assert signal.model_prob > 0.5
