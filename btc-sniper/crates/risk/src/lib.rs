//! # sniper-risk
//!
//! Hard risk limits and the global kill switch.
//!
//! The [`RiskEngine`] is consulted by the executor before every order:
//!
//! ```text
//!     submit_intent → risk.check_and_reserve(...) → true → fire
//!                                            ↘ false → drop
//! ```
//!
//! Limits enforced:
//!
//! * `max_daily_loss_usdc`   — kill switch, latches once tripped
//! * `max_open_positions`    — circuit breaker
//! * `max_position_usdc`     — per-trade cap (also checked upstream by sizing)
//! * **correlation check**   — never open YES and NO on the same market
//!
//! All counters are stored as `i64` USDC atoms (signed to allow negative
//! PnL) in atomics — zero locks on the hot path.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use sniper_feed::{AssetId, MarketId, Side, Size};
use tracing::{error, info, warn};

/// Immutable limits loaded from env.
#[derive(Clone, Copy, Debug)]
pub struct RiskLimits {
    /// Max allowed realised loss per UTC day, in USDC atoms. Negative.
    pub max_daily_loss_atoms: i64,
    /// Circuit breaker on simultaneous open positions.
    pub max_open_positions: usize,
    /// Per-trade size cap (atoms).
    pub max_position_atoms: u64,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_daily_loss_atoms: -250 * 1_000_000,
            max_open_positions: 8,
            max_position_atoms: 500 * 1_000_000,
        }
    }
}

/// Reason a `check_and_reserve` call was denied. Useful for dashboard &
/// metrics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectReason {
    KillSwitch,
    DailyLoss,
    TooManyPositions,
    SizeCap,
    CorrelatedExposure,
    Paused,
}

/// Outcome of a risk check.
#[derive(Clone, Copy, Debug)]
pub enum RiskVerdict {
    Allow,
    Deny(RejectReason),
}

/// Mapping asset → the MarketId it belongs to. We need it to enforce the
/// "no YES+NO on same market" rule. Built at startup from `markets.toml`.
#[derive(Clone, Debug, Default)]
pub struct AssetIndex {
    /// asset_id → (market_id, is_yes)
    map: DashMap<AssetId, (MarketId, bool)>,
}

impl AssetIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, asset: AssetId, market: MarketId, is_yes: bool) {
        self.map.insert(asset, (market, is_yes));
    }

    pub fn lookup(&self, asset: &AssetId) -> Option<(MarketId, bool)> {
        self.map.get(asset).map(|v| *v)
    }
}

/// Central risk engine. `Arc<Self>` is cloned into the executor and the
/// dashboard. All state is atomic.
pub struct RiskEngine {
    limits: RiskLimits,
    kill_latched: AtomicBool,
    daily_pnl_atoms: AtomicI64,
    realised_pnl_atoms: AtomicI64,
    open_positions: AtomicU64,
    /// For correlation check: market_id → side_mask (bit 0 = YES, bit 1 = NO)
    exposures: DashMap<MarketId, u8>,
    index: Arc<AssetIndex>,
}

impl RiskEngine {
    pub fn new(limits: RiskLimits, index: Arc<AssetIndex>) -> Arc<Self> {
        Arc::new(Self {
            limits,
            kill_latched: AtomicBool::new(false),
            daily_pnl_atoms: AtomicI64::new(0),
            realised_pnl_atoms: AtomicI64::new(0),
            open_positions: AtomicU64::new(0),
            exposures: DashMap::new(),
            index,
        })
    }

    pub fn limits(&self) -> &RiskLimits {
        &self.limits
    }

    /// Trip the kill switch. Subsequent checks all return Deny(KillSwitch)
    /// until the process is restarted. Intended for SIGINT / severe error.
    pub fn trip(&self, reason: &'static str) {
        if !self.kill_latched.swap(true, Ordering::SeqCst) {
            error!(reason, "KILL SWITCH LATCHED — no further orders will be placed");
        }
    }

    pub fn is_tripped(&self) -> bool {
        self.kill_latched.load(Ordering::Relaxed)
    }

    pub fn open_positions(&self) -> u64 {
        self.open_positions.load(Ordering::Relaxed)
    }

    pub fn daily_pnl_atoms(&self) -> i64 {
        self.daily_pnl_atoms.load(Ordering::Relaxed)
    }

    pub fn realised_pnl_atoms(&self) -> i64 {
        self.realised_pnl_atoms.load(Ordering::Relaxed)
    }

    /// Hot-path check. Returns [`RiskVerdict::Allow`] if the order can be
    /// fired, or a denial with reason. Reserves the position slot on Allow.
    pub fn check_and_reserve(&self, asset: AssetId, side: Side, size: Size) -> RiskVerdict {
        if self.is_tripped() {
            return RiskVerdict::Deny(RejectReason::KillSwitch);
        }
        if self.daily_pnl_atoms.load(Ordering::Relaxed) <= self.limits.max_daily_loss_atoms {
            self.trip("daily loss limit hit");
            return RiskVerdict::Deny(RejectReason::DailyLoss);
        }
        if size.0 > self.limits.max_position_atoms {
            return RiskVerdict::Deny(RejectReason::SizeCap);
        }
        if self.open_positions.load(Ordering::Relaxed) as usize >= self.limits.max_open_positions {
            return RiskVerdict::Deny(RejectReason::TooManyPositions);
        }

        // Correlation check: never hold YES and NO legs on the same market.
        if let Some((market, is_yes)) = self.index.lookup(&asset) {
            let mut bit = if is_yes { 0b01 } else { 0b10 };
            let other_bit = if is_yes { 0b10 } else { 0b01 };

            // Compare-exchange loop on the exposure bitmap.
            let mut current = self.exposures.entry(market).or_insert(0);
            if *current & other_bit != 0 {
                return RiskVerdict::Deny(RejectReason::CorrelatedExposure);
            }
            bit |= *current;
            *current = bit;
        }

        self.open_positions.fetch_add(1, Ordering::Relaxed);
        match side {
            Side::Buy => {}
            Side::Sell => {}
        }
        RiskVerdict::Allow
    }

    /// Release the position reservation taken by `check_and_reserve` — call
    /// on order fill/cancel/reject so the counter stays accurate.
    pub fn release(&self, asset: AssetId) {
        if self.open_positions.load(Ordering::Relaxed) > 0 {
            self.open_positions.fetch_sub(1, Ordering::Relaxed);
        }
        if let Some((market, _)) = self.index.lookup(&asset) {
            self.exposures.remove(&market);
        }
    }

    /// Apply a realised PnL delta (positive = profit, negative = loss). Trips
    /// the kill switch if the daily loss cap is exceeded.
    pub fn record_pnl(&self, delta_atoms: i64) {
        let new_daily = self.daily_pnl_atoms.fetch_add(delta_atoms, Ordering::Relaxed) + delta_atoms;
        self.realised_pnl_atoms.fetch_add(delta_atoms, Ordering::Relaxed);
        if new_daily <= self.limits.max_daily_loss_atoms {
            warn!(
                daily_atoms = new_daily,
                limit = self.limits.max_daily_loss_atoms,
                "daily loss hit — tripping kill switch"
            );
            self.trip("daily loss exceeded");
        }
    }

    /// Reset the daily counter — call at UTC midnight from a tokio task.
    pub fn reset_daily(&self) {
        self.daily_pnl_atoms.store(0, Ordering::Relaxed);
        info!("daily PnL reset");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(byte: u8) -> AssetId {
        [byte; 32]
    }
    fn market(byte: u8) -> MarketId {
        [byte; 32]
    }

    fn engine() -> (Arc<RiskEngine>, Arc<AssetIndex>) {
        let idx = Arc::new(AssetIndex::new());
        let eng = RiskEngine::new(
            RiskLimits {
                max_daily_loss_atoms: -100 * 1_000_000,
                max_open_positions: 2,
                max_position_atoms: 500 * 1_000_000,
            },
            idx.clone(),
        );
        (eng, idx)
    }

    #[test]
    fn kill_switch_latches() {
        let (eng, _) = engine();
        eng.trip("test");
        assert!(eng.is_tripped());
        let v = eng.check_and_reserve(asset(1), Side::Buy, Size::from_usdc(10.0));
        assert!(matches!(v, RiskVerdict::Deny(RejectReason::KillSwitch)));
    }

    #[test]
    fn size_cap_blocks_oversized_orders() {
        let (eng, _) = engine();
        let v = eng.check_and_reserve(asset(1), Side::Buy, Size::from_usdc(1_000.0));
        assert!(matches!(v, RiskVerdict::Deny(RejectReason::SizeCap)));
    }

    #[test]
    fn correlation_blocks_no_after_yes() {
        let (eng, idx) = engine();
        idx.insert(asset(1), market(9), true); // YES
        idx.insert(asset(2), market(9), false); // NO on same market
        let v1 = eng.check_and_reserve(asset(1), Side::Buy, Size::from_usdc(10.0));
        assert!(matches!(v1, RiskVerdict::Allow));
        let v2 = eng.check_and_reserve(asset(2), Side::Buy, Size::from_usdc(10.0));
        assert!(matches!(v2, RiskVerdict::Deny(RejectReason::CorrelatedExposure)));
    }

    #[test]
    fn position_counter_respects_cap() {
        let (eng, _) = engine();
        assert!(matches!(
            eng.check_and_reserve(asset(1), Side::Buy, Size::from_usdc(10.0)),
            RiskVerdict::Allow
        ));
        assert!(matches!(
            eng.check_and_reserve(asset(2), Side::Buy, Size::from_usdc(10.0)),
            RiskVerdict::Allow
        ));
        // 3rd should be denied
        assert!(matches!(
            eng.check_and_reserve(asset(3), Side::Buy, Size::from_usdc(10.0)),
            RiskVerdict::Deny(RejectReason::TooManyPositions)
        ));
    }

    #[test]
    fn daily_loss_trips_kill_switch() {
        let (eng, _) = engine();
        eng.record_pnl(-150 * 1_000_000);
        assert!(eng.is_tripped());
    }

    #[test]
    fn release_frees_slots() {
        let (eng, _) = engine();
        eng.check_and_reserve(asset(1), Side::Buy, Size::from_usdc(10.0));
        eng.check_and_reserve(asset(2), Side::Buy, Size::from_usdc(10.0));
        eng.release(asset(1));
        assert!(matches!(
            eng.check_and_reserve(asset(3), Side::Buy, Size::from_usdc(10.0)),
            RiskVerdict::Allow
        ));
    }
}
