from __future__ import annotations

import os
from dataclasses import dataclass, field

from dotenv import load_dotenv


@dataclass
class Config:
    # Polymarket
    polymarket_host: str = "https://clob.polymarket.com"
    gamma_host: str = "https://gamma-api.polymarket.com"
    chain_id: int = 137
    private_key: str = ""

    # GRID
    grid_api_key: str = ""

    # PandaScore
    pandascore_api_key: str = ""

    # Twitch
    twitch_client_id: str = ""
    twitch_client_secret: str = ""

    # Risk
    bankroll: float = 1000.0
    max_bet_pct: float = 0.05
    kelly_fraction: float = 0.25
    max_exposure_pct: float = 0.25
    min_edge: float = 0.05

    # Arbitrage
    arb_min_profit_pct: float = 0.5
    stale_threshold_s: float = 30.0

    # Stream monitoring
    stream_poll_interval: int = 10

    # Bot
    dry_run: bool = True
    scan_interval: int = 60
    log_level: str = "INFO"

    @classmethod
    def from_env(cls, env_path: str | None = None) -> Config:
        load_dotenv(env_path)
        return cls(
            polymarket_host=os.getenv("POLYMARKET_HOST", cls.polymarket_host),
            gamma_host=os.getenv("POLYMARKET_GAMMA_HOST", cls.gamma_host),
            chain_id=int(os.getenv("POLYMARKET_CHAIN_ID", str(cls.chain_id))),
            private_key=os.getenv("POLYMARKET_PRIVATE_KEY", ""),
            grid_api_key=os.getenv("GRID_API_KEY", ""),
            pandascore_api_key=os.getenv("PANDASCORE_API_KEY", ""),
            twitch_client_id=os.getenv("TWITCH_CLIENT_ID", ""),
            twitch_client_secret=os.getenv("TWITCH_CLIENT_SECRET", ""),
            bankroll=float(os.getenv("BANKROLL", str(cls.bankroll))),
            max_bet_pct=float(os.getenv("MAX_BET_PCT", str(cls.max_bet_pct))),
            kelly_fraction=float(os.getenv("KELLY_FRACTION", str(cls.kelly_fraction))),
            max_exposure_pct=float(os.getenv("MAX_EXPOSURE_PCT", str(cls.max_exposure_pct))),
            min_edge=float(os.getenv("MIN_EDGE", str(cls.min_edge))),
            arb_min_profit_pct=float(os.getenv("ARB_MIN_PROFIT_PCT", str(cls.arb_min_profit_pct))),
            stale_threshold_s=float(os.getenv("STALE_THRESHOLD_S", str(cls.stale_threshold_s))),
            stream_poll_interval=int(os.getenv("STREAM_POLL_INTERVAL", str(cls.stream_poll_interval))),
            dry_run=os.getenv("DRY_RUN", "true").lower() in ("true", "1", "yes"),
            scan_interval=int(os.getenv("SCAN_INTERVAL", str(cls.scan_interval))),
            log_level=os.getenv("LOG_LEVEL", cls.log_level),
        )
