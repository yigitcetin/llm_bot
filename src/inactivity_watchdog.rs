//! Detects prolonged trading inactivity and emits warnings.
//!
//! Tracks the last successful trade timestamp and consecutive
//! `order_size_below_minimum` skips. When thresholds are breached, emits
//! `WARN`-level logs so operators notice the bot is stuck.

use std::time::{Duration, Instant};
use tracing::warn;

use crate::risk::RiskManager;

const INACTIVITY_WARN_SECS: u64 = 4 * 3600; // 4 hours with zero trades
const SIZE_SKIP_WARN_THRESHOLD: u32 = 30; // consecutive order_size_below_minimum skips
const BALANCE_LOG_INTERVAL_SECS: u64 = 3600; // log balance status every hour

pub struct InactivityWatchdog {
    last_trade_at: Instant,
    last_balance_log: Instant,
    last_inactivity_warn: Instant,
    consecutive_size_skips: u32,
    size_skip_warned: bool,
    total_cycles: u64,
    total_trades: u64,
    report_requested: bool,
}

impl InactivityWatchdog {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            last_trade_at: now,
            last_balance_log: now,
            last_inactivity_warn: now,
            consecutive_size_skips: 0,
            size_skip_warned: false,
            total_cycles: 0,
            total_trades: 0,
            report_requested: false,
        }
    }

    /// Call after each cycle completes. `trades_this_cycle` is the number of
    /// trades placed (reserved or executed) during this cycle.
    pub fn on_cycle_end(&mut self, trades_this_cycle: u32, risk: &RiskManager) {
        self.total_cycles += 1;

        if trades_this_cycle > 0 {
            self.last_trade_at = Instant::now();
            self.total_trades += trades_this_cycle as u64;
            self.consecutive_size_skips = 0;
            self.size_skip_warned = false;
        }

        self.check_inactivity(risk);
        self.maybe_log_balance(risk);
    }

    /// Call whenever an `order_size_below_minimum` skip occurs.
    pub fn on_order_size_skip(
        &mut self,
        balance: rust_decimal::Decimal,
        max_position_pct: rust_decimal::Decimal,
        min_order_usdc: rust_decimal::Decimal,
        dynamic_min: rust_decimal::Decimal,
    ) {
        self.consecutive_size_skips += 1;

        if self.consecutive_size_skips >= SIZE_SKIP_WARN_THRESHOLD && !self.size_skip_warned {
            self.size_skip_warned = true;
            self.report_requested = true;
            let max_order = balance * max_position_pct;
            warn!(
                consecutive_size_skips = self.consecutive_size_skips,
                balance = %balance,
                max_position_pct = %max_position_pct,
                max_order_usdc = %max_order,
                min_order_usdc = %min_order_usdc,
                dynamic_min = %dynamic_min,
                "INACTIVITY: repeated order_size_below_minimum — \
                 balance may be too low for any trade even with dynamic floor ({dynamic_min})."
            );
        }
    }

    fn check_inactivity(&mut self, risk: &RiskManager) {
        let elapsed = self.last_trade_at.elapsed();
        let warn_interval = Duration::from_secs(INACTIVITY_WARN_SECS);

        if elapsed >= warn_interval && self.last_inactivity_warn.elapsed() >= warn_interval {
            self.last_inactivity_warn = Instant::now();
            self.report_requested = true;
            let hours = elapsed.as_secs() / 3600;
            let mins = (elapsed.as_secs() % 3600) / 60;
            warn!(
                hours_since_last_trade = hours,
                minutes = mins,
                balance = %risk.available_balance(),
                total_cycles = self.total_cycles,
                total_trades = self.total_trades,
                consecutive_size_skips = self.consecutive_size_skips,
                "INACTIVITY: no trades placed for {hours}h {mins}m — generating diagnostic report"
            );
        }
    }

    /// Returns `true` once when a diagnostic report should be generated,
    /// then resets the flag until the next warning fires.
    pub fn take_report_request(&mut self) -> bool {
        if self.report_requested {
            self.report_requested = false;
            return true;
        }
        false
    }

    fn maybe_log_balance(&mut self, risk: &RiskManager) {
        if self.last_balance_log.elapsed() >= Duration::from_secs(BALANCE_LOG_INTERVAL_SECS) {
            self.last_balance_log = Instant::now();
            tracing::info!(
                balance = %risk.available_balance(),
                total_cycles = self.total_cycles,
                total_trades = self.total_trades,
                "periodic balance status"
            );
        }
    }

    /// Reset the consecutive size-skip counter (e.g. when a non-size skip occurs).
    pub fn reset_size_skips(&mut self) {
        if self.consecutive_size_skips > 0 {
            self.consecutive_size_skips = 0;
            self.size_skip_warned = false;
        }
    }
}
