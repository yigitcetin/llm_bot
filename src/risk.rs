use rust_decimal::Decimal;
use std::collections::HashSet;
use tracing::warn;

use crate::config::AppConfig;

pub struct RiskManager {
    balance: Decimal,
    daily_loss_limit: Decimal,
    daily_loss: Decimal,
    open_positions: HashSet<String>,
    last_reset: chrono::NaiveDate,
}

impl RiskManager {
    pub fn new(cfg: &AppConfig) -> Self {
        Self {
            balance: cfg.initial_balance,
            daily_loss_limit: cfg.initial_balance * cfg.daily_loss_limit_pct,
            daily_loss: Decimal::ZERO,
            open_positions: HashSet::new(),
            last_reset: chrono::Utc::now().date_naive(),
        }
    }

    /// Check if we can open a new trade (`max_position_pct` from [`crate::config::AssetStrategy`] per asset).
    pub fn can_trade(
        &mut self,
        size_usdc: Decimal,
        condition_id: &str,
        max_position_pct: Decimal,
    ) -> bool {
        self.maybe_reset_daily();

        if self.daily_loss >= self.daily_loss_limit {
            warn!(
                daily_loss = %self.daily_loss,
                limit = %self.daily_loss_limit,
                "daily loss limit reached — halting"
            );
            return false;
        }

        if self.open_positions.contains(condition_id) {
            return false;
        }

        let max_size = self.balance * max_position_pct;
        if size_usdc > max_size {
            warn!(
                size = %size_usdc,
                max = %max_size,
                "position size exceeds limit"
            );
            return false;
        }

        if size_usdc > self.balance {
            warn!("insufficient balance");
            return false;
        }

        true
    }

    pub fn has_position(&self, condition_id: &str) -> bool {
        self.open_positions.contains(condition_id)
    }

    pub fn available_balance(&self) -> Decimal {
        self.balance
    }

    /// Call when an order is placed.
    pub fn record_trade(&mut self, size_usdc: Decimal, condition_id: String) {
        self.balance -= size_usdc;
        self.open_positions.insert(condition_id);
    }

    /// Call when a position resolves.
    pub fn record_resolution(
        &mut self,
        condition_id: &str,
        pnl: Decimal, // positive = win, negative = loss
    ) {
        self.open_positions.remove(condition_id);
        self.balance += pnl;
        if pnl < Decimal::ZERO {
            self.daily_loss += pnl.abs();
        }
        tracing::info!(
            condition_id = %condition_id,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, SignatureType};
    use rust_decimal_macros::dec;

    fn test_cfg() -> AppConfig {
        AppConfig {
            polymarket_private_key: "test".to_string(),
            assets: vec!["btc".to_string()],
            durations: vec!["5m".to_string()],
            min_edge: dec!(0.06),
            min_confidence: dec!(0.70),
            min_order_usdc: dec!(5),
            spot_exchange: "binance".to_string(),
            candle_interval: "1m".to_string(),
            candle_lookback: 100,
            rsi_period: 14,
            macd_fast: 12,
            macd_slow: 26,
            macd_signal: 9,
            volume_min_ratio: None,
            volume_avg_bars: 20,
            max_position_pct: dec!(0.05),
            daily_loss_limit_pct: dec!(0.10),
            initial_balance: dec!(200),
            dry_run: true,
            cycle_secs: 60,
            gamma_tag_id: crate::constants::GAMMA_TAG_ID_DEFAULT,
            clob_host: "https://clob.polymarket.com".to_string(),
            chain_id: 137,
            signature_type: SignatureType::Eoa,
            funder_address: None,
            builder_api_key: None,
            builder_api_secret: None,
            builder_api_passphrase: None,
        }
    }

    #[test]
    fn can_trade_normal() {
        let mut rm = RiskManager::new(&test_cfg());
        assert!(rm.can_trade(dec!(5), "cid1", dec!(0.05)));
    }

    #[test]
    fn cannot_trade_twice_same_market() {
        let mut rm = RiskManager::new(&test_cfg());
        rm.record_trade(dec!(5), "cid1".to_string());
        assert!(!rm.can_trade(dec!(5), "cid1", dec!(0.05)));
    }

    #[test]
    fn cannot_trade_over_position_limit() {
        let mut rm = RiskManager::new(&test_cfg());
        // 5% of 200 = $10 max
        assert!(!rm.can_trade(dec!(20), "cid1", dec!(0.05)));
    }

    #[test]
    fn daily_loss_limit_halts_trading() {
        let mut rm = RiskManager::new(&test_cfg());
        // Simulate $20 daily loss (10% of $200 = $20 limit)
        rm.daily_loss = dec!(20);
        assert!(!rm.can_trade(dec!(5), "cid1", dec!(0.05)));
    }
}
