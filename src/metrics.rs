//! File-based trade logs (`TradeRecord`, JSONL under `data/`).
//!
//! This is **not** Prometheus: scrape metrics live in [`crate::prometheus_export`] (`/metrics`).
//! Trade resolutions are written back into the same `trades.jsonl` line when a market settles.
//! Counterfactual (skipped) trades go to `shadow_trades.jsonl` with optional `skip_reason`.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use tracing::warn;

use crate::types::Direction;

/// A single trade record for logging and analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub timestamp: DateTime<Utc>,
    pub condition_id: String,
    pub asset: String,
    pub duration: String,
    pub direction: String, // "YES" | "NO"
    pub entry_price: String,
    pub size_usdc: String,
    pub size_shares: String,
    /// Technical signal probability vs YES (same as former `llm_probability` in JSON).
    #[serde(alias = "llm_probability")]
    pub signal_probability: String,
    pub confidence: String,
    pub edge: String,
    pub reasoning: String,
    pub order_id: String,
    pub outcome: Option<bool>, // true = YES won, false = NO won
    pub pnl: Option<String>,
    pub resolved_at: Option<DateTime<Utc>>,
    // --- Telemetry (optional for backward-compatible JSONL) ---
    #[serde(default)]
    pub rsi: Option<f64>,
    #[serde(default)]
    pub macd_histogram: Option<f64>,
    #[serde(default)]
    pub volume_ratio: Option<f64>,
    #[serde(default)]
    pub cluster_direction: Option<String>,
    #[serde(default)]
    pub market_yes_price: Option<String>,
    #[serde(default)]
    pub liquidity: Option<String>,
    #[serde(default)]
    pub secs_to_close: Option<i64>,
    #[serde(default)]
    pub volatility_std_pct: Option<f64>,
    #[serde(default)]
    pub kelly_fraction: Option<String>,
    #[serde(default)]
    pub balance_at_trade: Option<String>,
    #[serde(default)]
    pub daily_loss_at_trade: Option<String>,
    /// `Some(true)` when HTF filter ran and passed; `None` when HTF off or not applied.
    #[serde(default)]
    pub htf_aligned: Option<bool>,
    #[serde(default)]
    pub adaptive_min_edge: Option<String>,
    #[serde(default)]
    pub adaptive_min_confidence: Option<String>,
    #[serde(default)]
    pub sizing_cap_hit: Option<String>,
    #[serde(default)]
    pub momentum_5m: Option<f64>,
    #[serde(default)]
    pub momentum_15m: Option<f64>,
    #[serde(default)]
    pub taker_buy_ratio: Option<f64>,
    #[serde(default)]
    pub macd_line: Option<f64>,
    #[serde(default)]
    pub macd_signal_line: Option<f64>,
    #[serde(default)]
    pub question: Option<String>,
    #[serde(default)]
    pub slippage_bps: Option<String>,
    #[serde(default)]
    pub effective_min_edge: Option<String>,
    /// `filled` | `partial` | `expired` when order lifecycle is tracked (GTD); omitted on legacy rows.
    #[serde(default)]
    pub fill_status: Option<String>,
    /// When set, this row is a counterfactual (skipped trade) logged to `shadow_trades.jsonl`.
    #[serde(default)]
    pub skip_reason: Option<String>,
    /// Effective confidence after direction penalty (or raw when no penalty); snapshot for analysis.
    #[serde(default)]
    pub effective_confidence: Option<String>,
    /// Direction-specific confidence penalty active at trade time (`0` when off).
    #[serde(default)]
    pub direction_confidence_penalty: Option<String>,
    /// Snapshot of `min_macd_histogram_abs` when greater than 0 for this asset.
    #[serde(default)]
    pub min_macd_histogram_abs: Option<String>,
    /// Whether `taker_direction_confirm` was active for this asset.
    #[serde(default)]
    pub taker_direction_confirm: Option<bool>,
    /// Whether taker flow aligned with direction when filter on and TBR available.
    #[serde(default)]
    pub taker_direction_aligned: Option<bool>,
    #[serde(default)]
    pub effective_momentum_threshold: Option<String>,
    #[serde(default)]
    pub adaptive_penalty_applied: Option<String>,
    #[serde(default)]
    pub direction_wr_at_trade: Option<String>,
    #[serde(default)]
    pub multi_tf_direction_agreement: Option<bool>,
    #[serde(default)]
    pub multi_tf_confidence_adj: Option<String>,
    /// Shadow calibration version active when this trade was placed (`None` = base config).
    #[serde(default)]
    pub calibration_version: Option<u64>,
}

impl TradeRecord {
    pub fn new(
        condition_id: String,
        asset: String,
        duration: String,
        direction: Direction,
        entry_price: Decimal,
        size_usdc: Decimal,
        size_shares: Decimal,
        signal_probability: Decimal,
        confidence: Decimal,
        edge: Decimal,
        reasoning: String,
        order_id: String,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            condition_id,
            asset,
            duration,
            direction: match direction {
                Direction::Yes => "YES".to_string(),
                Direction::No => "NO".to_string(),
            },
            entry_price: entry_price.to_string(),
            size_usdc: size_usdc.to_string(),
            size_shares: size_shares.to_string(),
            signal_probability: signal_probability.to_string(),
            confidence: confidence.to_string(),
            edge: edge.to_string(),
            reasoning,
            order_id,
            outcome: None,
            pnl: None,
            resolved_at: None,
            rsi: None,
            macd_histogram: None,
            volume_ratio: None,
            cluster_direction: None,
            market_yes_price: None,
            liquidity: None,
            secs_to_close: None,
            volatility_std_pct: None,
            kelly_fraction: None,
            balance_at_trade: None,
            daily_loss_at_trade: None,
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
            fill_status: None,
            skip_reason: None,
            effective_confidence: None,
            direction_confidence_penalty: None,
            min_macd_histogram_abs: None,
            taker_direction_confirm: None,
            taker_direction_aligned: None,
            effective_momentum_threshold: None,
            adaptive_penalty_applied: None,
            direction_wr_at_trade: None,
            multi_tf_direction_agreement: None,
            multi_tf_confidence_adj: None,
            calibration_version: None,
        }
    }
}

/// Logs trades and skips to JSON Lines files; updates `trades.jsonl` when positions resolve.
pub struct MetricsLogger {
    trades_path: String,
    shadow_trades_path: String,
    skips_path: String,
    order_failures_path: String,
}

impl MetricsLogger {
    pub fn new(data_dir: &str) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("failed to create data directory: {}", data_dir))?;

        Ok(Self {
            trades_path: format!("{}/trades.jsonl", data_dir),
            shadow_trades_path: format!("{}/shadow_trades.jsonl", data_dir),
            skips_path: format!("{}/skip_reasons.jsonl", data_dir),
            order_failures_path: format!("{}/order_failures.jsonl", data_dir),
        })
    }

    /// Log a new trade.
    pub fn log_trade(&self, record: &TradeRecord) -> Result<()> {
        let json = serde_json::to_string(record).context("failed to serialize trade record")?;

        self.append_line(&self.trades_path, &json)
    }

    /// Log a counterfactual (shadow) trade to `shadow_trades.jsonl` for post-hoc PnL analysis.
    pub fn log_shadow_trade(&self, record: &TradeRecord) -> Result<()> {
        let json = serde_json::to_string(record).context("failed to serialize shadow trade record")?;

        self.append_line(&self.shadow_trades_path, &json)
    }

    /// Update an existing trade line with resolution outcome and PnL (rewrites `trades.jsonl`).
    pub fn update_trade_resolution(
        &self,
        condition_id: &str,
        order_id: &str,
        outcome: bool,
        pnl: Decimal,
    ) -> Result<()> {
        let path = &self.trades_path;
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                warn!(path = %path, "trades.jsonl not found; cannot update resolution");
                return Ok(());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("failed to read trades file: {}", path));
            }
        };

        let mut lines: Vec<String> = Vec::new();
        let mut updated = false;

        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<TradeRecord>(line) {
                Ok(mut trade) => {
                    if trade.condition_id == condition_id
                        && trade.order_id == order_id
                        && trade.outcome.is_none()
                    {
                        trade.outcome = Some(outcome);
                        trade.pnl = Some(pnl.to_string());
                        trade.resolved_at = Some(Utc::now());
                        updated = true;
                    }
                    lines.push(serde_json::to_string(&trade)?);
                }
                Err(_) => {
                    lines.push(line.to_string());
                }
            }
        }

        if updated {
            let mut file = std::fs::File::create(path)
                .with_context(|| format!("failed to rewrite trades file: {}", path))?;
            for line in &lines {
                writeln!(file, "{}", line)
                    .with_context(|| format!("failed to write trades file: {}", path))?;
            }
        } else {
            warn!(
                condition_id = %condition_id,
                order_id = %order_id,
                "no matching unresolved trade in trades.jsonl for resolution update"
            );
        }

        Ok(())
    }

    /// Log a skipped-trade decision reason.
    pub fn log_skip(&self, record: &SkipRecord) -> Result<()> {
        let json = serde_json::to_string(record).context("failed to serialize skip record")?;

        self.append_line(&self.skips_path, &json)
    }

    /// Log a CLOB / execution failure (distinct from skip reasons).
    pub fn log_order_failure(&self, record: &OrderFailureRecord) -> Result<()> {
        let json =
            serde_json::to_string(record).context("failed to serialize order failure record")?;
        self.append_line(&self.order_failures_path, &json)
    }

    /// Read all unresolved trades (outcome == null) from `trades.jsonl`.
    pub fn read_unresolved_trades(&self) -> Result<Vec<TradeRecord>> {
        let content = match std::fs::read_to_string(&self.trades_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };

        let mut unresolved = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(trade) = serde_json::from_str::<TradeRecord>(line) {
                if trade.outcome.is_none() {
                    unresolved.push(trade);
                }
            }
        }
        Ok(unresolved)
    }

    /// Read unresolved shadow trades (`outcome == null`) from `shadow_trades.jsonl`.
    pub fn read_unresolved_shadow_trades(&self) -> Result<Vec<TradeRecord>> {
        let content = match std::fs::read_to_string(&self.shadow_trades_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };

        let mut unresolved = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(trade) = serde_json::from_str::<TradeRecord>(line) {
                if trade.outcome.is_none() {
                    unresolved.push(trade);
                }
            }
        }
        Ok(unresolved)
    }

    /// Update a shadow trade line with resolution (rewrites `shadow_trades.jsonl`).
    pub fn update_shadow_trade_resolution(
        &self,
        condition_id: &str,
        order_id: &str,
        outcome: bool,
        pnl: Decimal,
    ) -> Result<()> {
        let path = &self.shadow_trades_path;
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                warn!(path = %path, "shadow_trades.jsonl not found; cannot update resolution");
                return Ok(());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("failed to read shadow trades file: {}", path));
            }
        };

        let mut lines: Vec<String> = Vec::new();
        let mut updated = false;

        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<TradeRecord>(line) {
                Ok(mut trade) => {
                    if trade.condition_id == condition_id
                        && trade.order_id == order_id
                        && trade.outcome.is_none()
                    {
                        trade.outcome = Some(outcome);
                        trade.pnl = Some(pnl.to_string());
                        trade.resolved_at = Some(Utc::now());
                        updated = true;
                    }
                    lines.push(serde_json::to_string(&trade)?);
                }
                Err(_) => {
                    lines.push(line.to_string());
                }
            }
        }

        if updated {
            let mut file = std::fs::File::create(path)
                .with_context(|| format!("failed to rewrite shadow trades file: {}", path))?;
            for line in &lines {
                writeln!(file, "{}", line)
                    .with_context(|| format!("failed to write shadow trades file: {}", path))?;
            }
        } else {
            warn!(
                condition_id = %condition_id,
                order_id = %order_id,
                "no matching unresolved shadow trade in shadow_trades.jsonl for resolution update"
            );
        }

        Ok(())
    }

    fn append_line(&self, path: &str, line: &str) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open file: {}", path))?;

        writeln!(file, "{}", line).with_context(|| format!("failed to write to file: {}", path))?;

        Ok(())
    }
}

/// Why a market was skipped in a cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkipRecord {
    pub timestamp: DateTime<Utc>,
    pub condition_id: String,
    pub asset: String,
    pub duration: String,
    pub question: String,
    pub reason: String,
    pub details: Option<String>,
}

impl SkipRecord {
    pub fn new(
        condition_id: String,
        asset: String,
        duration: String,
        question: String,
        reason: impl Into<String>,
        details: Option<String>,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            condition_id,
            asset,
            duration,
            question,
            reason: reason.into(),
            details,
        }
    }
}

/// Order placement failure for JSONL diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderFailureRecord {
    pub timestamp: DateTime<Utc>,
    pub condition_id: String,
    pub asset: String,
    pub duration: String,
    pub question: String,
    pub error: String,
}

impl OrderFailureRecord {
    pub fn new(
        condition_id: String,
        asset: String,
        duration: String,
        question: String,
        error: String,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            condition_id,
            asset,
            duration,
            question,
            error,
        }
    }
}

/// Read all parseable trade rows from `trades.jsonl` (for stats CLI / adaptive thresholds).
pub fn read_trades_from_path(path: &str) -> Result<Vec<TradeRecord>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read trades: {}", path)),
    };
    let mut out = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(t) = serde_json::from_str::<TradeRecord>(line) {
            out.push(t);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::fs;
    use std::path::PathBuf;

    fn test_logger_in_temp() -> (MetricsLogger, PathBuf) {
        let dir = std::env::temp_dir().join(format!("metrics_ut_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp dir");
        let logger = MetricsLogger::new(dir.to_str().expect("utf8 path")).expect("logger");
        (logger, dir)
    }

    #[test]
    fn update_trade_resolution_sets_outcome_pnl_resolved_at() {
        let (logger, dir) = test_logger_in_temp();
        let r1 = TradeRecord::new(
            "0xc1".to_string(),
            "btc".to_string(),
            "15m".to_string(),
            Direction::No,
            dec!(0.45),
            dec!(10),
            dec!(22.22),
            dec!(0.62),
            dec!(0.78),
            dec!(0.12),
            "rsi test".to_string(),
            "order-a".to_string(),
        );
        let r2 = TradeRecord::new(
            "0xc2".to_string(),
            "eth".to_string(),
            "5m".to_string(),
            Direction::Yes,
            dec!(0.5),
            dec!(5),
            dec!(10),
            dec!(0.6),
            dec!(0.7),
            dec!(0.1),
            "other".to_string(),
            "order-b".to_string(),
        );
        logger.log_trade(&r1).expect("log r1");
        logger.log_trade(&r2).expect("log r2");

        logger
            .update_trade_resolution("0xc1", "order-a", false, dec!(7.5))
            .expect("update");

        let path = dir.join("trades.jsonl");
        let content = fs::read_to_string(&path).expect("read trades");
        let rows: Vec<TradeRecord> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("parse row"))
            .collect();
        assert_eq!(rows.len(), 2);

        let t1 = rows
            .iter()
            .find(|t| t.order_id == "order-a")
            .expect("order-a");
        assert_eq!(t1.outcome, Some(false));
        assert_eq!(t1.pnl.as_deref(), Some("7.5"));
        assert!(t1.resolved_at.is_some());

        let t2 = rows
            .iter()
            .find(|t| t.order_id == "order-b")
            .expect("order-b");
        assert!(t2.outcome.is_none());
        assert!(t2.pnl.is_none());
        assert!(t2.resolved_at.is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_trade_resolution_second_call_does_not_change_resolved_row() {
        let (logger, dir) = test_logger_in_temp();
        let r = TradeRecord::new(
            "0xc1".to_string(),
            "btc".to_string(),
            "15m".to_string(),
            Direction::No,
            dec!(0.4),
            dec!(10),
            dec!(25),
            dec!(0.5),
            dec!(0.8),
            dec!(0.1),
            "x".to_string(),
            "ord-1".to_string(),
        );
        logger.log_trade(&r).expect("log");

        logger
            .update_trade_resolution("0xc1", "ord-1", true, dec!(15))
            .expect("first update");
        let after_first = fs::read_to_string(dir.join("trades.jsonl")).expect("read");
        let pnl_first: String =
            serde_json::from_str::<TradeRecord>(after_first.lines().next().expect("line"))
                .expect("parse")
                .pnl
                .expect("pnl");

        logger
            .update_trade_resolution("0xc1", "ord-1", false, dec!(99))
            .expect("second update noop");
        let after_second = fs::read_to_string(dir.join("trades.jsonl")).expect("read");
        let pnl_second: String =
            serde_json::from_str::<TradeRecord>(after_second.lines().next().expect("line"))
                .expect("parse")
                .pnl
                .expect("pnl");

        assert_eq!(
            pnl_first, pnl_second,
            "already resolved row must not be overwritten"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn trade_record_deserializes_legacy_llm_probability_key() {
        let json = r#"{"timestamp":"2026-01-01T00:00:00Z","condition_id":"0x","asset":"btc","duration":"15m","direction":"NO","entry_price":"0.4","size_usdc":"10","size_shares":"25","llm_probability":"0.55","confidence":"0.7","edge":"0.1","reasoning":"t","order_id":"o1","outcome":null,"pnl":null,"resolved_at":null}"#;
        let t: TradeRecord = serde_json::from_str(json).expect("alias");
        assert_eq!(t.signal_probability, "0.55");
        assert!(t.fill_status.is_none());
    }

    #[test]
    fn trade_record_fill_status_serde_roundtrip() {
        let mut r = TradeRecord::new(
            "0xc1".to_string(),
            "btc".to_string(),
            "5m".to_string(),
            Direction::Yes,
            dec!(0.5),
            dec!(10),
            dec!(20),
            dec!(0.6),
            dec!(0.8),
            dec!(0.1),
            "r".to_string(),
            "ord-fs".to_string(),
        );
        r.fill_status = Some("partial".to_string());
        let json = serde_json::to_string(&r).expect("serialize");
        let back: TradeRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.fill_status.as_deref(), Some("partial"));
    }

    #[test]
    fn update_trade_resolution_missing_trades_file_returns_ok() {
        let dir = std::env::temp_dir().join(format!("metrics_ut_nf_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp dir");
        let logger = MetricsLogger::new(dir.to_str().expect("path")).expect("logger");
        logger
            .update_trade_resolution("x", "y", true, dec!(1))
            .expect("missing trades.jsonl should not error");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_shadow_trade_resolution_sets_outcome_pnl() {
        let (logger, dir) = test_logger_in_temp();
        let mut r = TradeRecord::new(
            "0xshadow".to_string(),
            "btc".to_string(),
            "15m".to_string(),
            Direction::Yes,
            dec!(0.5),
            dec!(5),
            dec!(10),
            dec!(0.6),
            dec!(0.8),
            dec!(0.1),
            "s".to_string(),
            "shadow-abc".to_string(),
        );
        r.skip_reason = Some("edge_too_small".to_string());
        r.question = Some("Bitcoin Up or Down - April 11, 8:00AM-8:15AM ET".to_string());
        logger.log_shadow_trade(&r).expect("log shadow");

        logger
            .update_shadow_trade_resolution("0xshadow", "shadow-abc", true, dec!(5))
            .expect("update shadow");

        let path = dir.join("shadow_trades.jsonl");
        let line = fs::read_to_string(&path).expect("read shadow");
        let t: TradeRecord = serde_json::from_str(line.lines().next().expect("line")).expect("parse");
        assert_eq!(t.outcome, Some(true));
        assert_eq!(t.pnl.as_deref(), Some("5"));
        assert!(t.resolved_at.is_some());
        assert_eq!(t.skip_reason.as_deref(), Some("edge_too_small"));

        let _ = fs::remove_dir_all(&dir);
    }
}
