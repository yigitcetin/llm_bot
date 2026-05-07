use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use anyhow::Context;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use rust_decimal::prelude::ToPrimitive;

use crate::config::AppConfig;
use crate::fs_atomic;
use crate::types::{Direction, OpenPosition};

/// Persisted balance snapshot written to `data/balance_state.json`.
#[derive(Debug, Serialize, Deserialize)]
struct BalanceState {
    balance: Decimal,
    updated_at: chrono::DateTime<chrono::Utc>,
    /// `AppConfig.strategy_version` when this snapshot was written (`None` in legacy files).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    strategy_version: Option<String>,
    /// Peak balance observed for drawdown (`None` in legacy files — treated as `balance` on load).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    peak_balance: Option<Decimal>,
    /// Approximate drawdown from peak: `max(0, 1 - balance/peak)` as fraction (0..=1).
    #[serde(default)]
    current_drawdown_pct: f64,
    #[serde(default)]
    open_position_count: usize,
    #[serde(default)]
    total_exposure_usdc: Decimal,
    /// Sum of realized PnL from closed trades for `daily_counters_utc_date` (UTC calendar day).
    #[serde(default)]
    daily_realized_pnl: Decimal,
    /// Resolved (closed) trades counted for `daily_counters_utc_date`.
    #[serde(default)]
    resolved_trades_today: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_trade_resolution_at: Option<chrono::DateTime<chrono::Utc>>,
    /// UTC date for which `daily_realized_pnl` / `resolved_trades_today` are valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    daily_counters_utc_date: Option<chrono::NaiveDate>,
}

#[derive(Debug, Serialize)]
struct OpenPositionsFile {
    updated_at: chrono::DateTime<chrono::Utc>,
    strategy_version: String,
    count: usize,
    total_exposure_usdc: Decimal,
    positions: Vec<OpenPositionRow>,
}

#[derive(Debug, Serialize)]
struct OpenPositionRow {
    condition_id: String,
    order_id: String,
    direction: String,
    entry_price: Decimal,
    size_usdc: Decimal,
    size_shares: Decimal,
    end_date_ms: i64,
}

const BALANCE_STATE_FILE: &str = "balance_state.json";
const OPEN_POSITIONS_FILE: &str = "open_positions.json";

/// Why [`RiskManager`] refused a new trade (for skip logging / metrics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeBlockReason {
    DailyLossLimit,
    DuplicateMarket,
    PositionSizeExceedsMax,
    InsufficientBalance,
}

impl fmt::Display for TradeBlockReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DailyLossLimit => write!(f, "daily_loss_limit"),
            Self::DuplicateMarket => write!(f, "duplicate_market_position"),
            Self::PositionSizeExceedsMax => write!(f, "position_size_exceeds_max"),
            Self::InsufficientBalance => write!(f, "insufficient_balance"),
        }
    }
}

pub struct RiskManager {
    balance: Decimal,
    daily_loss_limit: Decimal,
    daily_loss: Decimal,
    /// Highest balance observed this process / restored from disk (for drawdown).
    peak_balance: Decimal,
    /// Realized PnL from resolutions today (UTC day, see `last_reset`).
    daily_realized_pnl: Decimal,
    /// Count of resolved trades today (UTC).
    resolved_trades_today: u64,
    last_trade_resolution_at: Option<chrono::DateTime<chrono::Utc>>,
    open_positions: HashMap<String, OpenPosition>,
    /// USDC reserved for GTD limit orders not yet confirmed filled (`condition_id` -> amount).
    reserved_usdc: HashMap<String, Decimal>,
    /// Markets with a resting order (reserved) — blocks duplicate signals until fill/cancel.
    reserved_markets: HashSet<String>,
    last_reset: chrono::NaiveDate,
    /// Directory for `balance_state.json`; `None` disables persistence (e.g. tests).
    data_dir: Option<PathBuf>,
    strategy_version: String,
}

/// [`data/balance_state.json`] if valid, otherwise [`AppConfig::initial_balance`].
#[must_use]
pub fn persisted_or_config_balance(cfg: &AppConfig) -> Decimal {
    let data_dir = PathBuf::from(&cfg.data_dir);
    load_balance_state(&data_dir).unwrap_or(cfg.initial_balance)
}

impl RiskManager {
    pub fn new(cfg: &AppConfig, starting_balance: Decimal) -> Self {
        let data_dir = PathBuf::from(&cfg.data_dir);

        if starting_balance != cfg.initial_balance {
            info!(
                starting_balance = %starting_balance,
                config_initial = %cfg.initial_balance,
                "starting balance (persisted file, CLOB sync, or explicit override)"
            );
        }

        let today = chrono::Utc::now().date_naive();
        let persisted = read_balance_state_full(&data_dir);
        let (peak_balance, daily_realized_pnl, resolved_today, last_res, last_reset) =
            if let Some(s) = persisted {
                let peak = s
                    .peak_balance
                    .unwrap_or(s.balance)
                    .max(starting_balance);
                let counter_date = s.daily_counters_utc_date.unwrap_or(today);
                if counter_date == today {
                    (
                        peak,
                        s.daily_realized_pnl,
                        s.resolved_trades_today,
                        s.last_trade_resolution_at,
                        today,
                    )
                } else {
                    (
                        peak,
                        Decimal::ZERO,
                        0,
                        s.last_trade_resolution_at,
                        counter_date,
                    )
                }
            } else {
                (
                    starting_balance,
                    Decimal::ZERO,
                    0,
                    None,
                    today,
                )
            };

        let mut rm = Self {
            balance: starting_balance,
            daily_loss_limit: starting_balance * cfg.daily_loss_limit_pct,
            daily_loss: Decimal::ZERO,
            peak_balance,
            daily_realized_pnl,
            resolved_trades_today: resolved_today,
            last_trade_resolution_at: last_res,
            open_positions: HashMap::new(),
            reserved_usdc: HashMap::new(),
            reserved_markets: HashSet::new(),
            last_reset,
            data_dir: Some(data_dir),
            strategy_version: cfg.strategy_version.trim().to_string(),
        };
        rm.maybe_reset_daily();
        rm.persist_disk_state();
        rm
    }

    /// Test-only constructor that skips balance persistence.
    #[cfg(test)]
    pub(crate) fn new_without_persistence(cfg: &AppConfig) -> Self {
        let today = chrono::Utc::now().date_naive();
        Self {
            balance: cfg.initial_balance,
            daily_loss_limit: cfg.initial_balance * cfg.daily_loss_limit_pct,
            daily_loss: Decimal::ZERO,
            peak_balance: cfg.initial_balance,
            daily_realized_pnl: Decimal::ZERO,
            resolved_trades_today: 0,
            last_trade_resolution_at: None,
            open_positions: HashMap::new(),
            reserved_usdc: HashMap::new(),
            reserved_markets: HashSet::new(),
            last_reset: today,
            data_dir: None,
            strategy_version: cfg.strategy_version.trim().to_string(),
        }
    }

    /// Persist `balance_state.json` and `open_positions.json`.
    fn persist_disk_state(&mut self) {
        let Some(dir) = self.data_dir.clone() else { return };
        self.maybe_reset_daily();
        self.peak_balance = self.peak_balance.max(self.balance);
        let drawdown_pct = drawdown_fraction(self.balance, self.peak_balance);
        let exposure: Decimal = self
            .open_positions
            .values()
            .map(|p| p.size_usdc)
            .sum();
        if let Err(e) = save_balance_state(
            &dir,
            self.balance,
            &self.strategy_version,
            self.peak_balance,
            drawdown_pct,
            self.open_positions.len(),
            exposure,
            self.daily_realized_pnl,
            self.resolved_trades_today,
            self.last_trade_resolution_at,
            self.last_reset,
        ) {
            warn!(error = %e, "failed to persist balance state");
        }
        if let Err(e) = save_open_positions(
            &dir,
            &self.strategy_version,
            &self.open_positions,
        ) {
            warn!(error = %e, "failed to persist open positions");
        }
    }

    /// If a trade is blocked, returns the reason (for skip `details`).
    pub fn trade_block_reason(
        &mut self,
        size_usdc: Decimal,
        condition_id: &str,
        max_position_pct: Decimal,
    ) -> Option<TradeBlockReason> {
        self.maybe_reset_daily();

        if self.daily_loss >= self.daily_loss_limit {
            warn!(
                daily_loss = %self.daily_loss,
                limit = %self.daily_loss_limit,
                "daily loss limit reached — halting"
            );
            return Some(TradeBlockReason::DailyLossLimit);
        }

        if self.open_positions.contains_key(condition_id) {
            return Some(TradeBlockReason::DuplicateMarket);
        }

        if self.reserved_markets.contains(condition_id) {
            return Some(TradeBlockReason::DuplicateMarket);
        }

        let max_size = self.balance * max_position_pct;
        if size_usdc > max_size {
            warn!(
                size = %size_usdc,
                max = %max_size,
                "position size exceeds limit"
            );
            return Some(TradeBlockReason::PositionSizeExceedsMax);
        }

        if size_usdc > self.balance {
            warn!("insufficient balance");
            return Some(TradeBlockReason::InsufficientBalance);
        }

        None
    }

    /// Check if we can open a new trade (`max_position_pct` from [`crate::config::AssetStrategy`] per asset).
    pub fn can_trade(
        &mut self,
        size_usdc: Decimal,
        condition_id: &str,
        max_position_pct: Decimal,
    ) -> bool {
        self.trade_block_reason(size_usdc, condition_id, max_position_pct)
            .is_none()
    }

    pub fn available_balance(&self) -> Decimal {
        self.balance
    }

    /// Cumulative loss today (absolute USDC), reset at day boundary.
    pub fn daily_loss(&self) -> Decimal {
        self.daily_loss
    }

    /// Reserve `size_usdc` for a resting GTD order (balance decreases; no [`OpenPosition`] yet).
    pub fn reserve_for_order(&mut self, condition_id: &str, size_usdc: Decimal) {
        self.balance -= size_usdc;
        self.reserved_usdc
            .insert(condition_id.to_string(), size_usdc);
        self.reserved_markets.insert(condition_id.to_string());
        self.persist_disk_state();
    }

    /// Release reservation when the order is cancelled/expired without a confirmed position.
    pub fn release_reservation(&mut self, condition_id: &str) {
        if let Some(amt) = self.reserved_usdc.remove(condition_id) {
            self.balance += amt;
            self.persist_disk_state();
        }
        self.reserved_markets.remove(condition_id);
    }

    /// Move reserved USDC into a confirmed [`OpenPosition`] (no net balance change).
    pub fn confirm_reserved_trade(&mut self, condition_id: &str, position: OpenPosition) {
        self.reserved_usdc.remove(condition_id);
        self.reserved_markets.remove(condition_id);
        self.open_positions
            .insert(position.condition_id.clone(), position);
        self.persist_disk_state();
    }

    /// Call when an order is placed and filled immediately (no prior reservation), or dry-run.
    pub fn record_trade(&mut self, size_usdc: Decimal, position: OpenPosition) {
        self.balance -= size_usdc;
        self.open_positions
            .insert(position.condition_id.clone(), position);
        self.persist_disk_state();
    }

    /// True if we have an open position or reserved resting order for this market.
    pub fn has_open_or_reserved(&self, condition_id: &str) -> bool {
        self.open_positions.contains_key(condition_id)
            || self.reserved_markets.contains(condition_id)
    }

    /// Call when a position resolves.
    pub fn record_resolution(&mut self, pos: &OpenPosition, pnl: Decimal) {
        self.open_positions.remove(&pos.condition_id);

        self.maybe_reset_daily();

        // Stake returned + PnL (negative PnL = loss).
        self.balance += pos.size_usdc + pnl;

        if pnl < Decimal::ZERO {
            self.daily_loss += pnl.abs();
        }

        self.daily_realized_pnl += pnl;
        self.resolved_trades_today = self.resolved_trades_today.saturating_add(1);
        self.last_trade_resolution_at = Some(chrono::Utc::now());
        self.persist_disk_state();

        tracing::info!(
            condition_id = %pos.condition_id,
            pnl = %pnl,
            balance = %self.balance,
            daily_loss = %self.daily_loss,
            "position resolved"
        );
    }

    /// Credit balance for a trade resolved from `trades.jsonl` that has no
    /// in-memory position (e.g. the bot restarted and the position was lost).
    /// The persisted balance already had the stake deducted, so we credit
    /// stake + PnL back.
    pub fn credit_file_resolution(&mut self, size_usdc: Decimal, pnl: Decimal) {
        self.maybe_reset_daily();

        self.balance += size_usdc + pnl;

        if pnl < Decimal::ZERO {
            self.daily_loss += pnl.abs();
        }

        self.daily_realized_pnl += pnl;
        self.resolved_trades_today = self.resolved_trades_today.saturating_add(1);
        self.last_trade_resolution_at = Some(chrono::Utc::now());
        self.persist_disk_state();

        tracing::info!(
            size_usdc = %size_usdc,
            pnl = %pnl,
            balance = %self.balance,
            "file-based resolution credited to balance"
        );
    }

    fn maybe_reset_daily(&mut self) {
        let today = chrono::Utc::now().date_naive();
        if today > self.last_reset {
            self.daily_loss = Decimal::ZERO;
            self.daily_realized_pnl = Decimal::ZERO;
            self.resolved_trades_today = 0;
            self.last_reset = today;
            tracing::info!("daily loss counter reset");
        }
    }

    pub fn has_position(&self, condition_id: &str) -> bool {
        self.open_positions.contains_key(condition_id)
    }

    pub fn open_positions_detail(&self) -> Vec<OpenPosition> {
        self.open_positions.values().cloned().collect()
    }
}

fn balance_state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(BALANCE_STATE_FILE)
}

fn open_positions_path(data_dir: &Path) -> PathBuf {
    data_dir.join(OPEN_POSITIONS_FILE)
}

fn read_balance_state_full(data_dir: &Path) -> Option<BalanceState> {
    let path = balance_state_path(data_dir);
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str::<BalanceState>(&content)
        .map_err(|e| warn!(error = %e, "corrupt balance_state.json, ignoring extended fields"))
        .ok()
}

fn load_balance_state(data_dir: &Path) -> Option<Decimal> {
    read_balance_state_full(data_dir).map(|s| s.balance)
}

fn drawdown_fraction(balance: Decimal, peak: Decimal) -> f64 {
    if peak <= Decimal::ZERO {
        return 0.0;
    }
    let one = Decimal::ONE;
    let raw = one - (balance / peak);
    raw.max(Decimal::ZERO).to_f64().unwrap_or(0.0)
}

fn save_balance_state(
    data_dir: &Path,
    balance: Decimal,
    strategy_version: &str,
    peak_balance: Decimal,
    current_drawdown_pct: f64,
    open_position_count: usize,
    total_exposure_usdc: Decimal,
    daily_realized_pnl: Decimal,
    resolved_trades_today: u64,
    last_trade_resolution_at: Option<chrono::DateTime<chrono::Utc>>,
    daily_counters_utc_date: chrono::NaiveDate,
) -> anyhow::Result<()> {
    let state = BalanceState {
        balance,
        updated_at: chrono::Utc::now(),
        strategy_version: Some(strategy_version.trim().to_string()),
        peak_balance: Some(peak_balance),
        current_drawdown_pct,
        open_position_count,
        total_exposure_usdc,
        daily_realized_pnl,
        resolved_trades_today,
        last_trade_resolution_at,
        daily_counters_utc_date: Some(daily_counters_utc_date),
    };
    let json = serde_json::to_string_pretty(&state)?;
    let path = balance_state_path(data_dir);
    fs_atomic::write_path_atomic(&path, json.as_bytes())
        .context("atomic write balance_state.json")?;
    Ok(())
}

fn save_open_positions(
    data_dir: &Path,
    strategy_version: &str,
    open_positions: &HashMap<String, OpenPosition>,
) -> anyhow::Result<()> {
    let exposure: Decimal = open_positions.values().map(|p| p.size_usdc).sum();
    let positions: Vec<OpenPositionRow> = open_positions
        .values()
        .map(|p| OpenPositionRow {
            condition_id: p.condition_id.clone(),
            order_id: p.order_id.clone(),
            direction: match p.direction {
                Direction::Yes => "YES".to_string(),
                Direction::No => "NO".to_string(),
            },
            entry_price: p.entry_price,
            size_usdc: p.size_usdc,
            size_shares: p.size_shares,
            end_date_ms: p.end_date_ms,
        })
        .collect();

    let file = OpenPositionsFile {
        updated_at: chrono::Utc::now(),
        strategy_version: strategy_version.trim().to_string(),
        count: positions.len(),
        total_exposure_usdc: exposure,
        positions,
    };
    let json = serde_json::to_string_pretty(&file)?;
    let path = open_positions_path(data_dir);
    fs_atomic::write_path_atomic(&path, json.as_bytes())
        .context("atomic write open_positions.json")?;
    Ok(())
}

#[cfg(test)]
impl RiskManager {
    /// Set calendar day for daily loss reset tests (`maybe_reset_daily`).
    pub(super) fn set_last_reset_for_test(&mut self, d: chrono::NaiveDate) {
        self.last_reset = d;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::types::Direction;
    use rust_decimal_macros::dec;

    fn test_cfg() -> AppConfig {
        let mut c = AppConfig::default();
        c.polymarket_private_key = "test".to_string();
        c
    }

    #[test]
    fn can_trade_normal() {
        let mut rm = RiskManager::new_without_persistence(&test_cfg());
        assert!(rm.can_trade(dec!(5), "cid1", dec!(0.05)));
    }

    #[test]
    fn cannot_trade_twice_same_market() {
        let mut rm = RiskManager::new_without_persistence(&test_cfg());
        rm.record_trade(
            dec!(5),
            OpenPosition {
                condition_id: "cid1".to_string(),
                order_id: "order-1".to_string(),
                direction: Direction::Yes,
                entry_price: dec!(0.5),
                size_usdc: dec!(5),
                size_shares: dec!(10),
                end_date_ms: 0,
            },
        );
        assert!(!rm.can_trade(dec!(5), "cid1", dec!(0.05)));
    }

    #[test]
    fn cannot_trade_over_position_limit() {
        let mut rm = RiskManager::new_without_persistence(&test_cfg());
        assert!(!rm.can_trade(dec!(20), "cid1", dec!(0.05)));
    }

    #[test]
    fn daily_loss_limit_halts_trading() {
        let mut rm = RiskManager::new_without_persistence(&test_cfg());
        rm.daily_loss = dec!(20);
        assert!(!rm.can_trade(dec!(5), "cid1", dec!(0.05)));
    }

    #[test]
    fn record_resolution_loss_increments_daily_loss() {
        let mut rm = RiskManager::new_without_persistence(&test_cfg());
        let pos = OpenPosition {
            condition_id: "cid1".to_string(),
            order_id: "o1".to_string(),
            direction: Direction::Yes,
            entry_price: dec!(0.5),
            size_usdc: dec!(10),
            size_shares: dec!(20),
            end_date_ms: 0,
        };
        rm.record_trade(dec!(10), pos.clone());
        rm.record_resolution(&pos, dec!(-3));
        assert_eq!(rm.daily_loss(), dec!(3));
    }

    #[test]
    fn daily_loss_resets_on_new_calendar_day() {
        let mut rm = RiskManager::new_without_persistence(&test_cfg());
        rm.daily_loss = dec!(15);
        rm.set_last_reset_for_test(
            chrono::Utc::now()
                .date_naive()
                .pred_opt()
                .expect("valid yesterday"),
        );
        assert_eq!(rm.daily_loss(), dec!(15));
        assert!(rm.can_trade(dec!(5), "cid_new", dec!(0.05)));
        assert_eq!(rm.daily_loss(), dec!(0));
    }

    #[test]
    fn reserve_for_order_decrements_balance_and_blocks_duplicate() {
        let mut rm = RiskManager::new_without_persistence(&test_cfg());
        let start = rm.available_balance();
        rm.reserve_for_order("cid1", dec!(30));
        assert_eq!(rm.available_balance(), start - dec!(30));
        assert!(rm.has_open_or_reserved("cid1"));
        assert_eq!(
            rm.trade_block_reason(dec!(5), "cid1", dec!(0.05)),
            Some(TradeBlockReason::DuplicateMarket)
        );
        assert!(!rm.can_trade(dec!(5), "cid1", dec!(0.05)));
    }

    #[test]
    fn release_reservation_restores_balance() {
        let mut rm = RiskManager::new_without_persistence(&test_cfg());
        let start = rm.available_balance();
        rm.reserve_for_order("cid1", dec!(25));
        assert_eq!(rm.available_balance(), start - dec!(25));
        rm.release_reservation("cid1");
        assert_eq!(rm.available_balance(), start);
        assert!(!rm.has_open_or_reserved("cid1"));
    }

    #[test]
    fn confirm_reserved_trade_opens_position_without_balance_change() {
        let mut rm = RiskManager::new_without_persistence(&test_cfg());
        let start = rm.available_balance();
        rm.reserve_for_order("cid1", dec!(10));
        assert_eq!(rm.available_balance(), start - dec!(10));

        let pos = OpenPosition {
            condition_id: "cid1".to_string(),
            order_id: "ord1".to_string(),
            direction: Direction::Yes,
            entry_price: dec!(0.5),
            size_usdc: dec!(10),
            size_shares: dec!(20),
            end_date_ms: 0,
        };
        rm.confirm_reserved_trade("cid1", pos);
        assert_eq!(rm.available_balance(), start - dec!(10));
        assert!(rm.has_position("cid1"));
        assert!(rm.has_open_or_reserved("cid1"));
    }

    #[test]
    fn cannot_trade_same_market_after_reserve() {
        let mut rm = RiskManager::new_without_persistence(&test_cfg());
        rm.reserve_for_order("cid1", dec!(5));
        assert_eq!(
            rm.trade_block_reason(dec!(5), "cid1", dec!(0.05)),
            Some(TradeBlockReason::DuplicateMarket)
        );
    }

    #[test]
    fn balance_persistence_round_trip() {
        let dir = std::env::temp_dir().join(format!("risk_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let balance = dec!(548.94);
        let today = chrono::Utc::now().date_naive();
        save_balance_state(
            &dir,
            balance,
            "1",
            balance,
            0.0,
            0,
            dec!(0),
            dec!(0),
            0,
            None,
            today,
        )
        .expect("save");
        let loaded = load_balance_state(&dir).expect("load");
        assert_eq!(loaded, balance);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_balance_file_returns_none() {
        let dir = std::env::temp_dir().join(format!("risk_test_miss_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert!(load_balance_state(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persisted_or_config_balance_prefers_file() {
        let dir = std::env::temp_dir().join(format!("risk_persist_cfg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let b = dec!(333.12);
        let today = chrono::Utc::now().date_naive();
        save_balance_state(&dir, b, "1", b, 0.0, 0, dec!(0), dec!(0), 0, None, today)
            .expect("save");

        let mut cfg = test_cfg();
        cfg.data_dir = dir.to_string_lossy().into_owned();
        cfg.initial_balance = dec!(200);

        assert_eq!(persisted_or_config_balance(&cfg), dec!(333.12));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persisted_or_config_balance_falls_back_to_initial() {
        let dir = std::env::temp_dir().join(format!("risk_persist_fallback_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let mut cfg = test_cfg();
        cfg.data_dir = dir.to_string_lossy().into_owned();
        cfg.initial_balance = dec!(412);

        assert_eq!(persisted_or_config_balance(&cfg), dec!(412));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
