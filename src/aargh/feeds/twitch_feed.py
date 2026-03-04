"""Twitch Helix API feed for monitoring esports streams."""

from __future__ import annotations

import asyncio
import logging
from typing import AsyncIterator

import aiohttp

from aargh.config import Config
from aargh.feeds.base import BaseFeed
from aargh.feeds.events import GameEvent

logger = logging.getLogger(__name__)

_HELIX_URL = "https://api.twitch.tv/helix"
_TOKEN_URL = "https://id.twitch.tv/oauth2/token"

# Twitch game IDs for esports titles
TWITCH_GAME_IDS = {
    "cs2": "32399",          # Counter-Strike
    "dota2": "29595",        # Dota 2
    "lol": "21779",          # League of Legends
    "valorant": "516575",    # VALORANT
}


class TwitchFeed(BaseFeed):
    """Monitors Twitch streams for live esports tournaments.

    Provides stream status as a proxy for match timing - when tournament
    streams go live, matches are likely starting.
    """

    def __init__(self, config: Config):
        self._config = config
        self._client_id = config.twitch_client_id
        self._client_secret = config.twitch_client_secret
        self._session: aiohttp.ClientSession | None = None
        self._access_token: str = ""
        self._running = False

    @property
    def name(self) -> str:
        return "twitch"

    async def connect(self) -> None:
        self._session = aiohttp.ClientSession()
        self._running = True
        if self._client_id and self._client_secret:
            await self._authenticate()
        logger.info("Twitch feed connected")

    async def disconnect(self) -> None:
        self._running = False
        if self._session and not self._session.closed:
            await self._session.close()

    async def _authenticate(self) -> None:
        """Get an app access token via client credentials flow."""
        if not self._session:
            return
        params = {
            "client_id": self._client_id,
            "client_secret": self._client_secret,
            "grant_type": "client_credentials",
        }
        try:
            async with self._session.post(_TOKEN_URL, params=params) as resp:
                if resp.status == 200:
                    data = await resp.json()
                    self._access_token = data.get("access_token", "")
                    logger.info("Twitch authenticated")
                else:
                    logger.warning("Twitch auth failed: %d", resp.status)
        except Exception as e:
            logger.warning("Twitch auth error: %s", e)

    def _headers(self) -> dict[str, str]:
        return {
            "Client-ID": self._client_id,
            "Authorization": f"Bearer {self._access_token}",
        }

    async def get_live_fixtures(self) -> list[dict]:
        """Find live esports streams on Twitch."""
        if not self._session or not self._access_token:
            return []

        fixtures: list[dict] = []
        for game, game_id in TWITCH_GAME_IDS.items():
            try:
                params = {
                    "game_id": game_id,
                    "first": "20",
                    "type": "live",
                }
                async with self._session.get(
                    f"{_HELIX_URL}/streams",
                    params=params,
                    headers=self._headers(),
                ) as resp:
                    if resp.status != 200:
                        continue
                    data = await resp.json()
                    streams = data.get("data", [])
                    for stream in streams:
                        # Filter for tournament/official streams by viewer count
                        if stream.get("viewer_count", 0) >= 1000:
                            fixtures.append({
                                "id": stream.get("id", ""),
                                "game": game,
                                "channel": stream.get("user_name", ""),
                                "title": stream.get("title", ""),
                                "viewers": stream.get("viewer_count", 0),
                                "started_at": stream.get("started_at", ""),
                                "source": "twitch",
                            })
            except Exception as e:
                logger.warning("Twitch streams error for %s: %s", game, e)

        logger.info("Twitch found %d live esports streams", len(fixtures))
        return fixtures

    async def subscribe(self, match_id: str) -> AsyncIterator[GameEvent]:
        """Twitch doesn't provide game events, just stream status.

        This is a no-op iterator; Twitch is used for fixture discovery only.
        """
        return
        yield  # Make this a proper async generator
