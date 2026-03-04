"""Timing advantage quantification and latency tracking.

Measures the information edge window: the gap between when game data
arrives via API versus when Twitch viewers see the same event.

This module tracks:
- API feed latency (how fast GRID/PandaScore delivers events)
- Estimated broadcast delay (from StreamMonitor)
- Net information edge (API speed - stream delay = trading window)
- Historical timing statistics
"""

from __future__ import annotations

import logging
import time
from collections import deque
from dataclasses import dataclass, field
from datetime import datetime, timezone
from statistics import mean, median, stdev

logger = logging.getLogger(__name__)


@dataclass
class LatencyRecord:
    """A single latency measurement for a feed."""

    feed_name: str
    match_id: str
    event_type: str
    request_time: float  # monotonic time when request was sent
    response_time: float  # monotonic time when response was received
    event_timestamp: str  # ISO timestamp from the API (when event actually happened)
    latency_ms: float  # response_time - request_time in ms

    @property
    def api_delay_s(self) -> float:
        """Estimated delay between event occurrence and API delivery."""
        try:
            event_dt = datetime.fromisoformat(self.event_timestamp.replace("Z", "+00:00"))
            now = datetime.now(timezone.utc)
            return max(0, (now - event_dt).total_seconds() - self.latency_ms / 1000)
        except (ValueError, TypeError):
            return self.latency_ms / 1000


@dataclass
class FeedLatencyStats:
    """Aggregate latency statistics for a feed."""

    feed_name: str
    sample_count: int
    avg_latency_ms: float
    median_latency_ms: float
    p95_latency_ms: float
    min_latency_ms: float
    max_latency_ms: float
    stddev_latency_ms: float


@dataclass
class EdgeWindow:
    """The calculated information edge window for a match."""

    match_id: str
    feed_name: str
    stream_channel: str | None
    api_latency_s: float  # How fast API delivers events
    stream_delay_s: float  # Estimated broadcast delay
    edge_window_s: float  # stream_delay - api_latency = our trading window
    confidence: float  # 0-1
    timestamp: datetime = field(default_factory=lambda: datetime.now(timezone.utc))

    @property
    def has_edge(self) -> bool:
        return self.edge_window_s > 2.0  # Need at least 2s to act

    @property
    def edge_quality(self) -> str:
        if self.edge_window_s >= 20:
            return "excellent"
        if self.edge_window_s >= 10:
            return "good"
        if self.edge_window_s >= 5:
            return "moderate"
        if self.edge_window_s >= 2:
            return "marginal"
        return "none"


class TimingTracker:
    """Tracks API latencies and calculates information edge windows."""

    def __init__(self, max_samples: int = 500):
        self._max_samples = max_samples
        self._latency_records: dict[str, deque[LatencyRecord]] = {}  # feed_name -> records
        self._edge_windows: dict[str, EdgeWindow] = {}  # match_id -> latest edge
        self._request_starts: dict[str, float] = {}  # request_id -> start time

    def start_request(self, request_id: str) -> None:
        """Mark the start of an API request."""
        self._request_starts[request_id] = time.monotonic()

    def record_response(
        self,
        request_id: str,
        feed_name: str,
        match_id: str,
        event_type: str,
        event_timestamp: str = "",
    ) -> LatencyRecord | None:
        """Record a completed API response and its latency."""
        start = self._request_starts.pop(request_id, None)
        if start is None:
            return None

        now = time.monotonic()
        latency_ms = (now - start) * 1000

        record = LatencyRecord(
            feed_name=feed_name,
            match_id=match_id,
            event_type=event_type,
            request_time=start,
            response_time=now,
            event_timestamp=event_timestamp or datetime.now(timezone.utc).isoformat(),
            latency_ms=latency_ms,
        )

        if feed_name not in self._latency_records:
            self._latency_records[feed_name] = deque(maxlen=self._max_samples)
        self._latency_records[feed_name].append(record)

        logger.debug(
            "Feed %s latency: %.1fms (match=%s, event=%s)",
            feed_name, latency_ms, match_id, event_type,
        )
        return record

    def record_latency_direct(
        self,
        feed_name: str,
        match_id: str,
        event_type: str,
        latency_ms: float,
        event_timestamp: str = "",
    ) -> None:
        """Record a latency measurement directly (when start/end tracking isn't used)."""
        now = time.monotonic()
        record = LatencyRecord(
            feed_name=feed_name,
            match_id=match_id,
            event_type=event_type,
            request_time=now - latency_ms / 1000,
            response_time=now,
            event_timestamp=event_timestamp or datetime.now(timezone.utc).isoformat(),
            latency_ms=latency_ms,
        )
        if feed_name not in self._latency_records:
            self._latency_records[feed_name] = deque(maxlen=self._max_samples)
        self._latency_records[feed_name].append(record)

    def get_feed_stats(self, feed_name: str) -> FeedLatencyStats | None:
        """Get aggregate latency stats for a feed."""
        records = self._latency_records.get(feed_name)
        if not records:
            return None

        latencies = [r.latency_ms for r in records]
        sorted_lat = sorted(latencies)
        p95_idx = max(0, int(len(sorted_lat) * 0.95) - 1)

        return FeedLatencyStats(
            feed_name=feed_name,
            sample_count=len(latencies),
            avg_latency_ms=mean(latencies),
            median_latency_ms=median(latencies),
            p95_latency_ms=sorted_lat[p95_idx],
            min_latency_ms=min(latencies),
            max_latency_ms=max(latencies),
            stddev_latency_ms=stdev(latencies) if len(latencies) > 1 else 0,
        )

    def calculate_edge_window(
        self,
        match_id: str,
        feed_name: str,
        stream_delay_s: float,
        stream_channel: str | None = None,
        delay_confidence: float = 0.5,
    ) -> EdgeWindow:
        """Calculate the information edge window for a match.

        edge_window = stream_delay - api_latency
        If positive, we receive information before stream viewers.
        """
        stats = self.get_feed_stats(feed_name)
        if stats:
            api_latency_s = stats.median_latency_ms / 1000
        else:
            api_latency_s = 1.0  # Default 1s if no data

        edge_s = max(0, stream_delay_s - api_latency_s)

        # Confidence combines feed latency certainty and stream delay certainty
        feed_confidence = min(0.9, (stats.sample_count / 20) if stats else 0.1)
        confidence = feed_confidence * delay_confidence

        edge = EdgeWindow(
            match_id=match_id,
            feed_name=feed_name,
            stream_channel=stream_channel,
            api_latency_s=api_latency_s,
            stream_delay_s=stream_delay_s,
            edge_window_s=edge_s,
            confidence=confidence,
        )

        self._edge_windows[match_id] = edge

        logger.info(
            "Edge window for %s: %.1fs (API=%.1fs, stream=%.1fs, quality=%s, conf=%.1f)",
            match_id, edge_s, api_latency_s, stream_delay_s, edge.edge_quality, confidence,
        )
        return edge

    def get_edge_window(self, match_id: str) -> EdgeWindow | None:
        return self._edge_windows.get(match_id)

    @property
    def all_edge_windows(self) -> dict[str, EdgeWindow]:
        return dict(self._edge_windows)

    @property
    def all_feed_stats(self) -> dict[str, FeedLatencyStats]:
        stats = {}
        for feed_name in self._latency_records:
            s = self.get_feed_stats(feed_name)
            if s:
                stats[feed_name] = s
        return stats
