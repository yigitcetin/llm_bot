//! Generates a diagnostic report when the inactivity watchdog fires.
//!
//! Reads all data files (`trades.jsonl`, `shadow_trades.jsonl`, `skip_reasons.jsonl`,
//! `calibration_state.json`, `balance_state.json`) to produce a comprehensive
//! JSON report under `data/inactivity_report_<ts>.json`.
//! The report is purely informational — no parameters are changed.

use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use tracing::{info, warn};

use crate::config::AssetStrategy;
use crate::metrics::{read_trades_from_path, SkipRecord, TradeRecord};
use crate::shadow_calibrator::CalibrationStateFile;

// ---------------------------------------------------------------------------
// Report structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct InactivityReport {
    pub generated_at: DateTime<Utc>,
    pub lookback_hours: u64,
    pub balance: BalanceSection,
    pub real_trade_performance: RealTradePerformance,
    pub skip_analysis: SkipAnalysis,
    pub skip_time_trend: Vec<HourlySkipBucket>,
    pub shadow_analysis: ShadowAnalysis,
    pub calibration_status: CalibrationSummary,
    pub balance_trend: BalanceTrend,
    pub recommendations: Vec<Recommendation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BalanceSection {
    pub available_balance: String,
    pub deadlock_detected: bool,
    pub max_order_usdc: String,
    pub min_order_usdc: String,
    pub min_order_usdc_floor: String,
    pub dynamic_min: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkipAnalysis {
    pub total_skips: usize,
    pub reason_distribution: Vec<ReasonCount>,
    pub top_blocker: Option<String>,
    pub asset_distribution: Vec<AssetSkipCount>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReasonCount {
    pub reason: String,
    pub count: usize,
    pub pct: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssetSkipCount {
    pub asset: String,
    pub count: usize,
    pub top_reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShadowAnalysis {
    pub total_resolved: usize,
    pub overall_wr: f64,
    pub overall_pnl: f64,
    pub per_skip_reason: Vec<ShadowReasonStats>,
    pub per_asset: Vec<ShadowAssetSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShadowReasonStats {
    pub reason: String,
    pub count: usize,
    pub wr: f64,
    pub pnl: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShadowAssetSummary {
    pub asset: String,
    pub count: usize,
    pub wr: f64,
    pub pnl: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RealTradePerformance {
    pub lookback_hours: u64,
    pub total_trades: usize,
    pub resolved: usize,
    pub pending: usize,
    pub wr: f64,
    pub pnl: f64,
    pub per_asset: Vec<RealTradeAssetSummary>,
    pub per_asset_direction: Vec<RealTradeDirectionSummary>,
    pub last_trade_at: Option<DateTime<Utc>>,
    pub hours_since_last_trade: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RealTradeAssetSummary {
    pub asset: String,
    pub count: usize,
    pub wr: f64,
    pub pnl: f64,
}

/// YES vs NO win rates per asset (resolved trades in lookback window).
#[derive(Debug, Clone, Serialize)]
pub struct RealTradeDirectionSummary {
    pub asset: String,
    pub yes_count: usize,
    pub yes_wr: f64,
    pub no_count: usize,
    pub no_wr: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HourlySkipBucket {
    pub hour_label: String,
    pub count: usize,
    pub top_reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalibrationSummary {
    pub loaded: bool,
    pub global_version: u64,
    pub per_asset: Vec<CalibrationAssetSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalibrationAssetSummary {
    pub asset: String,
    pub calibration_version: u64,
    pub shadow_wr: f64,
    pub shadow_pnl: f64,
    pub shadow_trade_count: usize,
    pub last_calibrated_at: String,
    pub rolled_back: bool,
    pub drift_params: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct BalanceTrend {
    pub data_points: Vec<BalancePoint>,
    pub peak_balance: f64,
    pub trough_balance: f64,
    pub current_drawdown_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BalancePoint {
    pub timestamp: DateTime<Utc>,
    pub balance: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Recommendation {
    /// When set, the recommendation targets this asset (global advice uses `None`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    pub priority: &'static str,
    pub category: &'static str,
    pub message: String,
}

/// One-line summary of recommendation rows for operator logs (paired with [`generate_report`]).
pub fn format_recommendations_log_summary(recs: &[Recommendation]) -> String {
    use std::collections::HashMap;

    if recs.is_empty() {
        return "recs=0".to_string();
    }

    let mut pri: HashMap<&str, usize> = HashMap::new();
    for r in recs {
        *pri.entry(r.priority).or_insert(0) += 1;
    }
    let pri_part = pri
        .into_iter()
        .map(|(k, v)| format!("{}:{}", k, v))
        .collect::<Vec<_>>()
        .join(",");

    let mut assets: Vec<&str> = recs.iter().filter_map(|r| r.asset.as_deref()).collect();
    assets.sort_unstable();
    assets.dedup();
    let asset_part = if assets.is_empty() {
        "-".to_string()
    } else {
        assets.join(",")
    };

    format!(
        "recs={} priority{{{}}} assets{{{}}}",
        recs.len(),
        pri_part,
        asset_part
    )
}

// ---------------------------------------------------------------------------
// Report generation
// ---------------------------------------------------------------------------

const LOOKBACK_HOURS: u64 = 4;
const TRADE_LOOKBACK_HOURS: u64 = 24;
const MAX_REPORTS: usize = 50;

/// Minimum resolved trades per asset to warn about weak recent performance.
const ASSET_PERF_MIN_TRADES: usize = 3;
/// Win-rate fraction below which we flag an asset (recent real trades).
const ASSET_PERF_MAX_WR: f64 = 0.45;
const ASSET_SHADOW_MIN_COUNT: usize = 5;
const ASSET_SHADOW_MIN_PNL: f64 = 20.0;
const ASSET_SHADOW_MIN_WR: f64 = 0.65;
/// Minimum trades on each side (YES and NO) to compare direction imbalance.
const DIR_MIN_EACH_SIDE: usize = 2;
/// Absolute YES−NO WR gap (fraction) to flag direction imbalance.
const DIR_WR_GAP: f64 = 0.25;
/// Minimum count of non-null calibration overrides to emit drift INFO for an asset.
const CAL_DRIFT_MIN_PARAMS: usize = 10;

pub fn generate_report(
    data_dir: &str,
    available_balance: Decimal,
    strategies: &HashMap<String, AssetStrategy>,
) -> Result<(String, String)> {
    let cutoff = Utc::now() - Duration::hours(LOOKBACK_HOURS as i64);
    let trade_cutoff = Utc::now() - Duration::hours(TRADE_LOOKBACK_HOURS as i64);

    let skip_analysis = analyze_skips(data_dir, cutoff)?;
    let skip_time_trend = analyze_skip_time_trend(data_dir, cutoff)?;
    let shadow_analysis = analyze_shadows(data_dir, cutoff)?;
    let real_trade_performance = analyze_real_trades(data_dir, trade_cutoff)?;
    let balance_section = check_balance(available_balance, strategies);
    let calibration_status = analyze_calibration(data_dir);
    let balance_trend = analyze_balance_trend(data_dir, trade_cutoff)?;
    let recommendations = build_recommendations(
        &balance_section,
        &skip_analysis,
        &shadow_analysis,
        &real_trade_performance,
        &balance_trend,
        &calibration_status,
    );
    let log_summary = format_recommendations_log_summary(&recommendations);

    let report = InactivityReport {
        generated_at: Utc::now(),
        lookback_hours: LOOKBACK_HOURS,
        balance: balance_section,
        real_trade_performance,
        skip_analysis,
        skip_time_trend,
        shadow_analysis,
        calibration_status,
        balance_trend,
        recommendations,
    };

    let json = serde_json::to_string_pretty(&report)
        .context("failed to serialize inactivity report")?;

    let filename = format!(
        "inactivity_report_{}.json",
        Utc::now().format("%Y%m%d_%H%M%S")
    );
    let path = format!("{}/{}", data_dir, filename);

    std::fs::write(&path, &json)
        .with_context(|| format!("failed to write report: {}", path))?;

    cleanup_old_reports(data_dir);

    info!(path = %path, "inactivity diagnostic report generated");

    Ok((path, log_summary))
}

// ---------------------------------------------------------------------------
// Skip analysis
// ---------------------------------------------------------------------------

fn read_skip_records(data_dir: &str) -> Result<Vec<SkipRecord>> {
    let path = format!("{}/skip_reasons.jsonl", data_dir);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read skips: {}", path)),
    };
    let mut out = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(r) = serde_json::from_str::<SkipRecord>(line) {
            out.push(r);
        }
    }
    Ok(out)
}

fn analyze_skips(data_dir: &str, cutoff: DateTime<Utc>) -> Result<SkipAnalysis> {
    let all = read_skip_records(data_dir)?;
    let recent: Vec<&SkipRecord> = all.iter().filter(|s| s.timestamp >= cutoff).collect();
    let total = recent.len();

    let mut reason_counts: HashMap<String, usize> = HashMap::new();
    let mut asset_reasons: HashMap<String, HashMap<String, usize>> = HashMap::new();

    for s in &recent {
        *reason_counts.entry(s.reason.clone()).or_default() += 1;
        *asset_reasons
            .entry(s.asset.clone())
            .or_default()
            .entry(s.reason.clone())
            .or_default() += 1;
    }

    let mut reason_distribution: Vec<ReasonCount> = reason_counts
        .iter()
        .map(|(r, &c)| ReasonCount {
            reason: r.clone(),
            count: c,
            pct: if total > 0 { c as f64 / total as f64 * 100.0 } else { 0.0 },
        })
        .collect();
    reason_distribution.sort_by(|a, b| b.count.cmp(&a.count));

    let top_blocker = reason_distribution.first().map(|r| r.reason.clone());

    let mut asset_distribution: Vec<AssetSkipCount> = asset_reasons
        .iter()
        .map(|(asset, reasons)| {
            let total_asset: usize = reasons.values().sum();
            let top = reasons
                .iter()
                .max_by_key(|(_r, &c)| c)
                .map(|(r, _)| r.clone())
                .unwrap_or_default();
            AssetSkipCount {
                asset: asset.clone(),
                count: total_asset,
                top_reason: top,
            }
        })
        .collect();
    asset_distribution.sort_by(|a, b| b.count.cmp(&a.count));

    Ok(SkipAnalysis {
        total_skips: total,
        reason_distribution,
        top_blocker,
        asset_distribution,
    })
}

// ---------------------------------------------------------------------------
// Shadow trade analysis
// ---------------------------------------------------------------------------

fn analyze_shadows(data_dir: &str, cutoff: DateTime<Utc>) -> Result<ShadowAnalysis> {
    let path = format!("{}/shadow_trades.jsonl", data_dir);
    let all = read_trades_from_path(&path)?;
    let resolved: Vec<&TradeRecord> = all
        .iter()
        .filter(|t| t.outcome.is_some() && t.timestamp >= cutoff)
        .collect();

    let total = resolved.len();
    let wins = resolved.iter().filter(|t| shadow_won(t)).count();
    let overall_wr = if total > 0 { wins as f64 / total as f64 } else { 0.0 };
    let overall_pnl: f64 = resolved
        .iter()
        .filter_map(|t| t.pnl.as_ref()?.parse::<f64>().ok())
        .sum();

    // Per skip_reason
    let mut by_reason: HashMap<String, Vec<&TradeRecord>> = HashMap::new();
    for t in &resolved {
        let reason = t.skip_reason.as_deref().unwrap_or("unknown").to_string();
        by_reason.entry(reason).or_default().push(t);
    }
    let mut per_skip_reason: Vec<ShadowReasonStats> = by_reason
        .iter()
        .map(|(reason, trades)| {
            let w = trades.iter().filter(|t| shadow_won(t)).count();
            let pnl: f64 = trades
                .iter()
                .filter_map(|t| t.pnl.as_ref()?.parse::<f64>().ok())
                .sum();
            ShadowReasonStats {
                reason: reason.clone(),
                count: trades.len(),
                wr: if trades.is_empty() { 0.0 } else { w as f64 / trades.len() as f64 },
                pnl,
            }
        })
        .collect();
    per_skip_reason.sort_by(|a, b| b.count.cmp(&a.count));

    // Per asset
    let mut by_asset: HashMap<String, Vec<&TradeRecord>> = HashMap::new();
    for t in &resolved {
        by_asset.entry(t.asset.clone()).or_default().push(t);
    }
    let mut per_asset: Vec<ShadowAssetSummary> = by_asset
        .iter()
        .map(|(asset, trades)| {
            let w = trades.iter().filter(|t| shadow_won(t)).count();
            let pnl: f64 = trades
                .iter()
                .filter_map(|t| t.pnl.as_ref()?.parse::<f64>().ok())
                .sum();
            ShadowAssetSummary {
                asset: asset.clone(),
                count: trades.len(),
                wr: if trades.is_empty() { 0.0 } else { w as f64 / trades.len() as f64 },
                pnl,
            }
        })
        .collect();
    per_asset.sort_by(|a, b| b.count.cmp(&a.count));

    Ok(ShadowAnalysis {
        total_resolved: total,
        overall_wr,
        overall_pnl,
        per_skip_reason,
        per_asset,
    })
}

fn shadow_won(t: &TradeRecord) -> bool {
    let Some(outcome) = t.outcome else { return false };
    matches!((t.direction.as_str(), outcome), ("YES", true) | ("NO", false))
}

fn trade_won(t: &TradeRecord) -> bool {
    shadow_won(t)
}

// ---------------------------------------------------------------------------
// Real trade performance (last 24h from trades.jsonl)
// ---------------------------------------------------------------------------

fn analyze_real_trades(data_dir: &str, cutoff: DateTime<Utc>) -> Result<RealTradePerformance> {
    let path = format!("{}/trades.jsonl", data_dir);
    let all = read_trades_from_path(&path)?;

    let last_trade_at = all.iter().map(|t| t.timestamp).max();
    let hours_since = last_trade_at.map(|lt| {
        let dur = Utc::now().signed_duration_since(lt);
        dur.num_minutes() as f64 / 60.0
    });

    let recent: Vec<&TradeRecord> = all.iter().filter(|t| t.timestamp >= cutoff).collect();
    let total = recent.len();
    let resolved: Vec<&&TradeRecord> = recent.iter().filter(|t| t.outcome.is_some()).collect();
    let pending = total - resolved.len();
    let wins = resolved.iter().filter(|t| trade_won(t)).count();
    let wr = if resolved.is_empty() { 0.0 } else { wins as f64 / resolved.len() as f64 };
    let pnl: f64 = resolved
        .iter()
        .filter_map(|t| t.pnl.as_ref()?.parse::<f64>().ok())
        .sum();

    let mut by_asset: HashMap<String, Vec<&&TradeRecord>> = HashMap::new();
    for t in &resolved {
        by_asset.entry(t.asset.clone()).or_default().push(t);
    }
    let mut per_asset: Vec<RealTradeAssetSummary> = by_asset
        .iter()
        .map(|(asset, trades)| {
            let w = trades.iter().filter(|t| trade_won(t)).count();
            let p: f64 = trades
                .iter()
                .filter_map(|t| t.pnl.as_ref()?.parse::<f64>().ok())
                .sum();
            RealTradeAssetSummary {
                asset: asset.clone(),
                count: trades.len(),
                wr: if trades.is_empty() { 0.0 } else { w as f64 / trades.len() as f64 },
                pnl: p,
            }
        })
        .collect();
    per_asset.sort_by(|a, b| b.pnl.partial_cmp(&a.pnl).unwrap_or(std::cmp::Ordering::Equal));

    let mut yes_stats: HashMap<String, (usize, usize)> = HashMap::new();
    let mut no_stats: HashMap<String, (usize, usize)> = HashMap::new();
    for t in &resolved {
        match t.direction.as_str() {
            "YES" => {
                let e = yes_stats.entry(t.asset.clone()).or_insert((0, 0));
                e.0 += 1;
                if trade_won(t) {
                    e.1 += 1;
                }
            }
            "NO" => {
                let e = no_stats.entry(t.asset.clone()).or_insert((0, 0));
                e.0 += 1;
                if trade_won(t) {
                    e.1 += 1;
                }
            }
            _ => {}
        }
    }
    let mut asset_keys: Vec<String> = yes_stats.keys().cloned().collect();
    for k in no_stats.keys() {
        if !asset_keys.contains(k) {
            asset_keys.push(k.clone());
        }
    }
    asset_keys.sort();
    let per_asset_direction: Vec<RealTradeDirectionSummary> = asset_keys
        .into_iter()
        .map(|asset| {
            let (yt, yw) = yes_stats.get(&asset).copied().unwrap_or((0, 0));
            let (nt, nw) = no_stats.get(&asset).copied().unwrap_or((0, 0));
            RealTradeDirectionSummary {
                asset,
                yes_count: yt,
                yes_wr: if yt == 0 { 0.0 } else { yw as f64 / yt as f64 },
                no_count: nt,
                no_wr: if nt == 0 { 0.0 } else { nw as f64 / nt as f64 },
            }
        })
        .collect();

    Ok(RealTradePerformance {
        lookback_hours: TRADE_LOOKBACK_HOURS,
        total_trades: total,
        resolved: resolved.len(),
        pending,
        wr,
        pnl,
        per_asset,
        per_asset_direction,
        last_trade_at,
        hours_since_last_trade: hours_since,
    })
}

// ---------------------------------------------------------------------------
// Skip time trend (hourly buckets)
// ---------------------------------------------------------------------------

fn analyze_skip_time_trend(data_dir: &str, cutoff: DateTime<Utc>) -> Result<Vec<HourlySkipBucket>> {
    let all = read_skip_records(data_dir)?;
    let recent: Vec<&SkipRecord> = all.iter().filter(|s| s.timestamp >= cutoff).collect();

    let mut buckets: HashMap<String, HashMap<String, usize>> = HashMap::new();
    for s in &recent {
        let label = s.timestamp.format("%Y-%m-%d %H:00").to_string();
        *buckets
            .entry(label)
            .or_default()
            .entry(s.reason.clone())
            .or_default() += 1;
    }

    let mut result: Vec<HourlySkipBucket> = buckets
        .iter()
        .map(|(label, reasons)| {
            let total: usize = reasons.values().sum();
            let top = reasons
                .iter()
                .max_by_key(|(_, &c)| c)
                .map(|(r, _)| r.clone())
                .unwrap_or_default();
            HourlySkipBucket {
                hour_label: label.clone(),
                count: total,
                top_reason: top,
            }
        })
        .collect();
    result.sort_by(|a, b| a.hour_label.cmp(&b.hour_label));

    Ok(result)
}

// ---------------------------------------------------------------------------
// Calibration state summary
// ---------------------------------------------------------------------------

fn analyze_calibration(data_dir: &str) -> CalibrationSummary {
    let path = format!("{}/calibration_state.json", data_dir);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            return CalibrationSummary {
                loaded: false,
                global_version: 0,
                per_asset: vec![],
            };
        }
    };

    let state: CalibrationStateFile = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(_) => {
            return CalibrationSummary {
                loaded: false,
                global_version: 0,
                per_asset: vec![],
            };
        }
    };

    let mut per_asset: Vec<CalibrationAssetSummary> = state
        .assets
        .iter()
        .map(|(asset, s)| {
            let overrides = &s.applied_overrides;
            let drift_count = [
                overrides.min_edge.is_some(),
                overrides.min_confidence.is_some(),
                overrides.yes_confidence_penalty.is_some(),
                overrides.no_confidence_penalty.is_some(),
                overrides.rsi_yes_max.is_some(),
                overrides.rsi_no_min.is_some(),
                overrides.cluster_rsi_oversold.is_some(),
                overrides.cluster_rsi_overbought.is_some(),
                overrides.min_macd_histogram_abs.is_some(),
                overrides.volume_min_ratio.is_some(),
                overrides.min_momentum_5m_abs.is_some(),
                overrides.cluster_tie_min_edge_multiplier.is_some(),
                overrides.neutral_taker_edge_multiplier.is_some(),
                overrides.htf_enabled.is_some(),
                overrides.taker_direction_confirm.is_some(),
                overrides.dynamic_momentum_threshold.is_some(),
                overrides.multi_tf_enabled.is_some(),
            ]
            .iter()
            .filter(|&&v| v)
            .count();

            CalibrationAssetSummary {
                asset: asset.clone(),
                calibration_version: s.calibration_version,
                shadow_wr: s.shadow_wr,
                shadow_pnl: s.shadow_pnl,
                shadow_trade_count: s.shadow_trade_count,
                last_calibrated_at: s.last_calibrated_at.to_rfc3339(),
                rolled_back: s.rolled_back,
                drift_params: drift_count,
            }
        })
        .collect();
    per_asset.sort_by(|a, b| a.asset.cmp(&b.asset));

    CalibrationSummary {
        loaded: true,
        global_version: state.global_version,
        per_asset,
    }
}

// ---------------------------------------------------------------------------
// Balance trend from trades.jsonl (balance_at_trade field)
// ---------------------------------------------------------------------------

fn analyze_balance_trend(data_dir: &str, cutoff: DateTime<Utc>) -> Result<BalanceTrend> {
    let path = format!("{}/trades.jsonl", data_dir);
    let all = read_trades_from_path(&path)?;

    let mut points: Vec<BalancePoint> = all
        .iter()
        .filter(|t| t.timestamp >= cutoff)
        .filter_map(|t| {
            let bal_str = t.balance_at_trade.as_ref()?;
            let bal = bal_str.parse::<f64>().ok()?;
            Some(BalancePoint {
                timestamp: t.timestamp,
                balance: bal,
            })
        })
        .collect();
    points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    if points.is_empty() {
        return Ok(BalanceTrend {
            data_points: vec![],
            peak_balance: 0.0,
            trough_balance: 0.0,
            current_drawdown_pct: 0.0,
        });
    }

    let peak = points.iter().map(|p| p.balance).fold(f64::MIN, f64::max);
    let trough = points.iter().map(|p| p.balance).fold(f64::MAX, f64::min);
    let last = points.last().map(|p| p.balance).unwrap_or(0.0);
    let drawdown_pct = if peak > 0.0 { (peak - last) / peak * 100.0 } else { 0.0 };

    Ok(BalanceTrend {
        data_points: points,
        peak_balance: peak,
        trough_balance: trough,
        current_drawdown_pct: drawdown_pct,
    })
}

// ---------------------------------------------------------------------------
// Balance deadlock check
// ---------------------------------------------------------------------------

fn check_balance(
    available_balance: Decimal,
    strategies: &HashMap<String, AssetStrategy>,
) -> BalanceSection {
    let first_st = strategies.values().next();
    let (max_position_pct, min_order, floor) = match first_st {
        Some(st) => (st.max_position_pct, st.min_order_usdc, st.min_order_usdc_floor),
        None => (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO),
    };

    let max_order = available_balance * max_position_pct;
    let dynamic_min = floor
        .max((available_balance * max_position_pct * Decimal::new(80, 2)).round_dp(2))
        .min(min_order);
    let deadlock = max_order < floor;

    BalanceSection {
        available_balance: available_balance.to_string(),
        deadlock_detected: deadlock,
        max_order_usdc: max_order.round_dp(2).to_string(),
        min_order_usdc: min_order.to_string(),
        min_order_usdc_floor: floor.to_string(),
        dynamic_min: dynamic_min.round_dp(2).to_string(),
    }
}

// ---------------------------------------------------------------------------
// Recommendation engine
// ---------------------------------------------------------------------------

fn build_recommendations(
    balance: &BalanceSection,
    skips: &SkipAnalysis,
    shadows: &ShadowAnalysis,
    real_trades: &RealTradePerformance,
    balance_trend: &BalanceTrend,
    calibration: &CalibrationSummary,
) -> Vec<Recommendation> {
    let mut recs = Vec::new();

    // --- CRITICAL: balance deadlock ---
    if balance.deadlock_detected {
        recs.push(Recommendation {
            asset: None,
            priority: "CRITICAL",
            category: "balance",
            message: format!(
                "Balance deadlock: max_order ({}) < min_order_usdc_floor ({}). \
                 Bot cannot place any trade. Deposit funds or lower min_order_usdc_floor.",
                balance.max_order_usdc, balance.min_order_usdc_floor
            ),
        });
    }

    // --- HIGH: dominant skip reason ---
    if let Some(top) = &skips.top_blocker {
        let top_count = skips.reason_distribution.first().map(|r| r.count).unwrap_or(0);
        let pct = skips.reason_distribution.first().map(|r| r.pct).unwrap_or(0.0);

        if pct > 50.0 {
            recs.push(Recommendation {
                asset: None,
                priority: "HIGH",
                category: "skip_filter",
                message: format!(
                    "'{}' accounts for {:.0}% of all skips ({}/{} in last {} hours). \
                     Consider relaxing this filter or adjusting related thresholds.",
                    top, pct, top_count, skips.total_skips, LOOKBACK_HOURS
                ),
            });
        }
    }

    // --- HIGH: sizing deadlock ---
    if skips.total_skips > 0 {
        if let Some(rc) = skips
            .reason_distribution
            .iter()
            .find(|r| r.reason == "order_size_below_minimum")
        {
            if rc.pct > 30.0 {
                recs.push(Recommendation {
                    asset: None,
                    priority: "HIGH",
                    category: "sizing",
                    message: format!(
                        "order_size_below_minimum is {:.0}% of skips. Balance or max_position_pct may be too low \
                         for current min_order thresholds. Current dynamic_min={}.",
                        rc.pct, balance.dynamic_min
                    ),
                });
            }
        }
    }

    // --- HIGH: significant drawdown ---
    if balance_trend.current_drawdown_pct > 15.0 {
        recs.push(Recommendation {
            asset: None,
            priority: "HIGH",
            category: "drawdown",
            message: format!(
                "Balance drawdown is {:.1}% from peak ({:.2} → current {}). \
                 Consider pausing or reducing position sizes.",
                balance_trend.current_drawdown_pct,
                balance_trend.peak_balance,
                balance.available_balance
            ),
        });
    }

    // --- HIGH: prolonged inactivity ---
    if let Some(hours) = real_trades.hours_since_last_trade {
        if hours > 12.0 {
            recs.push(Recommendation {
                asset: None,
                priority: "HIGH",
                category: "inactivity",
                message: format!(
                    "No trades placed for {:.1} hours. Last trade: {}.",
                    hours,
                    real_trades
                        .last_trade_at
                        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or_else(|| "never".to_string())
                ),
            });
        }
    }

    // --- INFO: calibration drift (per asset) ---
    if calibration.loaded {
        for ca in &calibration.per_asset {
            if ca.drift_params >= CAL_DRIFT_MIN_PARAMS {
                recs.push(Recommendation {
                    asset: Some(ca.asset.clone()),
                    priority: "INFO",
                    category: "calibration_drift",
                    message: format!(
                        "{}: {} calibrated parameter overrides active (shadow calibration). \
                         Review overrides vs base config if behaviour diverges from expectations.",
                        ca.asset, ca.drift_params
                    ),
                });
            }
        }
    }

    // --- MEDIUM: weak recent real performance (per asset) ---
    for a in &real_trades.per_asset {
        if a.count >= ASSET_PERF_MIN_TRADES && a.wr < ASSET_PERF_MAX_WR {
            recs.push(Recommendation {
                asset: Some(a.asset.clone()),
                priority: "MEDIUM",
                category: "asset_performance",
                message: format!(
                    "{}: recent real-trade WR {:.0}% over {} trades ({:.2} USDC PnL). \
                     Consider tightening filters or pausing this asset.",
                    a.asset,
                    a.wr * 100.0,
                    a.count,
                    a.pnl
                ),
            });
        }
    }

    // --- MEDIUM: shadow opportunity (per asset) ---
    for s in &shadows.per_asset {
        if s.count >= ASSET_SHADOW_MIN_COUNT
            && s.wr >= ASSET_SHADOW_MIN_WR
            && s.pnl > ASSET_SHADOW_MIN_PNL
        {
            recs.push(Recommendation {
                asset: Some(s.asset.clone()),
                priority: "MEDIUM",
                category: "asset_shadow_opportunity",
                message: format!(
                    "{}: shadow counterfactuals — {} resolved, {:.0}% WR, {:.2} USDC PnL. \
                     Filters may be blocking profitable setups on this asset.",
                    s.asset,
                    s.count,
                    s.wr * 100.0,
                    s.pnl
                ),
            });
        }
    }

    // --- MEDIUM: YES vs NO imbalance (per asset) ---
    for d in &real_trades.per_asset_direction {
        if d.yes_count >= DIR_MIN_EACH_SIDE && d.no_count >= DIR_MIN_EACH_SIDE {
            let gap = (d.yes_wr - d.no_wr).abs();
            if gap > DIR_WR_GAP {
                let weaker = if d.yes_wr < d.no_wr { "YES" } else { "NO" };
                recs.push(Recommendation {
                    asset: Some(d.asset.clone()),
                    priority: "MEDIUM",
                    category: "direction_imbalance",
                    message: format!(
                        "{}: YES {:.0}% WR (n={}) vs NO {:.0}% WR (n={}) — gap {:.0} pp. \
                         Weaker side recently: {}. Review direction penalties or `blocked_direction`.",
                        d.asset,
                        d.yes_wr * 100.0,
                        d.yes_count,
                        d.no_wr * 100.0,
                        d.no_count,
                        gap * 100.0,
                        weaker
                    ),
                });
            }
        }
    }

    // --- MEDIUM: shadow opportunity per reason (global) ---
    for reason_stat in &shadows.per_skip_reason {
        if reason_stat.count >= 5 && reason_stat.wr >= 0.70 && reason_stat.pnl > 0.0 {
            recs.push(Recommendation {
                asset: None,
                priority: "MEDIUM",
                category: "shadow_opportunity",
                message: format!(
                    "Shadow trades with skip_reason='{}': {} trades, {:.0}% WR, {:.2} USDC PnL. \
                     This filter may be blocking profitable opportunities.",
                    reason_stat.reason,
                    reason_stat.count,
                    reason_stat.wr * 100.0,
                    reason_stat.pnl
                ),
            });
        }
    }

    // --- MEDIUM: overall shadow performance (global) ---
    if shadows.total_resolved > 10 && shadows.overall_wr >= 0.65 && shadows.overall_pnl > 5.0 {
        recs.push(Recommendation {
            asset: None,
            priority: "MEDIUM",
            category: "shadow_overall",
            message: format!(
                "Overall shadow performance: {} resolved, {:.0}% WR, {:.2} USDC PnL. \
                 Filters are collectively blocking profitable trades.",
                shadows.total_resolved,
                shadows.overall_wr * 100.0,
                shadows.overall_pnl
            ),
        });
    }

    // --- MEDIUM: poor recent real WR (global) ---
    if real_trades.resolved >= 10 && real_trades.wr < 0.50 {
        recs.push(Recommendation {
            asset: None,
            priority: "MEDIUM",
            category: "performance",
            message: format!(
                "Recent real trade WR is low: {:.0}% over {} resolved trades ({:.2} USDC PnL). \
                 Strategy may need recalibration.",
                real_trades.wr * 100.0,
                real_trades.resolved,
                real_trades.pnl
            ),
        });
    }

    // --- INFO fallback ---
    if recs.is_empty() {
        recs.push(Recommendation {
            asset: None,
            priority: "INFO",
            category: "market",
            message: "No clear actionable issues found. Inactivity may be due to \
                      market conditions (low liquidity, few qualifying events)."
                .to_string(),
        });
    }

    recs
}

// ---------------------------------------------------------------------------
// Housekeeping: keep at most MAX_REPORTS report files
// ---------------------------------------------------------------------------

fn cleanup_old_reports(data_dir: &str) {
    let dir = match std::fs::read_dir(data_dir) {
        Ok(d) => d,
        Err(_) => return,
    };

    let mut reports: Vec<std::path::PathBuf> = dir
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("inactivity_report_") && n.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect();

    if reports.len() <= MAX_REPORTS {
        return;
    }

    reports.sort();
    let to_remove = reports.len() - MAX_REPORTS;
    for path in &reports[..to_remove] {
        if let Err(e) = std::fs::remove_file(path) {
            warn!(path = %path.display(), error = %e, "failed to clean up old inactivity report");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn balance_deadlock_detected_when_max_order_below_floor() {
        let mut strategies = HashMap::new();
        let st = crate::config::AppConfig::default().asset_strategy("btc");
        strategies.insert("btc".to_string(), st);

        let section = check_balance(dec!(10), &strategies);
        // balance=10, max_position_pct=0.05 (from default) → max_order=0.50
        // floor=2 → 0.50 < 2.0 → deadlock
        assert!(section.deadlock_detected);
    }

    #[test]
    fn no_deadlock_with_healthy_balance() {
        let mut strategies = HashMap::new();
        let st = crate::config::AppConfig::default().asset_strategy("btc");
        strategies.insert("btc".to_string(), st);

        let section = check_balance(dec!(549), &strategies);
        // balance=549, max_order=549*0.05=27.45, floor=2 → no deadlock
        assert!(!section.deadlock_detected);
    }

    fn empty_calibration() -> CalibrationSummary {
        CalibrationSummary {
            loaded: false,
            global_version: 0,
            per_asset: vec![],
        }
    }

    fn default_real_trades() -> RealTradePerformance {
        RealTradePerformance {
            lookback_hours: 24,
            total_trades: 0,
            resolved: 0,
            pending: 0,
            wr: 0.0,
            pnl: 0.0,
            per_asset: vec![],
            per_asset_direction: vec![],
            last_trade_at: None,
            hours_since_last_trade: None,
        }
    }

    fn default_balance_trend() -> BalanceTrend {
        BalanceTrend {
            data_points: vec![],
            peak_balance: 549.0,
            trough_balance: 549.0,
            current_drawdown_pct: 0.0,
        }
    }

    #[test]
    fn recommendations_empty_market_conditions() {
        let balance = BalanceSection {
            available_balance: "549".to_string(),
            deadlock_detected: false,
            max_order_usdc: "27.45".to_string(),
            min_order_usdc: "10".to_string(),
            min_order_usdc_floor: "2".to_string(),
            dynamic_min: "10".to_string(),
        };
        let skips = SkipAnalysis {
            total_skips: 5,
            reason_distribution: vec![ReasonCount { reason: "liquidity_too_low".to_string(), count: 5, pct: 100.0 }],
            top_blocker: Some("liquidity_too_low".to_string()),
            asset_distribution: vec![],
        };
        let shadows = ShadowAnalysis {
            total_resolved: 0,
            overall_wr: 0.0,
            overall_pnl: 0.0,
            per_skip_reason: vec![],
            per_asset: vec![],
        };

        let recs = build_recommendations(&balance, &skips, &shadows, &default_real_trades(), &default_balance_trend(), &empty_calibration());
        assert!(recs.iter().any(|r| r.category == "skip_filter"));
    }

    #[test]
    fn recommendations_deadlock_is_critical() {
        let balance = BalanceSection {
            available_balance: "10".to_string(),
            deadlock_detected: true,
            max_order_usdc: "0.50".to_string(),
            min_order_usdc: "10".to_string(),
            min_order_usdc_floor: "2".to_string(),
            dynamic_min: "2".to_string(),
        };
        let skips = SkipAnalysis {
            total_skips: 0,
            reason_distribution: vec![],
            top_blocker: None,
            asset_distribution: vec![],
        };
        let shadows = ShadowAnalysis {
            total_resolved: 0,
            overall_wr: 0.0,
            overall_pnl: 0.0,
            per_skip_reason: vec![],
            per_asset: vec![],
        };

        let recs = build_recommendations(&balance, &skips, &shadows, &default_real_trades(), &default_balance_trend(), &empty_calibration());
        assert!(recs.iter().any(|r| r.priority == "CRITICAL" && r.category == "balance"));
    }

    #[test]
    fn recommendations_drawdown_triggers_high() {
        let balance = BalanceSection {
            available_balance: "400".to_string(),
            deadlock_detected: false,
            max_order_usdc: "20".to_string(),
            min_order_usdc: "10".to_string(),
            min_order_usdc_floor: "2".to_string(),
            dynamic_min: "10".to_string(),
        };
        let skips = SkipAnalysis {
            total_skips: 0,
            reason_distribution: vec![],
            top_blocker: None,
            asset_distribution: vec![],
        };
        let shadows = ShadowAnalysis {
            total_resolved: 0,
            overall_wr: 0.0,
            overall_pnl: 0.0,
            per_skip_reason: vec![],
            per_asset: vec![],
        };
        let trend = BalanceTrend {
            data_points: vec![],
            peak_balance: 549.0,
            trough_balance: 380.0,
            current_drawdown_pct: 27.1,
        };

        let recs = build_recommendations(&balance, &skips, &shadows, &default_real_trades(), &trend, &empty_calibration());
        assert!(recs.iter().any(|r| r.priority == "HIGH" && r.category == "drawdown"));
    }

    #[test]
    fn recommendations_poor_wr_triggers_medium() {
        let balance = BalanceSection {
            available_balance: "549".to_string(),
            deadlock_detected: false,
            max_order_usdc: "27.45".to_string(),
            min_order_usdc: "10".to_string(),
            min_order_usdc_floor: "2".to_string(),
            dynamic_min: "10".to_string(),
        };
        let skips = SkipAnalysis {
            total_skips: 0,
            reason_distribution: vec![],
            top_blocker: None,
            asset_distribution: vec![],
        };
        let shadows = ShadowAnalysis {
            total_resolved: 0,
            overall_wr: 0.0,
            overall_pnl: 0.0,
            per_skip_reason: vec![],
            per_asset: vec![],
        };
        let real = RealTradePerformance {
            lookback_hours: 24,
            total_trades: 15,
            resolved: 15,
            pending: 0,
            wr: 0.33,
            pnl: -25.0,
            per_asset: vec![],
            per_asset_direction: vec![],
            last_trade_at: Some(Utc::now()),
            hours_since_last_trade: Some(0.5),
        };

        let recs = build_recommendations(&balance, &skips, &shadows, &real, &default_balance_trend(), &empty_calibration());
        assert!(recs.iter().any(|r| r.priority == "MEDIUM" && r.category == "performance"));
    }

    fn healthy_balance() -> BalanceSection {
        BalanceSection {
            available_balance: "549".to_string(),
            deadlock_detected: false,
            max_order_usdc: "27.45".to_string(),
            min_order_usdc: "10".to_string(),
            min_order_usdc_floor: "2".to_string(),
            dynamic_min: "10".to_string(),
        }
    }

    fn empty_skips() -> SkipAnalysis {
        SkipAnalysis {
            total_skips: 0,
            reason_distribution: vec![],
            top_blocker: None,
            asset_distribution: vec![],
        }
    }

    fn empty_shadows() -> ShadowAnalysis {
        ShadowAnalysis {
            total_resolved: 0,
            overall_wr: 0.0,
            overall_pnl: 0.0,
            per_skip_reason: vec![],
            per_asset: vec![],
        }
    }

    #[test]
    fn recommendations_per_asset_low_wr() {
        let real = RealTradePerformance {
            lookback_hours: 24,
            total_trades: 5,
            resolved: 5,
            pending: 0,
            wr: 0.5,
            pnl: 0.0,
            per_asset: vec![RealTradeAssetSummary {
                asset: "eth".to_string(),
                count: 4,
                wr: 0.40,
                pnl: -12.0,
            }],
            per_asset_direction: vec![],
            last_trade_at: None,
            hours_since_last_trade: None,
        };
        let recs = build_recommendations(
            &healthy_balance(),
            &empty_skips(),
            &empty_shadows(),
            &real,
            &default_balance_trend(),
            &empty_calibration(),
        );
        let r = recs
            .iter()
            .find(|r| r.category == "asset_performance")
            .expect("asset_performance");
        assert_eq!(r.asset.as_deref(), Some("eth"));
        assert_eq!(r.priority, "MEDIUM");
    }

    #[test]
    fn recommendations_per_asset_shadow_opportunity() {
        let shadows = ShadowAnalysis {
            total_resolved: 6,
            overall_wr: 0.0,
            overall_pnl: 0.0,
            per_skip_reason: vec![],
            per_asset: vec![ShadowAssetSummary {
                asset: "sol".to_string(),
                count: 6,
                wr: 0.70,
                pnl: 25.0,
            }],
        };
        let recs = build_recommendations(
            &healthy_balance(),
            &empty_skips(),
            &shadows,
            &default_real_trades(),
            &default_balance_trend(),
            &empty_calibration(),
        );
        let r = recs
            .iter()
            .find(|r| r.category == "asset_shadow_opportunity")
            .expect("asset_shadow_opportunity");
        assert_eq!(r.asset.as_deref(), Some("sol"));
    }

    #[test]
    fn recommendations_direction_imbalance() {
        let real = RealTradePerformance {
            lookback_hours: 24,
            total_trades: 10,
            resolved: 10,
            pending: 0,
            wr: 0.6,
            pnl: 1.0,
            per_asset: vec![],
            per_asset_direction: vec![RealTradeDirectionSummary {
                asset: "btc".to_string(),
                yes_count: 3,
                yes_wr: 0.30,
                no_count: 3,
                no_wr: 0.90,
            }],
            last_trade_at: None,
            hours_since_last_trade: None,
        };
        let recs = build_recommendations(
            &healthy_balance(),
            &empty_skips(),
            &empty_shadows(),
            &real,
            &default_balance_trend(),
            &empty_calibration(),
        );
        let r = recs
            .iter()
            .find(|r| r.category == "direction_imbalance")
            .expect("direction_imbalance");
        assert_eq!(r.asset.as_deref(), Some("btc"));
        assert!(r.message.contains("YES"));
        assert!(r.message.contains("NO"));
    }

    #[test]
    fn recommendations_calibration_drift_info() {
        let cal = CalibrationSummary {
            loaded: true,
            global_version: 1,
            per_asset: vec![CalibrationAssetSummary {
                asset: "xrp".to_string(),
                calibration_version: 2,
                shadow_wr: 0.5,
                shadow_pnl: 1.0,
                shadow_trade_count: 10,
                last_calibrated_at: "2025-01-01T00:00:00Z".to_string(),
                rolled_back: false,
                drift_params: 12,
            }],
        };
        let recs = build_recommendations(
            &healthy_balance(),
            &empty_skips(),
            &empty_shadows(),
            &default_real_trades(),
            &default_balance_trend(),
            &cal,
        );
        let r = recs
            .iter()
            .find(|r| r.category == "calibration_drift")
            .expect("calibration_drift");
        assert_eq!(r.asset.as_deref(), Some("xrp"));
        assert_eq!(r.priority, "INFO");
    }
}
