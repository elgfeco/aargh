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
    OBJECTIVE = "objective"
    ECONOMY_UPDATE = "economy_update"
    GOLD_UPDATE = "gold_update"
    # Mid-round CS2 events
    PLAYER_COUNT_UPDATE = "player_count_update"
    CLUTCH_SITUATION = "clutch_situation"
    # Dota2 objectives
    ROSHAN_KILL = "roshan_kill"
    TOWER_DESTROY = "tower_destroy"
    BARRACKS_DESTROY = "barracks_destroy"
    AEGIS_PICKUP = "aegis_pickup"


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

    # Mid-round CS2 state
    team_a_alive: int | None = None  # Players alive (0-5)
    team_b_alive: int | None = None
    bomb_planted: bool = False
    bomb_site: str | None = None  # "A" or "B"
    is_pistol_round: bool = False
    is_eco_round: bool = False  # team buying is on eco/force

    # Dota2 objectives
    objective_type: str | None = None  # "roshan", "tower", "barracks", "ancient"
    objective_tier: int | None = None  # Tower tier (1-4) or barracks type
    roshan_number: int | None = None  # Which Roshan kill (1st, 2nd, 3rd...)
    aegis_holder: str | None = None  # team_a or team_b

    # Metadata
    best_of: int = 3
    source: str = ""
    raw: dict = field(default_factory=dict)
