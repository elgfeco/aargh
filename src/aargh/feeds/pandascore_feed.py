"""PandaScore free fixtures tier feed for match discovery and schedules."""

from __future__ import annotations

import asyncio
import logging
from typing import AsyncIterator

import aiohttp

from aargh.config import Config
from aargh.feeds.base import BaseFeed
from aargh.feeds.events import EventType, GameEvent

logger = logging.getLogger(__name__)

_BASE_URL = "https://api.pandascore.co"

# PandaScore game slugs
GAME_SLUGS = {
    "cs2": "cs-2",
    "csgo": "csgo",
    "dota2": "dota-2",
    "lol": "league-of-legends",
    "valorant": "valorant",
}


class PandaScoreFeed(BaseFeed):
    """Polls PandaScore REST API for live/upcoming match fixtures.

    The free tier only provides fixture data (schedules, teams, results).
    Live in-game stats require a paid plan.
    """

    def __init__(self, config: Config):
        self._config = config
        self._token = config.pandascore_api_key
        self._session: aiohttp.ClientSession | None = None
        self._running = False

    @property
    def name(self) -> str:
        return "pandascore"

    async def connect(self) -> None:
        headers = {}
        if self._token:
            headers["Authorization"] = f"Bearer {self._token}"
        self._session = aiohttp.ClientSession(headers=headers)
        self._running = True
        logger.info("PandaScore feed connected")

    async def disconnect(self) -> None:
        self._running = False
        if self._session and not self._session.closed:
            await self._session.close()

    async def get_live_fixtures(self) -> list[dict]:
        """Fetch currently running matches across all supported games."""
        if not self._session:
            raise RuntimeError("Feed not connected")

        fixtures: list[dict] = []
        for game, slug in GAME_SLUGS.items():
            try:
                url = f"{_BASE_URL}/{slug}/matches"
                params = {
                    "filter[status]": "running",
                    "per_page": "50",
                    "sort": "-scheduled_at",
                }
                async with self._session.get(url, params=params) as resp:
                    if resp.status == 403:
                        logger.debug("PandaScore 403 for %s (API key needed)", slug)
                        continue
                    if resp.status != 200:
                        continue
                    matches = await resp.json()
                    for match in matches:
                        opponents = match.get("opponents", [])
                        team_a = opponents[0].get("opponent", {}).get("name", "") if opponents else ""
                        team_b = opponents[1].get("opponent", {}).get("name", "") if len(opponents) > 1 else ""
                        tournament = match.get("tournament", {}).get("name", "")
                        streams = match.get("streams_list", [])
                        twitch_url = ""
                        for s in streams:
                            if "twitch" in s.get("raw_url", ""):
                                twitch_url = s["raw_url"]
                                break
                        fixtures.append({
                            "id": str(match.get("id", "")),
                            "game": game,
                            "tournament": tournament,
                            "team_a": team_a,
                            "team_b": team_b,
                            "score_a": match.get("results", [{}])[0].get("score", 0) if match.get("results") else 0,
                            "score_b": match.get("results", [{}])[1].get("score", 0) if len(match.get("results", [])) > 1 else 0,
                            "best_of": match.get("number_of_games", 3),
                            "stream_url": twitch_url,
                            "source": "pandascore",
                        })
            except Exception as e:
                logger.warning("PandaScore error for %s: %s", game, e)

        logger.info("PandaScore found %d live fixtures", len(fixtures))
        return fixtures

    async def get_upcoming_fixtures(self, hours: int = 24) -> list[dict]:
        """Fetch upcoming matches within the next N hours."""
        if not self._session:
            raise RuntimeError("Feed not connected")

        fixtures: list[dict] = []
        for game, slug in GAME_SLUGS.items():
            try:
                url = f"{_BASE_URL}/{slug}/matches/upcoming"
                params = {"per_page": "50"}
                async with self._session.get(url, params=params) as resp:
                    if resp.status != 200:
                        continue
                    matches = await resp.json()
                    for match in matches:
                        opponents = match.get("opponents", [])
                        fixtures.append({
                            "id": str(match.get("id", "")),
                            "game": game,
                            "tournament": match.get("tournament", {}).get("name", ""),
                            "team_a": opponents[0].get("opponent", {}).get("name", "") if opponents else "",
                            "team_b": opponents[1].get("opponent", {}).get("name", "") if len(opponents) > 1 else "",
                            "scheduled_at": match.get("scheduled_at", ""),
                            "best_of": match.get("number_of_games", 3),
                            "source": "pandascore",
                        })
            except Exception as e:
                logger.warning("PandaScore upcoming error for %s: %s", game, e)

        return fixtures

    async def subscribe(self, match_id: str) -> AsyncIterator[GameEvent]:
        """Poll match status for changes (free tier has no live in-game data)."""
        if not self._session:
            raise RuntimeError("Feed not connected")

        last_status = None
        while self._running:
            try:
                url = f"{_BASE_URL}/matches/{match_id}"
                async with self._session.get(url) as resp:
                    if resp.status != 200:
                        await asyncio.sleep(30)
                        continue
                    match = await resp.json()
                    status = match.get("status")
                    if status != last_status:
                        if status == "finished":
                            results = match.get("results", [])
                            yield GameEvent(
                                event_type=EventType.SERIES_ENDED,
                                match_id=match_id,
                                team_a_maps=results[0].get("score", 0) if results else 0,
                                team_b_maps=results[1].get("score", 0) if len(results) > 1 else 0,
                                winning_side="team_a" if results and results[0].get("score", 0) > results[1].get("score", 0) else "team_b",
                                source="pandascore",
                            )
                        last_status = status
            except Exception as e:
                logger.warning("PandaScore subscribe error: %s", e)
            await asyncio.sleep(30)
