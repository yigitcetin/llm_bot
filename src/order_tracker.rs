//! Pending GTD orders: WebSocket + REST poll reconciliation, fill confirmation, cancel on timeout.

use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::execution::{Executor, OrderPollResult, PlaceOrderOutcome};
use crate::fill_tracker::{FillResult, FillTracker};
use crate::metrics::{MetricsLogger, TradeRecord};
use crate::prometheus_export;
use crate::risk::RiskManager;
use crate::types::{Direction, OpenPosition};
use polymarket_client_sdk::clob::types::OrderStatusType;

/// Minimum Polymarket shares (same as execution).
const MIN_SHARES: Decimal = dec!(5);

/// Telemetry captured at signal time; used to build [`TradeRecord`] on fill confirmation.
#[derive(Debug, Clone)]
pub struct PendingTradeMeta {
    pub asset: String,
    pub duration: String,
    pub direction: Direction,
    pub limit_price: Decimal,
    pub size_usdc: Decimal,
    pub size_shares: Decimal,
    pub signal_probability: Decimal,
    pub confidence: Decimal,
    pub edge: Decimal,
    pub reasoning: String,
    pub rsi: Option<f64>,
    pub macd_histogram: Option<f64>,
    pub volume_ratio: Option<f64>,
    pub cluster_direction: Option<String>,
    pub market_yes_price: Option<String>,
    pub liquidity: Option<String>,
    pub secs_to_close: Option<i64>,
    pub volatility_std_pct: Option<f64>,
    pub kelly_fraction: Option<String>,
    pub balance_at_signal: String,
    pub daily_loss_at_signal: String,
    pub htf_aligned: Option<bool>,
    pub adaptive_min_edge: Option<String>,
    pub adaptive_min_confidence: Option<String>,
    pub sizing_cap_hit: Option<String>,
    pub momentum_5m: Option<f64>,
    pub momentum_15m: Option<f64>,
    pub taker_buy_ratio: Option<f64>,
    pub macd_line: Option<f64>,
    pub macd_signal_line: Option<f64>,
    pub question: Option<String>,
    pub slippage_bps: Option<String>,
    pub effective_min_edge: Option<String>,
}

/// One resting GTD order we are tracking until fill, cancel, or timeout.
#[derive(Debug, Clone)]
pub struct PendingOrder {
    pub order_id: String,
    pub condition_id: String,
    pub end_date_ms: i64,
    pub placed_at_ms: i64,
    pub original_shares: Decimal,
    /// Sum of incremental fill deltas from [`FillResult`] (should match fill_tracker for this order).
    pub cumulative_filled_shares: Decimal,
    pub meta: PendingTradeMeta,
}

pub struct OrderTracker {
    pending: HashMap<String, PendingOrder>,
    fill_tracker: FillTracker,
}

impl OrderTracker {
    pub fn new(fill_tracker: FillTracker) -> Self {
        Self {
            pending: HashMap::new(),
            fill_tracker,
        }
    }

    pub fn fill_tracker(&self) -> FillTracker {
        self.fill_tracker.clone()
    }

    /// Condition IDs (as hex `0x…`) for user WebSocket subscription.
    pub fn ws_condition_ids(&self) -> Vec<String> {
        self.pending
            .values()
            .map(|p| p.condition_id.clone())
            .collect()
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Register a resting order after `place_order` returned `Live`.
    pub fn add_pending(&mut self, order: PendingOrder) {
        self.pending.insert(order.order_id.clone(), order);
    }

    /// Non-blocking: drain all available fill events from the user WS channel.
    pub fn process_fill_channel(
        &mut self,
        rx: &mut mpsc::UnboundedReceiver<FillResult>,
        risk: &mut RiskManager,
        logger: &MetricsLogger,
    ) -> Result<()> {
        while let Ok(fill) = rx.try_recv() {
            self.apply_fill_delta(&fill, risk, logger)?;
        }
        Ok(())
    }

    fn apply_fill_delta(
        &mut self,
        fill: &FillResult,
        risk: &mut RiskManager,
        logger: &MetricsLogger,
    ) -> Result<()> {
        let order_id = fill.order_id.clone();
        let (is_full, cum_for_log, orig) = {
            let Some(p) = self.pending.get_mut(&order_id) else {
                return Ok(());
            };
            p.cumulative_filled_shares += fill.size_matched;
            let cum = p
                .cumulative_filled_shares
                .max(self.fill_tracker.get_seen(&order_id));
            (cum >= p.original_shares, cum, p.original_shares)
        };

        if is_full {
            let done = self
                .pending
                .remove(&order_id)
                .expect("pending order must exist");
            self.confirm_full_fill(done, risk, logger)?;
        } else if cum_for_log > Decimal::ZERO {
            info!(
                order_id = %order_id,
                cumulative = %cum_for_log,
                original = %orig,
                "partial fill (WS)"
            );
        }
        Ok(())
    }

    fn confirm_full_fill(
        &mut self,
        p: PendingOrder,
        risk: &mut RiskManager,
        logger: &MetricsLogger,
    ) -> Result<()> {
        let record = build_trade_record(&p, "filled");
        let _ = logger.log_trade(&record);

        if !record.order_id.starts_with("dry-run-") {
            prometheus_export::record_trade_success();
        }

        let position = OpenPosition {
            condition_id: p.condition_id.clone(),
            order_id: p.order_id.clone(),
            direction: p.meta.direction,
            entry_price: p.meta.limit_price,
            size_usdc: p.meta.size_usdc,
            size_shares: p.meta.size_shares,
            end_date_ms: p.end_date_ms,
        };
        risk.confirm_reserved_trade(&p.condition_id, position);
        info!(
            order_id = %p.order_id,
            condition_id = %p.condition_id,
            "order fully filled — position opened"
        );
        Ok(())
    }

    /// REST poll fallback + timeout cancel. Call after each cycle sleep.
    pub async fn poll_and_reconcile(
        &mut self,
        executor: &Executor,
        risk: &mut RiskManager,
        logger: &MetricsLogger,
        fill_timeout_secs: u64,
        poll_min_order_age_secs: u64,
    ) -> Result<()> {
        if executor.is_dry_run() || self.pending.is_empty() {
            return Ok(());
        }

        let now_ms = Utc::now().timestamp_millis();
        let snapshots: Vec<(String, i64, Decimal, String)> = self
            .pending
            .iter()
            .map(|(oid, p)| {
                (
                    oid.clone(),
                    p.placed_at_ms,
                    p.original_shares,
                    p.condition_id.clone(),
                )
            })
            .collect();

        for (oid, placed_at_ms, original_shares, _cid) in snapshots {
            if self.pending.get(&oid).is_none() {
                continue;
            }

            if now_ms - placed_at_ms < (poll_min_order_age_secs as i64) * 1000 {
                continue;
            }

            let polled = match executor.poll_order(&oid).await {
                Ok(x) => x,
                Err(e) => {
                    warn!(order_id = %oid, error = %e, "poll_order failed");
                    continue;
                }
            };

            self.fill_tracker.mark_seen(&oid, polled.size_matched);

            // Fully matched via REST (WS may have missed).
            if matches!(polled.status, OrderStatusType::Matched)
                || (polled.size_matched >= original_shares && original_shares > Decimal::ZERO)
            {
                let done = self.pending.remove(&oid).expect("key exists");
                self.confirm_full_fill(done, risk, logger)?;
                continue;
            }

            if matches!(
                polled.status,
                OrderStatusType::Canceled | OrderStatusType::Unmatched
            ) {
                self.handle_terminal_not_filled(&oid, &polled, risk, logger)?;
                continue;
            }

            // Live: check fill timeout
            let age_secs = (now_ms - placed_at_ms) / 1000;
            if age_secs >= fill_timeout_secs as i64 {
                let _ = executor.cancel_order(&oid).await;
                let polled_after = executor.poll_order(&oid).await.ok();
                if let Some(ref po) = polled_after {
                    self.fill_tracker.mark_seen(&oid, po.size_matched);
                    if po.size_matched >= original_shares * dec!(0.99) {
                        let done = self.pending.remove(&oid).expect("key exists");
                        self.confirm_full_fill(done, risk, logger)?;
                        continue;
                    }
                }
                self.handle_timeout_partial(&oid, polled_after.as_ref(), risk, logger)?;
            }
        }

        Ok(())
    }

    fn handle_terminal_not_filled(
        &mut self,
        order_id: &str,
        polled: &OrderPollResult,
        risk: &mut RiskManager,
        logger: &MetricsLogger,
    ) -> Result<()> {
        let Some(p) = self.pending.remove(order_id) else {
            return Ok(());
        };

        if polled.size_matched >= MIN_SHARES {
            let actual_shares = polled.size_matched.min(p.original_shares);
            let actual_usdc = actual_shares * polled.price;
            let mut record = build_trade_record(&p, "partial");
            record.entry_price = polled.price.to_string();
            record.size_usdc = actual_usdc.to_string();
            record.size_shares = actual_shares.to_string();
            let _ = logger.log_trade(&record);
            risk.release_reservation(&p.condition_id);
            risk.record_trade(
                actual_usdc,
                OpenPosition {
                    condition_id: p.condition_id.clone(),
                    order_id: p.order_id.clone(),
                    direction: p.meta.direction,
                    entry_price: polled.price,
                    size_usdc: actual_usdc,
                    size_shares: actual_shares,
                    end_date_ms: p.end_date_ms,
                },
            );
        } else {
            risk.release_reservation(&p.condition_id);
            let mut record = build_trade_record(&p, "expired");
            record.size_usdc = Decimal::ZERO.to_string();
            record.size_shares = Decimal::ZERO.to_string();
            let _ = logger.log_trade(&record);
        }
        Ok(())
    }

    fn handle_timeout_partial(
        &mut self,
        order_id: &str,
        polled_after: Option<&OrderPollResult>,
        risk: &mut RiskManager,
        logger: &MetricsLogger,
    ) -> Result<()> {
        let Some(p) = self.pending.remove(order_id) else {
            return Ok(());
        };

        let matched = polled_after
            .map(|x| x.size_matched)
            .unwrap_or(Decimal::ZERO);

        if matched >= MIN_SHARES {
            let price = polled_after.map(|x| x.price).unwrap_or(p.meta.limit_price);
            let actual_shares = matched.min(p.original_shares);
            let actual_usdc = actual_shares * price;
            let mut record = build_trade_record(&p, "partial");
            record.size_usdc = actual_usdc.to_string();
            record.size_shares = actual_shares.to_string();
            record.entry_price = price.to_string();
            let _ = logger.log_trade(&record);
            risk.release_reservation(&p.condition_id);
            risk.record_trade(
                actual_usdc,
                OpenPosition {
                    condition_id: p.condition_id.clone(),
                    order_id: p.order_id.clone(),
                    direction: p.meta.direction,
                    entry_price: price,
                    size_usdc: actual_usdc,
                    size_shares: actual_shares,
                    end_date_ms: p.end_date_ms,
                },
            );
        } else {
            risk.release_reservation(&p.condition_id);
            let mut record = build_trade_record(&p, "expired");
            record.size_usdc = Decimal::ZERO.to_string();
            record.size_shares = Decimal::ZERO.to_string();
            let _ = logger.log_trade(&record);
        }
        Ok(())
    }
}

fn build_trade_record(p: &PendingOrder, fill_status: &str) -> TradeRecord {
    let mut r = TradeRecord::new(
        p.condition_id.clone(),
        p.meta.asset.clone(),
        p.meta.duration.clone(),
        p.meta.direction,
        p.meta.limit_price,
        p.meta.size_usdc,
        p.meta.size_shares,
        p.meta.signal_probability,
        p.meta.confidence,
        p.meta.edge,
        p.meta.reasoning.clone(),
        p.order_id.clone(),
    );
    r.rsi = p.meta.rsi;
    r.macd_histogram = p.meta.macd_histogram;
    r.volume_ratio = p.meta.volume_ratio;
    r.cluster_direction = p.meta.cluster_direction.clone();
    r.market_yes_price = p.meta.market_yes_price.clone();
    r.liquidity = p.meta.liquidity.clone();
    r.secs_to_close = p.meta.secs_to_close;
    r.volatility_std_pct = p.meta.volatility_std_pct;
    r.kelly_fraction = p.meta.kelly_fraction.clone();
    r.balance_at_trade = Some(p.meta.balance_at_signal.clone());
    r.daily_loss_at_trade = Some(p.meta.daily_loss_at_signal.clone());
    r.htf_aligned = p.meta.htf_aligned;
    r.adaptive_min_edge = p.meta.adaptive_min_edge.clone();
    r.adaptive_min_confidence = p.meta.adaptive_min_confidence.clone();
    r.sizing_cap_hit = p.meta.sizing_cap_hit.clone();
    r.momentum_5m = p.meta.momentum_5m;
    r.momentum_15m = p.meta.momentum_15m;
    r.taker_buy_ratio = p.meta.taker_buy_ratio;
    r.macd_line = p.meta.macd_line;
    r.macd_signal_line = p.meta.macd_signal_line;
    r.question = p.meta.question.clone();
    r.slippage_bps = p.meta.slippage_bps.clone();
    r.effective_min_edge = p.meta.effective_min_edge.clone();
    r.fill_status = Some(fill_status.to_string());
    r
}

/// Build [`PendingOrder`] after a successful `place_order` that returned `Live`.
pub fn pending_from_outcome(
    outcome: &PlaceOrderOutcome,
    condition_id: String,
    end_date_ms: i64,
    meta: PendingTradeMeta,
) -> PendingOrder {
    PendingOrder {
        order_id: outcome.order_id.clone(),
        condition_id,
        end_date_ms,
        placed_at_ms: Utc::now().timestamp_millis(),
        original_shares: outcome.original_size_shares,
        cumulative_filled_shares: Decimal::ZERO,
        meta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::metrics::MetricsLogger;
    use crate::risk::RiskManager;
    use std::fs;

    fn test_logger() -> (MetricsLogger, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("order_tracker_ut_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp dir");
        let logger = MetricsLogger::new(dir.to_str().expect("utf8 path")).expect("logger");
        (logger, dir)
    }

    fn minimal_meta() -> PendingTradeMeta {
        PendingTradeMeta {
            asset: "btc".to_string(),
            duration: "5m".to_string(),
            direction: Direction::Yes,
            limit_price: dec!(0.5),
            size_usdc: dec!(10),
            size_shares: dec!(20),
            signal_probability: dec!(0.6),
            confidence: dec!(0.8),
            edge: dec!(0.1),
            reasoning: "test".to_string(),
            rsi: None,
            macd_histogram: None,
            volume_ratio: None,
            cluster_direction: None,
            market_yes_price: None,
            liquidity: None,
            secs_to_close: None,
            volatility_std_pct: None,
            kelly_fraction: None,
            balance_at_signal: "200".to_string(),
            daily_loss_at_signal: "0".to_string(),
            htf_aligned: None,
            adaptive_min_edge: None,
            adaptive_min_confidence: None,
            sizing_cap_hit: None,
            momentum_5m: None,
            momentum_15m: None,
            taker_buy_ratio: None,
            macd_line: None,
            macd_signal_line: None,
            question: None,
            slippage_bps: None,
            effective_min_edge: None,
        }
    }

    #[tokio::test]
    async fn process_fill_channel_unknown_order_is_ignored() {
        let (ft, mut rx) = FillTracker::new();
        let mut tracker = OrderTracker::new(ft.clone());
        let mut cfg = AppConfig::default();
        cfg.polymarket_private_key = "t".to_string();
        let mut risk = RiskManager::new_without_persistence(&cfg);
        let bal_before = risk.available_balance();
        let (logger, dir) = test_logger();

        ft.on_order_update("not-pending", dec!(5), None).await;
        tracker
            .process_fill_channel(&mut rx, &mut risk, &logger)
            .expect("process");
        assert_eq!(risk.available_balance(), bal_before);
        assert!(!tracker.has_pending());

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn process_fill_channel_full_fill_confirms_reserved_trade() {
        let (ft, mut rx) = FillTracker::new();
        let mut tracker = OrderTracker::new(ft.clone());
        let mut cfg = AppConfig::default();
        cfg.polymarket_private_key = "t".to_string();
        let mut risk = RiskManager::new_without_persistence(&cfg);
        let start = risk.available_balance();
        risk.reserve_for_order("0xc1", dec!(10));
        assert_eq!(risk.available_balance(), start - dec!(10));

        tracker.add_pending(PendingOrder {
            order_id: "ord-full".to_string(),
            condition_id: "0xc1".to_string(),
            end_date_ms: 0,
            placed_at_ms: 0,
            original_shares: dec!(10),
            cumulative_filled_shares: Decimal::ZERO,
            meta: minimal_meta(),
        });

        let (logger, dir) = test_logger();
        ft.on_order_update("ord-full", dec!(10), None).await;
        tracker
            .process_fill_channel(&mut rx, &mut risk, &logger)
            .expect("process");

        assert_eq!(risk.available_balance(), start - dec!(10));
        assert!(risk.has_position("0xc1"));
        assert!(!tracker.has_pending());

        let _ = fs::remove_dir_all(&dir);
    }
}
