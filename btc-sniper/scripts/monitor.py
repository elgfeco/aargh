#!/usr/bin/env python3
"""
monitor.py — external health monitor for the BTC sniper.

Runs anywhere (NOT on the trading server — the whole point is independence).
Polls the trading host's systemd status + a local heartbeat file and sends a
Telegram alert on:

  * service not running
  * stale heartbeat (> THRESHOLD seconds)
  * latency p99 regression (> MAX_P99_US microseconds, read from stats log)
  * daily PnL below MAX_DAILY_LOSS_USDC

Usage (from a cheap tiny VM or laptop):

    export TELEGRAM_BOT_TOKEN=...
    export TELEGRAM_CHAT_ID=...
    export SNIPER_HOST=user@1.2.3.4
    python scripts/monitor.py --poll 30

Dependencies: stdlib only (urllib + subprocess).
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import subprocess
import sys
import time
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path


# --------------------------------------------------------------------------- #

@dataclass
class Config:
    sniper_host: str
    heartbeat_path: str
    poll_secs: int
    heartbeat_stale_secs: int
    max_p99_us: int
    max_daily_loss_usdc: float
    telegram_token: str | None
    telegram_chat: str | None


def load_config(argv: list[str] | None = None) -> Config:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default=os.environ.get("SNIPER_HOST", ""))
    ap.add_argument("--heartbeat", default="/opt/btc-sniper/state/heartbeat.json")
    ap.add_argument("--poll", type=int, default=30)
    ap.add_argument("--heartbeat-stale", type=int, default=60)
    ap.add_argument("--max-p99-us", type=int, default=500)
    ap.add_argument("--max-daily-loss", type=float, default=250.0)
    a = ap.parse_args(argv)
    return Config(
        sniper_host=a.host,
        heartbeat_path=a.heartbeat,
        poll_secs=a.poll,
        heartbeat_stale_secs=a.heartbeat_stale,
        max_p99_us=a.max_p99_us,
        max_daily_loss_usdc=a.max_daily_loss,
        telegram_token=os.environ.get("TELEGRAM_BOT_TOKEN"),
        telegram_chat=os.environ.get("TELEGRAM_CHAT_ID"),
    )


# --------------------------------------------------------------------------- #

def telegram(cfg: Config, text: str) -> None:
    """Best-effort Telegram send. Never raises — we don't want the monitor
    itself to crash."""
    if not (cfg.telegram_token and cfg.telegram_chat):
        print(f"[monitor] ALERT (no telegram): {text}", file=sys.stderr)
        return
    try:
        data = urllib.parse.urlencode(
            {"chat_id": cfg.telegram_chat, "text": f"[sniper] {text}"}
        ).encode()
        url = f"https://api.telegram.org/bot{cfg.telegram_token}/sendMessage"
        urllib.request.urlopen(url, data=data, timeout=5)  # noqa: S310
    except Exception as e:  # pylint: disable=broad-except
        print(f"[monitor] telegram send failed: {e}", file=sys.stderr)


def ssh_exec(host: str, cmd: str, timeout: int = 5) -> tuple[int, str]:
    if not host:
        # Running on the same host
        p = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=timeout)
        return p.returncode, (p.stdout + p.stderr)
    p = subprocess.run(
        ["ssh", "-o", "ConnectTimeout=5", "-o", "StrictHostKeyChecking=no", host, cmd],
        capture_output=True,
        text=True,
        timeout=timeout + 5,
    )
    return p.returncode, (p.stdout + p.stderr)


# --------------------------------------------------------------------------- #

def check_service(cfg: Config) -> str | None:
    rc, out = ssh_exec(cfg.sniper_host, "systemctl is-active sniper.service")
    if rc != 0 or "active" not in out:
        return f"sniper.service is not active: {out.strip()}"
    return None


def check_heartbeat(cfg: Config) -> str | None:
    rc, out = ssh_exec(cfg.sniper_host, f"cat {cfg.heartbeat_path}")
    if rc != 0:
        return f"heartbeat file missing: {out.strip()}"
    try:
        hb = json.loads(out)
    except json.JSONDecodeError:
        return "heartbeat file malformed"
    ts = hb.get("ts", 0)
    age = time.time() - ts
    if age > cfg.heartbeat_stale_secs:
        return f"heartbeat stale ({age:.0f}s > {cfg.heartbeat_stale_secs}s)"
    # p99 reported in microseconds
    p99 = hb.get("order_fired_p99_us", 0)
    if p99 > cfg.max_p99_us:
        return f"p99 order_fired {p99}µs exceeds {cfg.max_p99_us}µs"
    pnl = hb.get("daily_pnl_usdc", 0.0)
    if pnl < -cfg.max_daily_loss_usdc:
        return f"daily PnL {pnl:+.2f} < -{cfg.max_daily_loss_usdc:.2f}"
    return None


# --------------------------------------------------------------------------- #

def main(argv: list[str] | None = None) -> int:
    cfg = load_config(argv)
    hostname = socket.gethostname()
    telegram(cfg, f"monitor started on {hostname}, watching {cfg.sniper_host or 'localhost'}")

    last_state_ok = True
    while True:
        problems: list[str] = []
        for check in (check_service, check_heartbeat):
            try:
                err = check(cfg)
                if err:
                    problems.append(err)
            except Exception as e:  # pylint: disable=broad-except
                problems.append(f"{check.__name__} raised: {e}")

        if problems:
            msg = " | ".join(problems)
            print(f"[monitor] {msg}", file=sys.stderr)
            telegram(cfg, msg)
            last_state_ok = False
        elif not last_state_ok:
            telegram(cfg, "recovered")
            last_state_ok = True

        time.sleep(cfg.poll_secs)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print("monitor stopped", file=sys.stderr)
        sys.exit(0)
