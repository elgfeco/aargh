"""Abstract base class for live esports data feeds."""

from __future__ import annotations

from abc import ABC, abstractmethod
from typing import AsyncIterator

from aargh.feeds.events import GameEvent


class BaseFeed(ABC):
    """Interface for esports live data sources."""

    @property
    @abstractmethod
    def name(self) -> str:
        """Human-readable feed name."""
        ...

    @abstractmethod
    async def connect(self) -> None:
        """Establish connection to the data source."""
        ...

    @abstractmethod
    async def disconnect(self) -> None:
        """Clean up connections."""
        ...

    @abstractmethod
    async def get_live_fixtures(self) -> list[dict]:
        """Return currently live match fixtures."""
        ...

    @abstractmethod
    def subscribe(self, match_id: str) -> AsyncIterator[GameEvent]:
        """Stream live events for a specific match."""
        ...
