"""Discovers and tracks active Polymarket esports markets via the Gamma API."""

from __future__ import annotations

import asyncio
import logging
import re
from typing import AsyncIterator

import aiohttp

from aargh.config import Config
from aargh.market.models import GammaMarket, TrackedMarket

logger = logging.getLogger(__name__)

# Common patterns in Polymarket esports market questions
_TEAM_VS_PATTERN = re.compile(
    r"(?:Will\s+)?(.+?)\s+(?:win|beat|defeat)\s+(?:vs\.?\s+|against\s+)?(.+?)(?:\s+in\s+(.+?))?[?\.]?$",
    re.IGNORECASE,
)
_WINNER_PATTERN = re.compile(
    r"(?:Who will win|Winner of)\s+(.+?)\s+vs\.?\s+(.+?)(?:\s+(?:at|in|during)\s+(.+?))?[?\.]?$",
    re.IGNORECASE,
)


def _extract_teams(question: str) -> tuple[str, str, str | None]:
    """Extract team names and optional tournament from a market question."""
    for pat in (_TEAM_VS_PATTERN, _WINNER_PATTERN):
        m = pat.search(question)
        if m:
            team_a = m.group(1).strip()
            team_b = m.group(2).strip()
            tournament = m.group(3).strip() if m.group(3) else None
            return team_a, team_b, tournament
    # Fallback: try splitting on " vs " or " v "
    for sep in (" vs. ", " vs ", " v "):
        if sep in question:
            parts = question.split(sep, 1)
            a = parts[0].split()[-3:]  # last few words before vs
            b = parts[1].split("?")[0].split(" in ")[0].strip()
            return " ".join(a).strip(), b.strip(), None
    return "", "", None


class MarketScanner:
    """Polls the Polymarket Gamma API for active esports markets."""

    def __init__(self, config: Config, session: aiohttp.ClientSession | None = None):
        self._config = config
        self._session = session
        self._owns_session = session is None
        self._gamma = config.gamma_host
        self._esports_tag_ids: list[str] = []
        self._tracked: dict[str, TrackedMarket] = {}

    async def _ensure_session(self) -> aiohttp.ClientSession:
        if self._session is None or self._session.closed:
            self._session = aiohttp.ClientSession()
            self._owns_session = True
        return self._session

    async def close(self) -> None:
        if self._owns_session and self._session and not self._session.closed:
            await self._session.close()

    async def discover_esports_tags(self) -> list[str]:
        """Find tag IDs related to esports by querying GET /tags and GET /sports."""
        session = await self._ensure_session()
        tag_ids: list[str] = []

        esports_keywords = {
            "esports", "e-sports", "gaming", "cs2", "counter-strike",
            "dota", "league of legends", "valorant", "egames",
        }

        for endpoint in ("/tags", "/sports"):
            try:
                async with session.get(f"{self._gamma}{endpoint}") as resp:
                    if resp.status != 200:
                        logger.warning("GET %s returned %d", endpoint, resp.status)
                        continue
                    data = await resp.json()
                    if not isinstance(data, list):
                        data = [data] if isinstance(data, dict) else []
                    for item in data:
                        label = str(item.get("label", item.get("name", ""))).lower()
                        slug = str(item.get("slug", "")).lower()
                        if any(kw in label or kw in slug for kw in esports_keywords):
                            tag_id = str(item.get("id", ""))
                            if tag_id:
                                tag_ids.append(tag_id)
                                logger.info("Found esports tag: %s (id=%s)", label, tag_id)
            except Exception as e:
                logger.warning("Error discovering tags from %s: %s", endpoint, e)

        self._esports_tag_ids = tag_ids
        return tag_ids

    async def scan_once(self) -> list[TrackedMarket]:
        """Fetch active esports markets from Gamma API."""
        session = await self._ensure_session()
        markets: list[TrackedMarket] = []

        # If we have tag IDs, filter by them; otherwise do a broad scan
        if self._esports_tag_ids:
            for tag_id in self._esports_tag_ids:
                params = {
                    "tag_id": tag_id,
                    "active": "true",
                    "closed": "false",
                    "limit": "100",
                }
                new_markets = await self._fetch_markets(session, params)
                markets.extend(new_markets)
        else:
            # Broad search with esports-related text filtering
            params = {"active": "true", "closed": "false", "limit": "100"}
            all_markets = await self._fetch_markets(session, params)
            esports_terms = {"esport", "cs2", "counter-strike", "dota", "valorant",
                             "league of legends", "blast", "iem", "esl", "major"}
            for m in all_markets:
                q = m.market.question.lower()
                if any(term in q for term in esports_terms):
                    markets.append(m)

        # Deduplicate by market ID
        seen: set[str] = set()
        unique: list[TrackedMarket] = []
        for m in markets:
            if m.market.id not in seen:
                seen.add(m.market.id)
                unique.append(m)
                self._tracked[m.market.id] = m

        logger.info("Scan found %d esports markets", len(unique))
        return unique

    async def _fetch_markets(
        self, session: aiohttp.ClientSession, params: dict
    ) -> list[TrackedMarket]:
        """Fetch and parse markets from Gamma API."""
        tracked: list[TrackedMarket] = []

        # Try events endpoint first (events contain their markets)
        try:
            async with session.get(f"{self._gamma}/events", params=params) as resp:
                if resp.status == 200:
                    events = await resp.json()
                    if isinstance(events, list):
                        for event in events:
                            event_markets = event.get("markets", [])
                            for mdata in event_markets:
                                tm = self._parse_market(mdata)
                                if tm:
                                    tracked.append(tm)
        except Exception as e:
            logger.warning("Error fetching events: %s", e)

        # Also try markets endpoint directly
        try:
            async with session.get(f"{self._gamma}/markets", params=params) as resp:
                if resp.status == 200:
                    data = await resp.json()
                    if isinstance(data, list):
                        for mdata in data:
                            tm = self._parse_market(mdata)
                            if tm:
                                tracked.append(tm)
        except Exception as e:
            logger.warning("Error fetching markets: %s", e)

        return tracked

    def _parse_market(self, data: dict) -> TrackedMarket | None:
        """Parse a raw market dict into a TrackedMarket."""
        try:
            gm = GammaMarket.from_api(data)
        except Exception as e:
            logger.debug("Failed to parse market: %s", e)
            return None

        if not gm.clob_token_ids or gm.closed:
            return None

        team_a, team_b, tournament = _extract_teams(gm.question)
        if not team_a and not team_b:
            team_a = gm.question  # use full question as fallback

        # Detect game from question text
        q = gm.question.lower()
        game = "cs2"
        if "dota" in q:
            game = "dota2"
        elif "league" in q or "lol" in q:
            game = "lol"
        elif "valorant" in q:
            game = "valorant"

        return TrackedMarket(
            market=gm,
            team_a=team_a,
            team_b=team_b,
            tournament=tournament,
            game=game,
        )

    async def run(self, interval: int | None = None) -> AsyncIterator[list[TrackedMarket]]:
        """Continuously scan for markets, yielding new results."""
        interval = interval or self._config.scan_interval
        await self.discover_esports_tags()
        while True:
            try:
                markets = await self.scan_once()
                yield markets
            except Exception as e:
                logger.error("Market scan error: %s", e)
            await asyncio.sleep(interval)

    @property
    def tracked_markets(self) -> dict[str, TrackedMarket]:
        return dict(self._tracked)
