"""Tests for market scanner and question parsing."""

from aargh.market.models import GammaMarket, TrackedMarket
from aargh.market.scanner import _extract_teams


class TestExtractTeams:
    def test_will_win_pattern(self):
        a, b, t = _extract_teams("Will Natus Vincere win vs FaZe Clan in BLAST Open?")
        assert "Natus Vincere" in a
        assert "FaZe" in b

    def test_beat_pattern(self):
        a, b, t = _extract_teams("Will Team Vitality beat G2 Esports?")
        assert "Vitality" in a
        assert "G2" in b

    def test_vs_separator(self):
        a, b, t = _extract_teams("Something about NaVi vs FaZe match")
        assert a != ""
        assert b != ""

    def test_tournament_extraction(self):
        a, b, t = _extract_teams("Will NaVi win vs FaZe in IEM Katowice?")
        assert t is not None
        assert "Katowice" in t or "IEM" in t


class TestGammaMarket:
    def test_from_api_parses_json_strings(self):
        data = {
            "id": "12345",
            "question": "Will NaVi win?",
            "conditionId": "cond-1",
            "slug": "navi-win",
            "outcomes": '["Yes","No"]',
            "outcomePrices": '["0.65","0.35"]',
            "clobTokenIds": '["tok-yes","tok-no"]',
            "active": True,
            "closed": False,
            "liquidity": "50000",
            "volume": "100000",
        }
        gm = GammaMarket.from_api(data)
        assert gm.id == "12345"
        assert gm.outcomes == ["Yes", "No"]
        assert gm.outcome_prices == [0.65, 0.35]
        assert gm.clob_token_ids == ["tok-yes", "tok-no"]
        assert gm.liquidity == 50000.0

    def test_from_api_handles_native_lists(self):
        data = {
            "id": "123",
            "question": "Test?",
            "conditionId": "c1",
            "slug": "test",
            "outcomes": ["Yes", "No"],
            "outcomePrices": [0.5, 0.5],
            "clobTokenIds": ["a", "b"],
            "active": True,
            "closed": False,
        }
        gm = GammaMarket.from_api(data)
        assert gm.outcomes == ["Yes", "No"]
        assert gm.outcome_prices == [0.5, 0.5]


class TestTrackedMarket:
    def test_properties(self):
        gm = GammaMarket(
            id="1", question="Test", condition_id="c", slug="s",
            outcomes=["Yes", "No"], outcome_prices=[0.6, 0.4],
            clob_token_ids=["yes-tok", "no-tok"],
            active=True, closed=False,
        )
        tm = TrackedMarket(market=gm, team_a="A", team_b="B")
        assert tm.yes_token == "yes-tok"
        assert tm.no_token == "no-tok"
        assert tm.yes_price == 0.6
        assert tm.no_price == 0.4
