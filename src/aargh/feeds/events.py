from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import Enum


class EventType(Enum):
    ROUND_STARTED = "round_started"
    ROUND_ENDED = "round_ended"
    MAP_STARTED = "map_started"
    MAP_ENDED = "map_ended"
    SERIES_ENDED = "series_ended"
    KILL = "kill"
    BOMB_PLANTED = "bomb_planted"
    BOMB_DEFUSED = "bomb_defused"
    BOMB_EXPLODED = "bomb_exploded"
    OBJECTIVE = "objective"  # Roshan, tower, barracks (Dota2)
    ECONOMY_UPDATE = "economy_update"
    GOLD_UPDATE = "gold_update"


class WinCondition(Enum):
    ELIMINATION = "elimination"
    BOMB_DEFUSED = "bomb_defused"
    BOMB_EXPLODED = "bomb_exploded"
    TIME_EXPIRED = "time_expired"
    UNKNOWN = "unknown"


@dataclass
class GameEvent:
    """Normalized game event consumed by the signal engine."""

    event_type: EventType
    match_id: str
    timestamp: datetime = field(default_factory=lambda: datetime.now(timezone.utc))

    # Map/round state
    map_number: int | None = None
    round_number: int | None = None
    map_name: str | None = None

    # Scores
    team_a_rounds: int | None = None
    team_b_rounds: int | None = None
    team_a_maps: int | None = None
    team_b_maps: int | None = None

    # Round result
    winning_side: str | None = None  # team_a or team_b
    win_condition: WinCondition | None = None

    # Economy / Dota2
    team_a_economy: int | None = None
    team_b_economy: int | None = None
    gold_lead: int | None = None  # positive = team_a leads
    xp_lead: int | None = None

    # Metadata
    best_of: int = 3
    source: str = ""
    raw: dict = field(default_factory=dict)
