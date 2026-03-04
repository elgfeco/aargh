"""Tests for match linking and team name normalization."""

from aargh.matching.normalization import normalize_team_name


class TestNormalization:
    def test_canonical_name(self):
        assert normalize_team_name("Natus Vincere") == "Natus Vincere"

    def test_alias_navi(self):
        assert normalize_team_name("NaVi") == "Natus Vincere"
        assert normalize_team_name("NAVI") == "Natus Vincere"
        assert normalize_team_name("Na'Vi") == "Natus Vincere"

    def test_alias_g2(self):
        assert normalize_team_name("G2") == "G2 Esports"
        assert normalize_team_name("G2 Esports") == "G2 Esports"

    def test_alias_faze(self):
        assert normalize_team_name("FaZe") == "FaZe Clan"
        assert normalize_team_name("faze") == "FaZe Clan"

    def test_suffix_stripping(self):
        assert normalize_team_name("Fnatic Esports") == "Fnatic"

    def test_unknown_team_passthrough(self):
        assert normalize_team_name("Unknown Team XYZ") == "Unknown Team XYZ"

    def test_whitespace_handling(self):
        assert normalize_team_name("  NaVi  ") == "Natus Vincere"

    def test_team_liquid(self):
        assert normalize_team_name("Liquid") == "Team Liquid"
        assert normalize_team_name("TL") == "Team Liquid"

    def test_dota2_teams(self):
        assert normalize_team_name("Secret") == "Team Secret"
        assert normalize_team_name("Tundra") == "Tundra Esports"
        assert normalize_team_name("LGD") == "PSG.LGD"
