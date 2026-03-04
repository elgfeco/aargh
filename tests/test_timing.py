"""Tests for timing advantage and latency tracking."""

from aargh.signals.timing import TimingTracker, EdgeWindow


class TestTimingTracker:
    def test_record_latency_direct(self):
        tracker = TimingTracker()
        tracker.record_latency_direct("grid", "match-1", "round_ended", 150.0)
        tracker.record_latency_direct("grid", "match-1", "round_ended", 200.0)
        stats = tracker.get_feed_stats("grid")
        assert stats is not None
        assert stats.sample_count == 2
        assert stats.avg_latency_ms == 175.0

    def test_start_record_response(self):
        tracker = TimingTracker()
        tracker.start_request("req-1")
        record = tracker.record_response("req-1", "pandascore", "m1", "round_ended")
        assert record is not None
        assert record.latency_ms >= 0
        assert record.feed_name == "pandascore"

    def test_unknown_request_returns_none(self):
        tracker = TimingTracker()
        record = tracker.record_response("unknown", "grid", "m1", "round_ended")
        assert record is None

    def test_feed_stats_percentiles(self):
        tracker = TimingTracker()
        for i in range(20):
            tracker.record_latency_direct("grid", "m1", "round_ended", float(i * 10))
        stats = tracker.get_feed_stats("grid")
        assert stats is not None
        assert stats.sample_count == 20
        assert stats.min_latency_ms == 0.0
        assert stats.max_latency_ms == 190.0
        assert stats.p95_latency_ms >= 170.0

    def test_no_stats_for_unknown_feed(self):
        tracker = TimingTracker()
        assert tracker.get_feed_stats("unknown") is None


class TestEdgeWindow:
    def test_calculate_with_good_edge(self):
        tracker = TimingTracker()
        # Add some latency samples
        for _ in range(5):
            tracker.record_latency_direct("grid", "m1", "round_ended", 500.0)

        edge = tracker.calculate_edge_window("m1", "grid", stream_delay_s=20.0)
        assert edge.edge_window_s > 15  # 20s stream - 0.5s API
        assert edge.has_edge
        assert edge.edge_quality in ("excellent", "good")

    def test_no_edge_when_api_slow(self):
        tracker = TimingTracker()
        for _ in range(5):
            tracker.record_latency_direct("slow_feed", "m1", "round_ended", 25000.0)

        edge = tracker.calculate_edge_window("m1", "slow_feed", stream_delay_s=10.0)
        assert edge.edge_window_s == 0  # API slower than stream
        assert not edge.has_edge
        assert edge.edge_quality == "none"

    def test_edge_quality_levels(self):
        edge = EdgeWindow(
            match_id="m1", feed_name="grid", stream_channel="esl",
            api_latency_s=0.5, stream_delay_s=25.5, edge_window_s=25.0,
            confidence=0.8,
        )
        assert edge.edge_quality == "excellent"

        edge.edge_window_s = 12.0
        assert edge.edge_quality == "good"

        edge.edge_window_s = 6.0
        assert edge.edge_quality == "moderate"

        edge.edge_window_s = 3.0
        assert edge.edge_quality == "marginal"

        edge.edge_window_s = 1.0
        assert edge.edge_quality == "none"

    def test_all_edge_windows_dict(self):
        tracker = TimingTracker()
        tracker.record_latency_direct("grid", "m1", "round_ended", 100.0)
        tracker.calculate_edge_window("m1", "grid", stream_delay_s=15.0)
        tracker.calculate_edge_window("m2", "grid", stream_delay_s=20.0)
        assert len(tracker.all_edge_windows) == 2
