from __future__ import annotations

import json
from dataclasses import dataclass, field


@dataclass
class GammaMarket:
    """A market as returned by the Polymarket Gamma API."""

    id: str
    question: str
    condition_id: str
    slug: str
    outcomes: list[str]
    outcome_prices: list[float]
    clob_token_ids: list[str]
    active: bool
    closed: bool
    liquidity: float = 0.0
    volume: float = 0.0
    end_date: str = ""
    neg_risk: bool = False

    @classmethod
    def from_api(cls, data: dict) -> GammaMarket:
        """Parse a market dict from the Gamma API response.

        The API returns outcomes, outcomePrices, and clobTokenIds as JSON strings.
        """

        def _parse_json_list(val, cast=str) -> list:
            if isinstance(val, str):
                try:
                    return [cast(x) for x in json.loads(val)]
                except (json.JSONDecodeError, ValueError):
                    return []
            if isinstance(val, list):
                return [cast(x) for x in val]
            return []

        return cls(
            id=str(data.get("id", "")),
            question=data.get("question", ""),
            condition_id=data.get("conditionId", data.get("condition_id", "")),
            slug=data.get("slug", ""),
            outcomes=_parse_json_list(data.get("outcomes", "[]")),
            outcome_prices=_parse_json_list(data.get("outcomePrices", "[]"), float),
            clob_token_ids=_parse_json_list(data.get("clobTokenIds", "[]")),
            active=bool(data.get("active", False)),
            closed=bool(data.get("closed", False)),
            liquidity=float(data.get("liquidity", 0) or 0),
            volume=float(data.get("volume", 0) or 0),
            end_date=data.get("endDate", data.get("end_date", "")),
            neg_risk=bool(data.get("negRisk", False)),
        )


@dataclass
class TrackedMarket:
    """A Polymarket market enriched with parsed team info and linked match data."""

    market: GammaMarket
    team_a: str
    team_b: str
    tournament: str | None = None
    game: str = "cs2"
    linked_match_id: str | None = None
    best_of: int = 3

    @property
    def yes_token(self) -> str:
        return self.market.clob_token_ids[0] if self.market.clob_token_ids else ""

    @property
    def no_token(self) -> str:
        return self.market.clob_token_ids[1] if len(self.market.clob_token_ids) > 1 else ""

    @property
    def yes_price(self) -> float:
        return self.market.outcome_prices[0] if self.market.outcome_prices else 0.5

    @property
    def no_price(self) -> float:
        return self.market.outcome_prices[1] if len(self.market.outcome_prices) > 1 else 0.5
