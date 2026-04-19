#!/usr/bin/env python3
"""
backtest.py — historical edge analysis for the BTC Polymarket sniper.

Feeds the Rust signal engine's *exact same* edge formula through a historical
replay:

  * load Polymarket YES-token mid-prices from CSV snapshots (`data/poly_*.csv`)
  * load BTC trade prints from CSV (`data/btc_*.csv`)
  * walk forward in time, maintain EMA(5)/EMA(20) on BTC prints
  * compute edge_bps = (model_prob - poly_mid_prob) * 10_000 - tx_cost
  * "fire" when |edge| >= min_edge_bps, simulate a +1-tick fill at the
    current opposite quote
  * output PnL curve, win rate, avg edge, Sharpe

CSV format expected:

  poly:  ts_ms,best_bid,best_ask
  btc :  ts_ms,price,size

Usage:
    python scripts/backtest.py \
        --poly data/poly_btc70k.csv \
        --btc  data/btc.csv \
        --min-edge-bps 250 \
        --kelly 0.15 \
        --bankroll 10000

The script is intentionally dependency-free (stdlib only) so it runs without
a Python environment setup.
"""

from __future__ import annotations

import argparse
import csv
import math
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterator


# --------------------------------------------------------------------------- #
# Data loading
# --------------------------------------------------------------------------- #

@dataclass
class PolySnap:
    ts_ms: int
    best_bid: float
    best_ask: float

    @property
    def mid(self) -> float:
        return 0.5 * (self.best_bid + self.best_ask)


@dataclass
class BtcTick:
    ts_ms: int
    price: float
    size: float


def load_poly(path: Path) -> list[PolySnap]:
    out: list[PolySnap] = []
    with path.open() as f:
        reader = csv.DictReader(f)
        for row in reader:
            try:
                out.append(
                    PolySnap(
                        ts_ms=int(row["ts_ms"]),
                        best_bid=float(row["best_bid"]),
                        best_ask=float(row["best_ask"]),
                    )
                )
            except (KeyError, ValueError):
                continue
    return out


def load_btc(path: Path) -> list[BtcTick]:
    out: list[BtcTick] = []
    with path.open() as f:
        reader = csv.DictReader(f)
        for row in reader:
            try:
                out.append(
                    BtcTick(
                        ts_ms=int(row["ts_ms"]),
                        price=float(row["price"]),
                        size=float(row["size"]),
                    )
                )
            except (KeyError, ValueError):
                continue
    return out


# --------------------------------------------------------------------------- #
# Signal logic — MUST stay in sync with crates/signal/src/edge.rs
# --------------------------------------------------------------------------- #

@dataclass
class Tape:
    ema_fast: float = 0.0
    ema_slow: float = 0.0
    last: float = 0.0
    count: int = 0

    ALPHA_FAST: float = 2.0 / (5.0 + 1.0)
    ALPHA_SLOW: float = 2.0 / (20.0 + 1.0)

    def push(self, price: float) -> None:
        if self.count == 0:
            self.ema_fast = self.ema_slow = price
        else:
            self.ema_fast = self.ALPHA_FAST * price + (1 - self.ALPHA_FAST) * self.ema_fast
            self.ema_slow = self.ALPHA_SLOW * price + (1 - self.ALPHA_SLOW) * self.ema_slow
        self.last = price
        self.count += 1

    def momentum(self) -> float:
        if self.count < 2 or self.ema_slow == 0.0:
            return 0.0
        delta = self.ema_fast - self.ema_slow
        return max(-1.0, min(1.0, delta / (self.ema_slow * 0.005)))

    def directional_prob(self, max_conviction_bps: int = 1000) -> float:
        shift = self.momentum() * max_conviction_bps / 10_000
        return max(0.001, min(0.999, 0.5 + shift))


def edge_bps(model_prob: float, poly_mid: float, tx_cost_bps: int) -> int:
    raw = int(round((model_prob - poly_mid) * 10_000))
    if raw > 0:
        return max(0, raw - tx_cost_bps)
    return min(0, raw + tx_cost_bps)


def kelly_fraction(edge_bps_: int, kelly: float) -> float:
    """Matches the (X=0.5 symmetric) formula in crates/signal/src/kelly.rs."""
    if edge_bps_ <= 0:
        return 0.0
    f_star = min(1.0, edge_bps_ / 5_000.0)  # 2*edge, capped
    return max(0.0, f_star * kelly)


# --------------------------------------------------------------------------- #
# Backtest loop
# --------------------------------------------------------------------------- #

@dataclass
class Fill:
    ts_ms: int
    side: str      # "BUY" or "SELL" (of YES)
    price: float
    size_usdc: float
    edge_bps: int
    pnl: float = 0.0


@dataclass
class Report:
    fills: list[Fill] = field(default_factory=list)
    total_pnl: float = 0.0
    wins: int = 0
    losses: int = 0

    @property
    def n(self) -> int:
        return len(self.fills)

    @property
    def win_rate(self) -> float:
        return self.wins / self.n if self.n else 0.0

    @property
    def avg_edge_bps(self) -> float:
        return sum(abs(f.edge_bps) for f in self.fills) / self.n if self.n else 0.0

    def sharpe(self) -> float:
        if self.n < 2:
            return 0.0
        pnls = [f.pnl for f in self.fills]
        m = sum(pnls) / self.n
        var = sum((p - m) ** 2 for p in pnls) / (self.n - 1)
        return m / math.sqrt(var) if var > 0 else 0.0


def merge_streams(poly: list[PolySnap], btc: list[BtcTick]) -> Iterator[tuple[str, object]]:
    """Yield (kind, event) in wall-clock order."""
    i = j = 0
    while i < len(poly) and j < len(btc):
        if poly[i].ts_ms <= btc[j].ts_ms:
            yield "poly", poly[i]
            i += 1
        else:
            yield "btc", btc[j]
            j += 1
    while i < len(poly):
        yield "poly", poly[i]
        i += 1
    while j < len(btc):
        yield "btc", btc[j]
        j += 1


def backtest(
    poly: list[PolySnap],
    btc: list[BtcTick],
    *,
    min_edge_bps: int,
    tx_cost_bps: int,
    kelly: float,
    bankroll: float,
    max_position_usdc: float,
) -> Report:
    tape = Tape()
    report = Report()
    last_poly: PolySnap | None = None
    # open positions: (entry_price, size_usdc, side)
    open_pos: list[tuple[float, float, str]] = []

    for kind, ev in merge_streams(poly, btc):
        if kind == "btc":
            tape.push(ev.price)  # type: ignore[attr-defined]
            continue

        snap: PolySnap = ev  # type: ignore[assignment]
        last_poly = snap
        if tape.count < 2:
            continue

        model_prob = tape.directional_prob()
        mid = snap.mid
        eb = edge_bps(model_prob, mid, tx_cost_bps)

        # Mark-to-market open positions using the new mid — resolve on
        # opposite-side signal or at EOF.
        still_open: list[tuple[float, float, str]] = []
        for entry, sz, side in open_pos:
            if (side == "BUY" and eb <= -min_edge_bps) or (side == "SELL" and eb >= min_edge_bps):
                # Close at current opposite quote
                exit_px = snap.best_bid if side == "BUY" else snap.best_ask
                shares = sz / entry
                if side == "BUY":
                    pnl = shares * (exit_px - entry)
                else:
                    pnl = shares * (entry - exit_px)
                report.total_pnl += pnl
                report.fills.append(
                    Fill(
                        ts_ms=snap.ts_ms,
                        side=f"CLOSE-{side}",
                        price=exit_px,
                        size_usdc=sz,
                        edge_bps=eb,
                        pnl=pnl,
                    )
                )
                if pnl >= 0:
                    report.wins += 1
                else:
                    report.losses += 1
            else:
                still_open.append((entry, sz, side))
        open_pos = still_open

        # Open new position?
        if abs(eb) >= min_edge_bps and len(open_pos) < 4:
            frac = kelly_fraction(abs(eb), kelly)
            size_usdc = min(bankroll * frac, max_position_usdc)
            if size_usdc <= 0:
                continue
            if eb > 0:
                side = "BUY"
                entry = snap.best_ask
            else:
                side = "SELL"
                entry = snap.best_bid
            open_pos.append((entry, size_usdc, side))

    # Flatten any stragglers at last known mid.
    if last_poly is not None:
        mid = last_poly.mid
        for entry, sz, side in open_pos:
            shares = sz / entry
            pnl = shares * (mid - entry) if side == "BUY" else shares * (entry - mid)
            report.total_pnl += pnl
            report.fills.append(
                Fill(
                    ts_ms=last_poly.ts_ms,
                    side=f"FLAT-{side}",
                    price=mid,
                    size_usdc=sz,
                    edge_bps=0,
                    pnl=pnl,
                )
            )
            (report.wins if pnl >= 0 else report.losses).__add__(1)

    return report


# --------------------------------------------------------------------------- #
# CLI
# --------------------------------------------------------------------------- #

def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--poly", type=Path, required=True)
    ap.add_argument("--btc", type=Path, required=True)
    ap.add_argument("--min-edge-bps", type=int, default=250)
    ap.add_argument("--tx-cost-bps", type=int, default=0)
    ap.add_argument("--kelly", type=float, default=0.15)
    ap.add_argument("--bankroll", type=float, default=10_000.0)
    ap.add_argument("--max-position", type=float, default=500.0)
    ap.add_argument("--out", type=Path, default=None, help="optional CSV fill log")
    args = ap.parse_args(argv)

    poly = load_poly(args.poly)
    btc = load_btc(args.btc)
    if not poly or not btc:
        print("error: no data loaded", file=sys.stderr)
        return 1

    report = backtest(
        poly,
        btc,
        min_edge_bps=args.min_edge_bps,
        tx_cost_bps=args.tx_cost_bps,
        kelly=args.kelly,
        bankroll=args.bankroll,
        max_position_usdc=args.max_position,
    )

    print(f"fills       : {report.n}")
    print(f"total PnL   : {report.total_pnl:+.2f} USDC")
    print(f"win rate    : {report.win_rate * 100:.1f}%")
    print(f"avg |edge|  : {report.avg_edge_bps:.0f} bps")
    print(f"sharpe      : {report.sharpe():.2f}")

    if args.out:
        with args.out.open("w") as f:
            w = csv.writer(f)
            w.writerow(["ts_ms", "side", "price", "size_usdc", "edge_bps", "pnl"])
            for fill in report.fills:
                w.writerow([fill.ts_ms, fill.side, fill.price, fill.size_usdc, fill.edge_bps, fill.pnl])
        print(f"wrote {args.out}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
