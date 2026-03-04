"""Paper portfolio tracker for simulated positions and P&L."""

from __future__ import annotations

import json
import logging
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path

logger = logging.getLogger(__name__)

PORTFOLIO_FILE = "portfolio_state.json"


@dataclass
class Position:
    """A paper trading position."""

    market_id: str
    market_question: str
    token_id: str
    side: str
    entry_price: float
    amount: float       # USDC spent
    shares: float       # Amount / entry_price
    current_price: float = 0.0
    resolved: bool = False
    won: bool = False
    pnl: float = 0.0
    opened_at: str = ""
    closed_at: str = ""

    @property
    def unrealized_pnl(self) -> float:
        if self.resolved:
            return self.pnl
        # Current value of shares minus cost
        return self.shares * self.current_price - self.amount


@dataclass
class Portfolio:
    """Tracks all paper positions and aggregate performance."""

    positions: list[Position] = field(default_factory=list)
    total_pnl: float = 0.0
    total_bets: int = 0
    wins: int = 0
    losses: int = 0

    def add_position(self, pos: Position) -> None:
        self.positions.append(pos)
        self.total_bets += 1
        logger.info(
            "PAPER BET: %s $%.2f @ %.3f on '%s'",
            pos.side, pos.amount, pos.entry_price, pos.market_question[:50],
        )

    def resolve_position(self, market_id: str, winning_token: str) -> None:
        """Resolve all positions for a market."""
        for pos in self.positions:
            if pos.market_id == market_id and not pos.resolved:
                pos.resolved = True
                pos.closed_at = datetime.now(timezone.utc).isoformat()
                if pos.token_id == winning_token:
                    # Won: shares pay out at $1 each
                    pos.pnl = pos.shares * 1.0 - pos.amount
                    pos.won = True
                    self.wins += 1
                else:
                    # Lost: shares worth $0
                    pos.pnl = -pos.amount
                    pos.won = False
                    self.losses += 1
                self.total_pnl += pos.pnl
                logger.info(
                    "RESOLVED: %s P&L=$%.2f ('%s')",
                    "WIN" if pos.won else "LOSS", pos.pnl, pos.market_question[:50],
                )

    def update_prices(self, market_id: str, yes_price: float, no_price: float) -> None:
        """Update current prices for unrealized P&L calculation."""
        for pos in self.positions:
            if pos.market_id == market_id and not pos.resolved:
                # Determine if this is a yes or no token position
                pos.current_price = yes_price  # simplified

    @property
    def open_positions(self) -> list[Position]:
        return [p for p in self.positions if not p.resolved]

    @property
    def closed_positions(self) -> list[Position]:
        return [p for p in self.positions if p.resolved]

    @property
    def win_rate(self) -> float:
        total = self.wins + self.losses
        return self.wins / total if total > 0 else 0.0

    @property
    def unrealized_pnl(self) -> float:
        return sum(p.unrealized_pnl for p in self.open_positions)

    def save(self, path: str = PORTFOLIO_FILE) -> None:
        """Persist portfolio state to JSON."""
        data = {
            "total_pnl": self.total_pnl,
            "total_bets": self.total_bets,
            "wins": self.wins,
            "losses": self.losses,
            "positions": [asdict(p) for p in self.positions],
        }
        Path(path).write_text(json.dumps(data, indent=2))

    @classmethod
    def load(cls, path: str = PORTFOLIO_FILE) -> Portfolio:
        """Load portfolio state from JSON."""
        try:
            data = json.loads(Path(path).read_text())
            portfolio = cls(
                total_pnl=data.get("total_pnl", 0),
                total_bets=data.get("total_bets", 0),
                wins=data.get("wins", 0),
                losses=data.get("losses", 0),
            )
            for p in data.get("positions", []):
                portfolio.positions.append(Position(**p))
            return portfolio
        except FileNotFoundError:
            return cls()
        except Exception as e:
            logger.warning("Failed to load portfolio: %s", e)
            return cls()
