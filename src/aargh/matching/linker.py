"""Maps Polymarket markets to live esports matches via fuzzy team name matching."""

from __future__ import annotations

import logging
from rapidfuzz import fuzz, process

from aargh.feeds.base import BaseFeed
from aargh.market.models import TrackedMarket
from aargh.matching.normalization import normalize_team_name

logger = logging.getLogger(__name__)

MATCH_THRESHOLD = 70  # Minimum fuzzy match score (0-100)


class MatchLinker:
    """Links Polymarket markets to live match fixtures from data feeds."""

    def __init__(self, feeds: list[BaseFeed], threshold: int = MATCH_THRESHOLD):
        self._feeds = feeds
        self._threshold = threshold
        self._links: dict[str, str] = {}  # market_id -> match_id

    async def link_markets(self, markets: list[TrackedMarket]) -> list[TrackedMarket]:
        """Attempt to link each unlinked market to a live fixture."""
        # Gather all live fixtures from all feeds
        all_fixtures: list[dict] = []
        for feed in self._feeds:
            try:
                fixtures = await feed.get_live_fixtures()
                all_fixtures.extend(fixtures)
            except Exception as e:
                logger.warning("Error getting fixtures from %s: %s", feed.name, e)

        if not all_fixtures:
            return markets

        linked: list[TrackedMarket] = []
        for market in markets:
            if market.linked_match_id:
                linked.append(market)
                continue

            match_id = self._find_match(market, all_fixtures)
            if match_id:
                market.linked_match_id = match_id
                self._links[market.market.id] = match_id
                logger.info(
                    "Linked market '%s' to match %s",
                    market.market.question[:60],
                    match_id,
                )
            linked.append(market)

        return linked

    def _find_match(self, market: TrackedMarket, fixtures: list[dict]) -> str | None:
        """Find the best matching fixture for a market."""
        norm_a = normalize_team_name(market.team_a)
        norm_b = normalize_team_name(market.team_b)

        best_score = 0
        best_id: str | None = None

        for fixture in fixtures:
            fix_a = normalize_team_name(fixture.get("team_a", ""))
            fix_b = normalize_team_name(fixture.get("team_b", ""))

            if not fix_a or not fix_b:
                continue

            # Try both orderings (market team_a could be fixture team_b)
            score_direct = (
                fuzz.token_sort_ratio(norm_a, fix_a) +
                fuzz.token_sort_ratio(norm_b, fix_b)
            ) / 2

            score_flipped = (
                fuzz.token_sort_ratio(norm_a, fix_b) +
                fuzz.token_sort_ratio(norm_b, fix_a)
            ) / 2

            score = max(score_direct, score_flipped)

            # Boost score if tournament name also matches
            if market.tournament and fixture.get("tournament"):
                tourn_score = fuzz.token_sort_ratio(
                    market.tournament.lower(),
                    fixture["tournament"].lower(),
                )
                if tourn_score > 60:
                    score = min(100, score + 10)

            if score > best_score:
                best_score = score
                best_id = fixture.get("id")

        if best_score >= self._threshold and best_id:
            return str(best_id)
        return None

    @property
    def links(self) -> dict[str, str]:
        return dict(self._links)
