"""Real-time stream monitoring with delay estimation and state tracking.

Monitors Twitch streams for esports tournaments, tracks stream state
transitions (offline -> live -> offline), and estimates broadcast delay
by comparing API event timestamps to stream visibility.
"""

from __future__ import annotations

import asyncio
import logging
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import AsyncIterator

import aiohttp

from aargh.config import Config

logger = logging.getLogger(__name__)

_HELIX_URL = "https://api.twitch.tv/helix"
_TOKEN_URL = "https://id.twitch.tv/oauth2/token"

TWITCH_GAME_IDS = {
    "cs2": "32399",
    "dota2": "29595",
    "lol": "21779",
    "valorant": "516575",
}


class StreamState:
    OFFLINE = "offline"
    LIVE = "live"
    UNKNOWN = "unknown"


@dataclass
class StreamStatus:
    """Current state of a monitored stream."""

    channel: str
    game: str
    state: str = StreamState.UNKNOWN
    viewer_count: int = 0
    title: str = ""
    started_at: str = ""
    last_checked: float = 0.0
    estimated_delay_s: float = 15.0  # Default broadcast delay estimate
    state_changes: list[tuple[float, str, str]] = field(default_factory=list)

    @property
    def is_live(self) -> bool:
        return self.state == StreamState.LIVE

    @property
    def uptime_s(self) -> float:
        if not self.started_at or not self.is_live:
            return 0.0
        try:
            start = datetime.fromisoformat(self.started_at.replace("Z", "+00:00"))
            return (datetime.now(timezone.utc) - start).total_seconds()
        except (ValueError, TypeError):
            return 0.0


@dataclass
class DelayEstimate:
    """Estimated broadcast delay for a stream."""

    channel: str
    estimated_delay_s: float
    confidence: float  # 0-1, how confident we are in this estimate
    sample_count: int
    last_updated: float
    method: str  # "api_diff", "title_change", "viewer_spike", "default"


class StreamMonitor:
    """Monitors Twitch stream states and estimates broadcast delay.

    The broadcast delay (typically 10-30+ seconds) is the core timing
    advantage: game server data arrives via GRID/PandaScore APIs before
    Twitch viewers see the result on stream.

    Delay estimation methods:
    1. API timestamp diff: Compare GRID event time to stream title/viewer changes
    2. Viewer spike detection: Sudden viewer count changes correlate with events
    3. Title change tracking: Stream titles often update with match info
    """

    def __init__(self, config: Config):
        self._config = config
        self._client_id = config.twitch_client_id
        self._client_secret = config.twitch_client_secret
        self._session: aiohttp.ClientSession | None = None
        self._access_token: str = ""
        self._running = False
        self._streams: dict[str, StreamStatus] = {}
        self._delay_estimates: dict[str, DelayEstimate] = {}
        self._event_timestamps: dict[str, list[float]] = {}  # match_id -> [api_event_times]
        self._viewer_history: dict[str, list[tuple[float, int]]] = {}  # channel -> [(time, count)]
        self._poll_interval = 10  # seconds between polls

    async def start(self) -> None:
        self._session = aiohttp.ClientSession()
        self._running = True
        if self._client_id and self._client_secret:
            await self._authenticate()
        logger.info("StreamMonitor started")

    async def stop(self) -> None:
        self._running = False
        if self._session and not self._session.closed:
            await self._session.close()

    async def _authenticate(self) -> None:
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
        except Exception as e:
            logger.warning("StreamMonitor auth error: %s", e)

    def _headers(self) -> dict[str, str]:
        return {
            "Client-ID": self._client_id,
            "Authorization": f"Bearer {self._access_token}",
        }

    async def monitor_streams(self) -> AsyncIterator[tuple[str, StreamStatus]]:
        """Continuously poll streams and yield state changes."""
        while self._running:
            if not self._access_token:
                await asyncio.sleep(self._poll_interval)
                continue
            try:
                changes = await self._poll_all_games()
                for channel, status in changes:
                    yield channel, status
            except Exception as e:
                logger.warning("StreamMonitor poll error: %s", e)
            await asyncio.sleep(self._poll_interval)

    async def _poll_all_games(self) -> list[tuple[str, StreamStatus]]:
        """Poll all game categories and detect state changes."""
        if not self._session:
            return []

        changes: list[tuple[str, StreamStatus]] = []
        now = time.monotonic()
        live_channels: set[str] = set()

        for game, game_id in TWITCH_GAME_IDS.items():
            try:
                params = {"game_id": game_id, "first": "50", "type": "live"}
                async with self._session.get(
                    f"{_HELIX_URL}/streams", params=params, headers=self._headers()
                ) as resp:
                    if resp.status != 200:
                        continue
                    data = await resp.json()
                    for stream in data.get("data", []):
                        if stream.get("viewer_count", 0) < 500:
                            continue
                        channel = stream["user_name"]
                        live_channels.add(channel)
                        viewers = stream.get("viewer_count", 0)

                        # Track viewer history for delay estimation
                        if channel not in self._viewer_history:
                            self._viewer_history[channel] = []
                        self._viewer_history[channel].append((now, viewers))
                        # Keep last 5 minutes
                        cutoff = now - 300
                        self._viewer_history[channel] = [
                            (t, v) for t, v in self._viewer_history[channel] if t > cutoff
                        ]

                        old_status = self._streams.get(channel)
                        old_state = old_status.state if old_status else StreamState.OFFLINE

                        new_status = StreamStatus(
                            channel=channel,
                            game=game,
                            state=StreamState.LIVE,
                            viewer_count=viewers,
                            title=stream.get("title", ""),
                            started_at=stream.get("started_at", ""),
                            last_checked=now,
                            estimated_delay_s=self._get_delay(channel),
                        )

                        if old_state != StreamState.LIVE:
                            new_status.state_changes = (
                                (old_status.state_changes if old_status else [])
                                + [(now, old_state, StreamState.LIVE)]
                            )
                            changes.append((channel, new_status))
                            logger.info(
                                "Stream %s went LIVE (%s, %d viewers)",
                                channel, game, viewers,
                            )
                        elif old_status and abs(viewers - old_status.viewer_count) > old_status.viewer_count * 0.15:
                            # Viewer spike detected - possible in-game event
                            changes.append((channel, new_status))
                            self._detect_viewer_spike(channel, old_status.viewer_count, viewers, now)

                        self._streams[channel] = new_status

            except Exception as e:
                logger.warning("StreamMonitor error for %s: %s", game, e)

        # Detect streams that went offline
        for channel, status in list(self._streams.items()):
            if channel not in live_channels and status.state == StreamState.LIVE:
                status.state = StreamState.OFFLINE
                status.state_changes.append((now, StreamState.LIVE, StreamState.OFFLINE))
                changes.append((channel, status))
                logger.info("Stream %s went OFFLINE", channel)

        return changes

    def record_api_event(self, match_id: str, event_time: float | None = None) -> None:
        """Record when a game event was received from the data API.

        Call this when GRID/PandaScore delivers an event. We later compare
        this timestamp to stream viewer reactions to estimate delay.
        """
        t = event_time or time.monotonic()
        if match_id not in self._event_timestamps:
            self._event_timestamps[match_id] = []
        self._event_timestamps[match_id].append(t)
        # Keep last 20
        self._event_timestamps[match_id] = self._event_timestamps[match_id][-20:]

    def _detect_viewer_spike(
        self, channel: str, old_count: int, new_count: int, timestamp: float
    ) -> None:
        """Detect viewer spikes that correlate with game events.

        When a big play happens, viewers typically spike 10-30s after the
        actual event (due to broadcast delay + reaction time). By
        correlating API event timestamps with viewer spikes, we can
        estimate the broadcast delay.
        """
        # Find recent API events across all matches
        now = timestamp
        for match_id, event_times in self._event_timestamps.items():
            for et in reversed(event_times):
                delay = now - et
                if 5 <= delay <= 60:
                    # Plausible broadcast delay range
                    old_estimate = self._delay_estimates.get(channel)
                    sample_count = (old_estimate.sample_count + 1) if old_estimate else 1

                    # Exponential moving average
                    if old_estimate:
                        alpha = 0.3
                        estimated = alpha * delay + (1 - alpha) * old_estimate.estimated_delay_s
                    else:
                        estimated = delay

                    self._delay_estimates[channel] = DelayEstimate(
                        channel=channel,
                        estimated_delay_s=estimated,
                        confidence=min(0.9, sample_count * 0.1),
                        sample_count=sample_count,
                        last_updated=now,
                        method="viewer_spike",
                    )
                    logger.info(
                        "Delay estimate for %s: %.1fs (confidence=%.1f, samples=%d)",
                        channel, estimated, min(0.9, sample_count * 0.1), sample_count,
                    )
                    return

    def _get_delay(self, channel: str) -> float:
        """Get current delay estimate for a channel."""
        estimate = self._delay_estimates.get(channel)
        if estimate:
            return estimate.estimated_delay_s
        return 15.0  # Default: typical Twitch low-latency is ~10-15s

    def get_delay_estimate(self, channel: str) -> DelayEstimate:
        """Get the delay estimate for a specific channel."""
        return self._delay_estimates.get(channel, DelayEstimate(
            channel=channel,
            estimated_delay_s=15.0,
            confidence=0.1,
            sample_count=0,
            last_updated=0,
            method="default",
        ))

    @property
    def live_streams(self) -> dict[str, StreamStatus]:
        return {ch: s for ch, s in self._streams.items() if s.is_live}

    @property
    def all_streams(self) -> dict[str, StreamStatus]:
        return dict(self._streams)

    @property
    def delay_estimates(self) -> dict[str, DelayEstimate]:
        return dict(self._delay_estimates)
