"""GRID Open Access feed for CS2 and Dota2 live data.

GRID Open Access provides a GraphQL API for match data. The Series Events
(real-time WebSocket) is paid, so we poll the Series State API and diff
consecutive states to generate events.
"""

from __future__ import annotations

import asyncio
import logging
from datetime import datetime, timezone
from typing import AsyncIterator

import aiohttp

from aargh.config import Config
from aargh.feeds.base import BaseFeed
from aargh.feeds.events import EventType, GameEvent, WinCondition

logger = logging.getLogger(__name__)

_CENTRAL_URL = "https://api-op.grid.gg/central-data/graphql"
_SERIES_STATE_URL = "https://api-op.grid.gg/series-state/graphql"

_LIVE_SERIES_QUERY = """
query LiveSeries($titleIds: [Int!]) {
  allSeries(
    filter: {
      states: [LIVE]
      titleIds: $titleIds
    }
    first: 50
    orderBy: StartTimeScheduled
  ) {
    edges {
      node {
        id
        title { name nameShortened }
        tournament { name }
        teams {
          baseInfo { name }
          score
        }
        format { bestOf }
        startTimeScheduled
      }
    }
  }
}
"""

_SERIES_STATE_QUERY = """
query SeriesState($seriesId: ID!) {
  seriesState(id: $seriesId) {
    id
    title { name }
    teams {
      name
      score
      won
    }
    games {
      sequenceNumber
      map { name }
      finished
      teams {
        name
        score
        won
        side
      }
    }
    finished
  }
}
"""

# GRID title IDs: CS2 = 25, Dota 2 = 1
TITLE_IDS = {"cs2": 25, "dota2": 1}


class GridFeed(BaseFeed):
    """Polls GRID Open Access for live match state changes."""

    def __init__(self, config: Config):
        self._config = config
        self._api_key = config.grid_api_key
        self._session: aiohttp.ClientSession | None = None
        self._last_states: dict[str, dict] = {}
        self._running = False

    @property
    def name(self) -> str:
        return "grid"

    async def connect(self) -> None:
        headers = {}
        if self._api_key:
            headers["x-api-key"] = self._api_key
        self._session = aiohttp.ClientSession(headers=headers)
        self._running = True
        logger.info("GRID feed connected")

    async def disconnect(self) -> None:
        self._running = False
        if self._session and not self._session.closed:
            await self._session.close()

    async def _graphql(self, url: str, query: str, variables: dict | None = None) -> dict:
        if not self._session:
            raise RuntimeError("Feed not connected")
        payload = {"query": query}
        if variables:
            payload["variables"] = variables
        async with self._session.post(url, json=payload) as resp:
            if resp.status != 200:
                text = await resp.text()
                logger.warning("GRID GraphQL %d: %s", resp.status, text[:200])
                return {}
            return await resp.json()

    async def get_live_fixtures(self) -> list[dict]:
        title_ids = list(TITLE_IDS.values())
        result = await self._graphql(
            _CENTRAL_URL,
            _LIVE_SERIES_QUERY,
            {"titleIds": title_ids},
        )
        edges = (
            result.get("data", {})
            .get("allSeries", {})
            .get("edges", [])
        )
        fixtures = []
        for edge in edges:
            node = edge.get("node", {})
            teams = node.get("teams", [])
            fixtures.append({
                "id": node.get("id"),
                "game": node.get("title", {}).get("nameShortened", ""),
                "tournament": node.get("tournament", {}).get("name", ""),
                "team_a": teams[0].get("baseInfo", {}).get("name", "") if teams else "",
                "team_b": teams[1].get("baseInfo", {}).get("name", "") if len(teams) > 1 else "",
                "score_a": teams[0].get("score", 0) if teams else 0,
                "score_b": teams[1].get("score", 0) if len(teams) > 1 else 0,
                "best_of": node.get("format", {}).get("bestOf", 3),
                "source": "grid",
            })
        return fixtures

    async def _get_series_state(self, series_id: str) -> dict:
        result = await self._graphql(
            _SERIES_STATE_URL,
            _SERIES_STATE_QUERY,
            {"seriesId": series_id},
        )
        return result.get("data", {}).get("seriesState", {})

    def _diff_state(self, series_id: str, new_state: dict) -> list[GameEvent]:
        """Compare new state with last known state to produce events."""
        old_state = self._last_states.get(series_id, {})
        self._last_states[series_id] = new_state
        events: list[GameEvent] = []

        if not old_state:
            return events  # First poll, no diff

        old_games = old_state.get("games", [])
        new_games = new_state.get("games", [])

        new_teams = new_state.get("teams", [])
        team_a_maps = new_teams[0].get("score", 0) if new_teams else 0
        team_b_maps = new_teams[1].get("score", 0) if len(new_teams) > 1 else 0

        for i, game in enumerate(new_games):
            old_game = old_games[i] if i < len(old_games) else {}
            game_teams = game.get("teams", [])
            old_teams = old_game.get("teams", [])

            if not game_teams:
                continue

            new_score_a = game_teams[0].get("score", 0)
            new_score_b = game_teams[1].get("score", 0) if len(game_teams) > 1 else 0
            old_score_a = old_teams[0].get("score", 0) if old_teams else 0
            old_score_b = old_teams[1].get("score", 0) if len(old_teams) > 1 else 0

            # Detect round end (score changed)
            if new_score_a != old_score_a or new_score_b != old_score_b:
                winning = "team_a" if new_score_a > old_score_a else "team_b"
                events.append(GameEvent(
                    event_type=EventType.ROUND_ENDED,
                    match_id=series_id,
                    map_number=i + 1,
                    round_number=new_score_a + new_score_b,
                    map_name=game.get("map", {}).get("name"),
                    team_a_rounds=new_score_a,
                    team_b_rounds=new_score_b,
                    team_a_maps=team_a_maps,
                    team_b_maps=team_b_maps,
                    winning_side=winning,
                    win_condition=WinCondition.UNKNOWN,
                    best_of=len(new_games),
                    source="grid",
                    raw=game,
                ))

            # Detect map end
            if game.get("finished") and not old_game.get("finished"):
                winner = "team_a" if game_teams[0].get("won") else "team_b"
                events.append(GameEvent(
                    event_type=EventType.MAP_ENDED,
                    match_id=series_id,
                    map_number=i + 1,
                    map_name=game.get("map", {}).get("name"),
                    team_a_rounds=new_score_a,
                    team_b_rounds=new_score_b,
                    team_a_maps=team_a_maps,
                    team_b_maps=team_b_maps,
                    winning_side=winner,
                    best_of=len(new_games),
                    source="grid",
                    raw=game,
                ))

        # Detect series end
        if new_state.get("finished") and not old_state.get("finished"):
            winner = "team_a" if new_teams[0].get("won") else "team_b"
            events.append(GameEvent(
                event_type=EventType.SERIES_ENDED,
                match_id=series_id,
                team_a_maps=team_a_maps,
                team_b_maps=team_b_maps,
                winning_side=winner,
                best_of=len(new_games),
                source="grid",
            ))

        return events

    async def subscribe(self, match_id: str) -> AsyncIterator[GameEvent]:
        """Poll series state every 6 seconds and yield diff events."""
        while self._running:
            try:
                state = await self._get_series_state(match_id)
                if state:
                    events = self._diff_state(match_id, state)
                    for event in events:
                        yield event
            except Exception as e:
                logger.warning("GRID poll error for %s: %s", match_id, e)
            await asyncio.sleep(6)  # ~10 req/min limit
