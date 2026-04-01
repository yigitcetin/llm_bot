//! File-based trade and resolution logs (`TradeRecord`, JSONL under `data/`).
//!
//! This is **not** Prometheus: scrape metrics live in [`crate::prometheus_export`] (`/metrics`).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
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
    pub llm_probability: String,
    pub confidence: String,
    pub edge: String,
    pub news_summary: String,
    pub reasoning: String,
    pub order_id: String,
    // Resolution fields (filled later)
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
        llm_probability: Decimal,
        confidence: Decimal,
        edge: Decimal,
        news_summary: String,
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
            llm_probability: llm_probability.to_string(),
            confidence: confidence.to_string(),
            edge: edge.to_string(),
            news_summary,
            reasoning,
            order_id,
            outcome: None,
            pnl: None,
            resolved_at: None,
        }
    }
}

/// Resolution record for a completed trade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolutionRecord {
    pub timestamp: DateTime<Utc>,
    pub condition_id: String,
    pub order_id: String,
    pub outcome: bool, // true = YES won
    pub pnl: String,
}

/// Logs trades and resolutions to JSON Lines files.
pub struct MetricsLogger {
    trades_path: String,
    resolutions_path: String,
    skips_path: String,
}

impl MetricsLogger {
    pub fn new(data_dir: &str) -> Result<Self> {
        // Ensure data directory exists
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("failed to create data directory: {}", data_dir))?;

        Ok(Self {
            trades_path: format!("{}/trades.jsonl", data_dir),
            resolutions_path: format!("{}/resolutions.jsonl", data_dir),
            skips_path: format!("{}/skip_reasons.jsonl", data_dir),
        })
    }

    /// Log a new trade.
    pub fn log_trade(&self, record: &TradeRecord) -> Result<()> {
        let json = serde_json::to_string(record)
            .context("failed to serialize trade record")?;

        self.append_line(&self.trades_path, &json)
    }

    /// Log a market resolution.
    pub fn log_resolution(&self, record: &ResolutionRecord) -> Result<()> {
        let json = serde_json::to_string(record)
            .context("failed to serialize resolution record")?;

        self.append_line(&self.resolutions_path, &json)
    }

    /// Log a skipped-trade decision reason.
    pub fn log_skip(&self, record: &SkipRecord) -> Result<()> {
        let json = serde_json::to_string(record)
            .context("failed to serialize skip record")?;

        self.append_line(&self.skips_path, &json)
    }

    /// Append a line to a file (creates if doesn't exist).
    fn append_line(&self, path: &str, line: &str) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open file: {}", path))?;

        writeln!(file, "{}", line)
            .with_context(|| format!("failed to write to file: {}", path))?;

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

/// Read all trades from the log file.
pub fn read_trades(data_dir: &str) -> Result<Vec<TradeRecord>> {
    let path = format!("{}/trades.jsonl", data_dir);

    if !Path::new(&path).exists() {
        return Ok(vec![]);
    }

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read trades file: {}", path))?;

    let mut trades = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<TradeRecord>(line) {
            Ok(trade) => trades.push(trade),
            Err(e) => {
                warn!(line_num = i + 1, error = %e, "failed to parse trade record");
            }
        }
    }

    Ok(trades)
}

/// Read all resolutions from the log file.
pub fn read_resolutions(data_dir: &str) -> Result<Vec<ResolutionRecord>> {
    let path = format!("{}/resolutions.jsonl", data_dir);

    if !Path::new(&path).exists() {
        return Ok(vec![]);
    }

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read resolutions file: {}", path))?;

    let mut resolutions = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<ResolutionRecord>(line) {
            Ok(res) => resolutions.push(res),
            Err(e) => {
                warn!(line_num = i + 1, error = %e, "failed to parse resolution record");
            }
        }
    }

    Ok(resolutions)
}
