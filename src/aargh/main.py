"""Entry point for the AARGH Polymarket esports arbitrage bot."""

from __future__ import annotations

import asyncio
import logging
import signal
import sys

from aargh.config import Config
from aargh.orchestrator import Orchestrator


def setup_logging(level: str = "INFO") -> None:
    logging.basicConfig(
        level=getattr(logging, level.upper(), logging.INFO),
        format="%(asctime)s %(levelname)-8s %(name)-20s %(message)s",
        datefmt="%H:%M:%S",
    )
    # Quiet noisy loggers
    logging.getLogger("aiohttp").setLevel(logging.WARNING)
    logging.getLogger("websockets").setLevel(logging.WARNING)


async def async_main() -> None:
    config = Config.from_env()
    setup_logging(config.log_level)
    logger = logging.getLogger("aargh")

    logger.info("AARGH starting up (dry_run=%s)", config.dry_run)

    orchestrator = Orchestrator(config)

    # Handle shutdown signals
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, lambda: asyncio.create_task(_shutdown(orchestrator)))

    await orchestrator.run()


async def _shutdown(orchestrator: Orchestrator) -> None:
    await orchestrator._shutdown()
    sys.exit(0)


def main() -> None:
    try:
        asyncio.run(async_main())
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
