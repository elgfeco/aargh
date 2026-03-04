"""Tests for the CS2 win probability model."""

from aargh.feeds.events import EventType, GameEvent
from aargh.market.models import GammaMarket, TrackedMarket
from aargh.signals.cs2_model import CS2Model, map_win_prob, series_win_prob


def _make_market(yes_price: float = 0.5) -> TrackedMarket:
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
        team_a="Natus Vincere",
        team_b="FaZe Clan",
        game="cs2",
        best_of=3,
    )


class TestMapWinProb:
    def test_team_a_already_won(self):
        assert map_win_prob(13, 5) == 1.0

    def test_team_b_already_won(self):
        assert map_win_prob(5, 13) == 0.0

    def test_equal_score_is_balanced(self):
        p = map_win_prob(6, 6)
        assert 0.45 <= p <= 0.55

    def test_overtime_equal(self):
        p = map_win_prob(12, 12)
        assert p == 0.5

    def test_large_lead_high_probability(self):
        p = map_win_prob(12, 3)
        assert p > 0.95

    def test_small_lead_slight_advantage(self):
        p = map_win_prob(7, 5)
        assert 0.5 < p < 0.85

    def test_zero_zero_is_balanced(self):
        p = map_win_prob(0, 0)
        assert 0.45 <= p <= 0.55

    def test_monotonic_with_score_a(self):
        """As team A's score increases, their probability should increase."""
        probs = [map_win_prob(a, 5) for a in range(0, 13)]
        for i in range(1, len(probs)):
            assert probs[i] >= probs[i - 1]

    def test_monotonic_with_score_b(self):
        """As team B's score increases, team A's probability should decrease."""
        probs = [map_win_prob(5, b) for b in range(0, 13)]
        for i in range(1, len(probs)):
            assert probs[i] <= probs[i - 1]

    def test_symmetry(self):
        """P(A wins | a, b) should equal 1 - P(A wins | b, a)."""
        for a in range(0, 13):
            for b in range(0, 13):
                assert abs(map_win_prob(a, b) + map_win_prob(b, a) - 1.0) < 0.01


class TestSeriesWinProb:
    def test_bo1_equals_map_prob(self):
        p = series_win_prob(0, 0, 0.6, 1)
        assert abs(p - 0.6) < 0.01

    def test_bo3_team_a_up_one_map(self):
        p = series_win_prob(1, 0, 0.5, 3)
        assert p > 0.6  # Need 1 more map vs opponent needs 2

    def test_bo3_team_a_won(self):
        assert series_win_prob(2, 0, 0.5, 3) == 1.0

    def test_bo3_team_b_won(self):
        assert series_win_prob(0, 2, 0.5, 3) == 0.0

    def test_bo3_equal_maps_balanced(self):
        p = series_win_prob(1, 1, 0.5, 3)
        assert 0.45 <= p <= 0.55

    def test_high_map_prob_increases_series_prob(self):
        low = series_win_prob(0, 0, 0.3, 3)
        high = series_win_prob(0, 0, 0.7, 3)
        assert high > low


class TestCS2Model:
    def test_round_ended_produces_signal(self):
        model = CS2Model()
        market = _make_market(0.5)
        event = GameEvent(
            event_type=EventType.ROUND_ENDED,
            match_id="match-1",
            team_a_rounds=10,
            team_b_rounds=5,
            team_a_maps=0,
            team_b_maps=0,
            best_of=3,
        )
        signal = model.evaluate(event, market)
        assert signal is not None
        assert signal.model_prob > 0.5  # team_a leading
        assert signal.edge > 0  # model says more likely than 0.5

    def test_series_ended_produces_definitive_signal(self):
        model = CS2Model()
        market = _make_market(0.7)
        event = GameEvent(
            event_type=EventType.SERIES_ENDED,
            match_id="match-1",
            winning_side="team_a",
        )
        signal = model.evaluate(event, market)
        assert signal is not None
        assert signal.model_prob == 1.0
        assert signal.confidence == 1.0

    def test_irrelevant_event_returns_none(self):
        model = CS2Model()
        market = _make_market()
        event = GameEvent(
            event_type=EventType.KILL,
            match_id="match-1",
        )
        signal = model.evaluate(event, market)
        assert signal is None
