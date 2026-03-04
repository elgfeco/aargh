"""Tests for order book analysis and market efficiency."""

from aargh.market.orderbook import (
    OrderBookAnalyzer,
    OrderBookLevel,
    OrderBookSnapshot,
    PriceImpact,
)


def _make_book(
    bids: list[tuple[float, float]] | None = None,
    asks: list[tuple[float, float]] | None = None,
) -> OrderBookSnapshot:
    if bids is None:
        bids = [(0.59, 500), (0.58, 300), (0.57, 200)]
    if asks is None:
        asks = [(0.61, 500), (0.62, 300), (0.63, 200)]
    return OrderBookSnapshot(
        token_id="tok-1",
        market_id="m1",
        bids=[OrderBookLevel(price=p, size=s) for p, s in bids],
        asks=[OrderBookLevel(price=p, size=s) for p, s in asks],
    )


class TestOrderBookSnapshot:
    def test_best_bid_ask(self):
        book = _make_book()
        assert book.best_bid == 0.59
        assert book.best_ask == 0.61

    def test_spread(self):
        book = _make_book()
        assert abs(book.spread - 0.02) < 0.001

    def test_spread_pct(self):
        book = _make_book()
        assert book.spread_pct > 0  # ~3.3%

    def test_mid_price(self):
        book = _make_book()
        assert abs(book.mid_price - 0.60) < 0.01

    def test_total_liquidity(self):
        book = _make_book()
        assert book.total_bid_liquidity == 1000
        assert book.total_ask_liquidity == 1000

    def test_bid_ask_imbalance_balanced(self):
        book = _make_book()
        assert abs(book.bid_ask_imbalance) < 0.01

    def test_bid_ask_imbalance_skewed(self):
        book = _make_book(
            bids=[(0.59, 1000)],
            asks=[(0.61, 200)],
        )
        assert book.bid_ask_imbalance > 0  # More buy pressure

    def test_empty_book(self):
        book = OrderBookSnapshot(token_id="t", market_id="m", bids=[], asks=[])
        assert book.best_bid == 0.0
        assert book.best_ask == 1.0
        assert book.mid_price == 0.5


class TestPriceImpact:
    def test_small_order_low_impact(self):
        book = _make_book()
        analyzer = OrderBookAnalyzer.__new__(OrderBookAnalyzer)
        impact = analyzer.estimate_price_impact(book, "buy", 100)
        assert impact.slippage_pct < 2.0
        assert impact.levels_consumed <= 1

    def test_large_order_high_impact(self):
        book = _make_book()
        analyzer = OrderBookAnalyzer.__new__(OrderBookAnalyzer)
        impact = analyzer.estimate_price_impact(book, "buy", 900)
        assert impact.slippage_pct > 0
        assert impact.levels_consumed >= 2

    def test_order_exceeding_book_depth(self):
        book = _make_book(asks=[(0.61, 100)])
        analyzer = OrderBookAnalyzer.__new__(OrderBookAnalyzer)
        impact = analyzer.estimate_price_impact(book, "buy", 500)
        assert impact.levels_consumed == 1
        # Only partially filled

    def test_sell_side_impact(self):
        book = _make_book()
        analyzer = OrderBookAnalyzer.__new__(OrderBookAnalyzer)
        impact = analyzer.estimate_price_impact(book, "sell", 200)
        assert impact.side == "sell"
        assert impact.slippage_pct >= 0
