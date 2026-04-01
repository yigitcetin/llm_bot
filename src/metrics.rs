//! File-based trade logs (`TradeRecord`, JSONL under `data/`).
//!
//! This is **not** Prometheus: scrape metrics live in [`crate::prometheus_export`] (`/metrics`).
//! Trade resolutions are written back into the same `trades.jsonl` line when a market settles.

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
        }
    }
}

/// Logs trades and skips to JSON Lines files; updates `trades.jsonl` when positions resolve.
pub struct MetricsLogger {
    trades_path: String,
    skips_path: String,
    order_failures_path: String,
}

impl MetricsLogger {
    pub fn new(data_dir: &str) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("failed to create data directory: {}", data_dir))?;

        Ok(Self {
            trades_path: format!("{}/trades.jsonl", data_dir),
            skips_path: format!("{}/skip_reasons.jsonl", data_dir),
            order_failures_path: format!("{}/order_failures.jsonl", data_dir),
        })
    }

    /// Log a new trade.
    pub fn log_trade(&self, record: &TradeRecord) -> Result<()> {
        let json = serde_json::to_string(record).context("failed to serialize trade record")?;

        self.append_line(&self.trades_path, &json)
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

        let t1 = rows.iter().find(|t| t.order_id == "order-a").expect("order-a");
        assert_eq!(t1.outcome, Some(false));
        assert_eq!(t1.pnl.as_deref(), Some("7.5"));
        assert!(t1.resolved_at.is_some());

        let t2 = rows.iter().find(|t| t.order_id == "order-b").expect("order-b");
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
        let pnl_first: String = serde_json::from_str::<TradeRecord>(
            after_first.lines().next().expect("line"),
        )
        .expect("parse")
        .pnl
        .expect("pnl");

        logger
            .update_trade_resolution("0xc1", "ord-1", false, dec!(99))
            .expect("second update noop");
        let after_second = fs::read_to_string(dir.join("trades.jsonl")).expect("read");
        let pnl_second: String = serde_json::from_str::<TradeRecord>(
            after_second.lines().next().expect("line"),
        )
        .expect("parse")
        .pnl
        .expect("pnl");

        assert_eq!(pnl_first, pnl_second, "already resolved row must not be overwritten");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn trade_record_deserializes_legacy_llm_probability_key() {
        let json = r#"{"timestamp":"2026-01-01T00:00:00Z","condition_id":"0x","asset":"btc","duration":"15m","direction":"NO","entry_price":"0.4","size_usdc":"10","size_shares":"25","llm_probability":"0.55","confidence":"0.7","edge":"0.1","reasoning":"t","order_id":"o1","outcome":null,"pnl":null,"resolved_at":null}"#;
        let t: TradeRecord = serde_json::from_str(json).expect("alias");
        assert_eq!(t.signal_probability, "0.55");
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
}
