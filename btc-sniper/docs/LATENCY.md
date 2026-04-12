# Latency Measurement & Tuning

This document describes how the BTC sniper measures each stage of its pipeline,
what the targets are, and how to investigate regressions.

## Pipeline stages

```
  Polymarket WS frame
        │
        ▼  ws_parse          (target: < 5 µs)
  PolymarketMessage
        │
        ▼  signal_eval       (target: < 10 µs)
  Decision (Intent::Fire | Hold)
        │
        ▼  order_fired       (target: < 50 µs from decision to TCP write)
  HTTP POST /order on the wire
        │
        ▼  order_acked       (target: < 5 ms, network-bound)
  CLOB response / fill confirmation
```

The stages are defined in `crates/executor/src/latency.rs::LatencyStage` and
recorded into per-stage `hdrhistogram::Histogram<u64>` instances. The
histograms span `[1 ns, 60 s]` with 3 significant digits, costing ≈4 KiB per
stage.

## Collecting percentiles

Send `SIGUSR1` to the running process to dump current p50/p95/p99/p99.9/max
for every stage. The `main.rs` signal handler wires this to `stats.snapshot()`
and prints to stdout in the `LatencySnapshot::Display` format:

```
stage         count     p50     p95     p99    p999     max
ws_parse       123456   2100ns  3900ns  5200ns  7800ns 12000ns
signal_eval    123456    520ns   900ns  1400ns  2800ns  6100ns
order_fired      3421  42000ns 58000ns 81000ns 132000ns 245000ns
order_acked      3420   2.1ms   3.2ms   4.5ms   7.8ms  12.3ms
```

From a shell:

```bash
pkill -USR1 sniper
journalctl -u sniper.service -n 40
```

## Measurement methodology

Each stage uses `std::time::Instant::now()`:

```rust
let t0 = Instant::now();
// ... work ...
stats.record(LatencyStage::SignalEval, t0.elapsed().as_nanos() as u64);
```

* `Instant::now()` on Linux calls `clock_gettime(CLOCK_MONOTONIC)` which
  resolves to the TSC via vDSO on modern x86 / ARM64, so the overhead is
  ~15 ns — negligible relative to the stages we're measuring.
* `hdrhistogram::Histogram::record` is lock-protected via `parking_lot::Mutex`.
  In practice contention is near-zero because each stage is recorded from a
  single writer thread.

## Targets and ceilings

| stage        | target (p99) | hard ceiling (p99) |
| ------------ | ------------ | ------------------ |
| ws_parse     | 5 µs         | 25 µs              |
| signal_eval  | 10 µs        | 50 µs              |
| order_fired  | 50 µs        | 250 µs             |
| order_acked  | 5 ms         | 20 ms              |

If any stage exceeds its hard ceiling for > 60 seconds, the monitor script
(`scripts/monitor.py`) pages via Telegram. The `order_acked` stage is
network-bound and depends on distance to Polymarket's infra; us-east-1 cluster
placement group is assumed.

## Common causes of regression

1. **`ws_parse` creeping above 25 µs** — scratch buffer is allocating
   per-frame. Check that `PolymarketFeed::run_once` is reusing `scratch` and
   not calling `Vec::new()` inside the loop.
2. **`signal_eval` p99 spiky** — the hot path has drifted into floating-point
   or allocation. Run `cargo flamegraph --bench signal_hot_path` and confirm
   `EdgeEngine::evaluate` stays in integer-ops only (see the design comment in
   `crates/signal/src/edge.rs`).
3. **`order_fired` > 250 µs** — reqwest is opening a new TCP connection. Check
   `ClobClient::inner` was built with `http2_prior_knowledge()` and
   `pool_max_idle_per_host(8)`. A restart of `sniper.service` triggers fresh
   connections.
4. **`order_acked` > 20 ms** — likely a network path issue. Run
   `mtr -rwc 20 clob.polymarket.com`. Check `net.ipv4.tcp_congestion_control`
   is `bbr` (our setup.sh sets this).

## Deployment checklist

Before every production deploy, verify:

- [ ] `setup.sh` ran without WARNings
- [ ] `/proc/sys/vm/nr_hugepages` shows `2048`
- [ ] `sysctl net.core.busy_poll` shows `50`
- [ ] `sysctl net.ipv4.tcp_congestion_control` shows `bbr`
- [ ] `cat /sys/devices/system/cpu/cpu4/cpufreq/scaling_governor` shows `performance`
- [ ] `cat /sys/devices/system/cpu/smt/control` shows `off`
- [ ] `cat /proc/irq/*/smp_affinity` — NIC IRQs on cores 0-3 only
- [ ] `systemctl status sniper.service` — CPUAffinity `4-63`, SCHED_FIFO 99
- [ ] `ping clob.polymarket.com` — RTT < 2 ms from us-east-1
- [ ] `cargo test --workspace --release` — all green
- [ ] After 5 min of warm-up, `pkill -USR1 sniper` shows p99s within target

## Benchmark harness

The `crates/signal` crate exposes the `EdgeEngine::evaluate` function publicly
so you can drive it from a Criterion benchmark. The intended bench (not yet
wired into `bench/`) looks like:

```rust
use criterion::{criterion_group, criterion_main, Criterion};
use sniper_feed::{BookSnapshot, PriceLevel, Price, Size, TapeSnapshot};
use sniper_signal::{EdgeEngine, EdgeParams, PositionSizer};

fn bench(c: &mut Criterion) {
    let engine = EdgeEngine::new(
        EdgeParams::default(),
        PositionSizer::new(10_000.0, 500.0, 0.15),
    );
    let book = BookSnapshot {
        best_bid: Some(PriceLevel { price: Price::from_prob(0.50), size: Size::from_usdc(1000.0) }),
        best_ask: Some(PriceLevel { price: Price::from_prob(0.51), size: Size::from_usdc(1000.0) }),
        bid_depth: 1, ask_depth: 1, seq: 1,
    };
    let tape = TapeSnapshot { momentum: 0.4, ..Default::default() };

    c.bench_function("edge_evaluate", |b| {
        b.iter(|| engine.evaluate(&book, &tape));
    });
}
criterion_group!(benches, bench);
criterion_main!(benches);
```

Expected: `edge_evaluate` ~ 400-600 ns on Graviton3 at `-C target-cpu=native`.
