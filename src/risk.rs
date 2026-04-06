use rust_decimal::Decimal;
use tracing::warn;
use std::collections::HashMap;
use std::fmt;

use crate::config::AppConfig;
use crate::types::OpenPosition;

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
    open_positions: HashMap<String, OpenPosition>,
    last_reset: chrono::NaiveDate,
}

impl RiskManager {
    pub fn new(cfg: &AppConfig) -> Self {
        Self {
            balance: cfg.initial_balance,
            daily_loss_limit: cfg.initial_balance * cfg.daily_loss_limit_pct,
            daily_loss: Decimal::ZERO,
            open_positions: HashMap::new(),
            last_reset: chrono::Utc::now().date_naive(),
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

    /// Call when an order is placed.
    pub fn record_trade(&mut self, size_usdc: Decimal, position: OpenPosition) {
        self.balance -= size_usdc;
        self.open_positions.insert(position.condition_id.clone(), position);
    }

    /// Call when a position resolves.
    pub fn record_resolution(&mut self, pos: &OpenPosition, pnl: Decimal) {
        self.open_positions.remove(&pos.condition_id);

        // Stake returned + PnL (negative PnL = loss).
        self.balance += pos.size_usdc + pnl;

        if pnl < Decimal::ZERO {
            self.daily_loss += pnl.abs();
        }

        tracing::info!(
            condition_id = %pos.condition_id,
            pnl = %pnl,
            balance = %self.balance,
            daily_loss = %self.daily_loss,
            "position resolved"
        );
    }

    fn maybe_reset_daily(&mut self) {
        let today = chrono::Utc::now().date_naive();
        if today > self.last_reset {
            self.daily_loss = Decimal::ZERO;
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
        let mut rm = RiskManager::new(&test_cfg());
        assert!(rm.can_trade(dec!(5), "cid1", dec!(0.05)));
    }

    #[test]
    fn cannot_trade_twice_same_market() {
        let mut rm = RiskManager::new(&test_cfg());
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
        let mut rm = RiskManager::new(&test_cfg());
        assert!(!rm.can_trade(dec!(20), "cid1", dec!(0.05)));
    }

    #[test]
    fn daily_loss_limit_halts_trading() {
        let mut rm = RiskManager::new(&test_cfg());
        rm.daily_loss = dec!(20);
        assert!(!rm.can_trade(dec!(5), "cid1", dec!(0.05)));
    }
}
