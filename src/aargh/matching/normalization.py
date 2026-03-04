"""Team name normalization and alias resolution for esports teams."""

from __future__ import annotations

import re

# Canonical name -> known aliases
TEAM_ALIASES: dict[str, list[str]] = {
    "Natus Vincere": ["NaVi", "NAVI", "Na'Vi", "navi", "Natus vincere"],
    "FaZe Clan": ["FaZe", "faze"],
    "G2 Esports": ["G2", "g2"],
    "Team Vitality": ["Vitality", "VIT"],
    "Team Liquid": ["Liquid", "TL"],
    "Cloud9": ["C9", "cloud9"],
    "Astralis": ["astralis", "AST"],
    "MOUZ": ["mouz", "mousesports", "MOUZ NXT"],
    "Heroic": ["heroic", "HEROIC"],
    "FURIA": ["FURIA Esports", "furia"],
    "Virtus.pro": ["VP", "Virtus Pro", "virtus.pro"],
    "Complexity": ["compLexity", "COL", "Complexity Gaming"],
    "ENCE": ["ence", "ENCE eSports"],
    "Fnatic": ["fnatic", "FNC"],
    "Ninjas in Pyjamas": ["NiP", "NIP", "Ninjas In Pyjamas"],
    "BIG": ["BIG Clan", "big"],
    "OG": ["OG Esports", "og"],
    "Team Spirit": ["Spirit", "TSpirit"],
    "Monte": ["monte", "MONTE"],
    "GamerLegion": ["GL", "Gamer Legion"],
    "9z Team": ["9z", "9Z"],
    "paiN Gaming": ["paiN", "pain"],
    "MIBR": ["mibr", "Made in Brazil"],
    "Imperial": ["Imperial Esports", "imperial"],
    "Eternal Fire": ["EF", "eternal fire"],
    "SAW": ["saw"],
    "TheMongolz": ["The MongolZ", "the mongolz", "TMZ"],
    # Dota 2 teams
    "Team Secret": ["Secret", "TS"],
    "Tundra Esports": ["Tundra"],
    "Gaimin Gladiators": ["GG", "Gaimin"],
    "BetBoom Team": ["BB", "BetBoom"],
    "PSG.LGD": ["LGD", "PSG LGD"],
    "Azure Ray": ["azure ray"],
    "Xtreme Gaming": ["XG", "Xtreme"],
}

# Build reverse lookup: alias -> canonical name
_ALIAS_TO_CANONICAL: dict[str, str] = {}
for canonical, aliases in TEAM_ALIASES.items():
    _ALIAS_TO_CANONICAL[canonical.lower()] = canonical
    for alias in aliases:
        _ALIAS_TO_CANONICAL[alias.lower()] = canonical


def normalize_team_name(name: str) -> str:
    """Normalize a team name to its canonical form.

    Returns the canonical name if found in aliases, otherwise returns
    a cleaned version of the input.
    """
    cleaned = name.strip()
    # Try exact match first
    lower = cleaned.lower()
    if lower in _ALIAS_TO_CANONICAL:
        return _ALIAS_TO_CANONICAL[lower]
    # Try removing common suffixes/prefixes
    for suffix in (" Esports", " Gaming", " Team", " Clan", " eSports"):
        stripped = re.sub(re.escape(suffix) + r"$", "", cleaned, flags=re.IGNORECASE)
        if stripped.lower() in _ALIAS_TO_CANONICAL:
            return _ALIAS_TO_CANONICAL[stripped.lower()]
    return cleaned
