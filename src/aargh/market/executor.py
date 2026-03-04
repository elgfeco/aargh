"""Paper trade executor - simulates Polymarket order placement."""

from __future__ import annotations

import logging
from dataclasses import dataclass
from datetime import datetime, timezone

from aargh.config import Config
from aargh.trading.portfolio import Portfolio, Position
from aargh.trading.risk import BetOrder

logger = logging.getLogger(__name__)


@dataclass
class BetResult:
    """Result of a bet placement (real or simulated)."""

    success: bool
    order_id: str
    fill_price: float
    fill_amount: float
    dry_run: bool
    error: str = ""


class Executor:
    """Places paper bets and tracks them in the portfolio.

    In dry_run mode (default), logs what would be placed without hitting Polymarket.
    Could be extended to use py-clob-client for live trading.
    """

    def __init__(self, config: Config, portfolio: Portfolio):
        self._config = config
        self._portfolio = portfolio
        self._dry_run = config.dry_run
        self._bet_counter = 0

    async def execute(self, order: BetOrder) -> BetResult:
        """Execute a bet order (paper trade)."""
        self._bet_counter += 1
        order_id = f"paper-{self._bet_counter:06d}"

        if self._dry_run:
            # Simulate fill at current market price
            result = BetResult(
                success=True,
                order_id=order_id,
                fill_price=order.price,
                fill_amount=order.amount,
                dry_run=True,
            )

            # Record in portfolio
            shares = order.amount / order.price if order.price > 0 else 0
            position = Position(
                market_id=order.signal.market.market.id,
                market_question=order.signal.market.market.question,
                token_id=order.token_id,
                side=order.side,
                entry_price=order.price,
                amount=order.amount,
                shares=round(shares, 4),
                current_price=order.price,
                opened_at=datetime.now(timezone.utc).isoformat(),
            )
            self._portfolio.add_position(position)

            logger.info(
                "PAPER TRADE #%s: %s $%.2f @ %.3f (%s shares) | edge=%+.1f%% | '%s'",
                order_id, order.side, order.amount, order.price,
                f"{shares:.2f}", order.signal.edge * 100,
                order.signal.market.market.question[:50],
            )

            return result

        # Live trading would go here using py-clob-client:
        # from py_clob_client.client import ClobClient
        # from py_clob_client.clob_types import MarketOrderArgs, OrderType
        # client = ClobClient(host, key=private_key, chain_id=137)
        # creds = client.create_or_derive_api_creds()
        # client.set_api_creds(creds)
        # mo = MarketOrderArgs(token_id=order.token_id, amount=order.amount, side=order.side)
        # signed = client.create_market_order(mo)
        # resp = client.post_order(signed, OrderType.FOK)

        return BetResult(
            success=False,
            order_id="",
            fill_price=0,
            fill_amount=0,
            dry_run=False,
            error="Live trading not implemented - set DRY_RUN=true",
        )

    @property
    def portfolio(self) -> Portfolio:
        return self._portfolio
