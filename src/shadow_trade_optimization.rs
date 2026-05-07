//! Shadow-trade–driven diagnostics: data-quality gate, opportunity scoring, tuning guidance.
//!
//! Consumed by [`crate::inactivity_diagnostics`] to extend `inactivity_report.json`. Does not
//! change runtime strategy parameters.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;

use crate::metrics::TradeRecord;
use crate::types::Direction;

/// Minimum resolved count for "high confidence" opportunity cells (plan: 8–10).
pub const MIN_SAMPLE_HIGH: usize = 8;
/// Minimum resolved count for "medium" tier when PnL is positive.
pub const MIN_SAMPLE_MEDIUM: usize = 5;
/// Win-rate threshold (fraction) for "high" opportunity tier.
pub const HIGH_OPPORTUNITY_MIN_WR: f64 = 0.55;

// ---------------------------------------------------------------------------
// Data quality gate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowExclusionReason {
    MissingOutcome,
    MissingPnl,
    MissingSkipReason,
    MissingAsset,
    MissingDuration,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExclusionCount {
    pub reason: ShadowExclusionReason,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShadowDataQuality {
    pub total_rows_scanned: usize,
    pub unresolved: usize,
    pub resolved: usize,
    pub eligible_for_analysis: usize,
    /// `eligible / resolved` when `resolved > 0`, else 0.
    pub eligible_pct_of_resolved: f64,
    /// `eligible / total_rows_scanned` when `total_rows_scanned > 0`, else 0.
    pub eligible_pct_of_total: f64,
    pub exclusion_breakdown: Vec<ExclusionCount>,
}

/// Classify a shadow row; first failing rule wins (stable breakdown).
pub fn classify_shadow_eligibility(t: &TradeRecord) -> Result<(), ShadowExclusionReason> {
    if t.outcome.is_none() {
        return Err(ShadowExclusionReason::MissingOutcome);
    }
    let pnl_ok = t
        .pnl
        .as_ref()
        .and_then(|p| p.parse::<f64>().ok())
        .is_some();
    if !pnl_ok {
        return Err(ShadowExclusionReason::MissingPnl);
    }
    let sr = t.skip_reason.as_deref().unwrap_or("").trim();
    if sr.is_empty() {
        return Err(ShadowExclusionReason::MissingSkipReason);
    }
    if t.asset.trim().is_empty() {
        return Err(ShadowExclusionReason::MissingAsset);
    }
    if t.duration.trim().is_empty() {
        return Err(ShadowExclusionReason::MissingDuration);
    }
    Ok(())
}

pub fn build_shadow_data_quality(trades: &[TradeRecord]) -> ShadowDataQuality {
    let total = trades.len();
    let mut unresolved = 0usize;
    let mut resolved = 0usize;
    let mut eligible = 0usize;
    let mut ex_map: HashMap<ShadowExclusionReason, usize> = HashMap::new();

    for t in trades {
        if t.outcome.is_none() {
            unresolved += 1;
            continue;
        }
        resolved += 1;
        match classify_shadow_eligibility(t) {
            Ok(()) => eligible += 1,
            Err(e) => *ex_map.entry(e).or_default() += 1,
        }
    }

    let mut breakdown: Vec<ExclusionCount> = ex_map
        .into_iter()
        .map(|(reason, count)| ExclusionCount { reason, count })
        .collect();
    breakdown.sort_by(|a, b| b.count.cmp(&a.count));

    let eligible_pct = if resolved > 0 {
        eligible as f64 / resolved as f64 * 100.0
    } else {
        0.0
    };
    let eligible_pct_total = if total > 0 {
        eligible as f64 / total as f64 * 100.0
    } else {
        0.0
    };

    ShadowDataQuality {
        total_rows_scanned: total,
        unresolved,
        resolved,
        eligible_for_analysis: eligible,
        eligible_pct_of_resolved: eligible_pct,
        eligible_pct_of_total: eligible_pct_total,
        exclusion_breakdown: breakdown,
    }
}

// ---------------------------------------------------------------------------
// Opportunity scoring (skip_reason × asset)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ShadowOpportunityCell {
    pub skip_reason: String,
    pub asset: String,
    pub count: usize,
    pub wr: f64,
    pub pnl: f64,
    pub avg_pnl: f64,
    /// `high` | `medium` | `low` | `insufficient_sample`
    pub opportunity_tier: &'static str,
    /// Heuristic rank key (higher = more interesting unlock candidate).
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShadowOpportunityBoard {
    pub lookback_hours: u64,
    pub cells: Vec<ShadowOpportunityCell>,
    pub min_sample_high: usize,
    pub min_sample_medium: usize,
}

fn shadow_won(t: &TradeRecord) -> bool {
    let Some(outcome) = t.outcome else {
        return false;
    };
    matches!(
        (t.direction, outcome),
        (Direction::Yes, true) | (Direction::No, false)
    )
}

fn opportunity_tier_and_score(count: usize, wr: f64, pnl: f64) -> (&'static str, f64) {
    if count < MIN_SAMPLE_MEDIUM {
        return ("insufficient_sample", 0.0);
    }
    let score = if count >= MIN_SAMPLE_HIGH {
        pnl * (wr - 0.5).max(0.01) * (count as f64).sqrt()
    } else {
        pnl * 0.5 * (count as f64).sqrt()
    };

    if count >= MIN_SAMPLE_HIGH && pnl > 0.0 && wr > HIGH_OPPORTUNITY_MIN_WR {
        ("high", score)
    } else if pnl > 0.0 && count >= MIN_SAMPLE_MEDIUM {
        ("medium", score)
    } else {
        ("low", score)
    }
}

/// Eligible resolved trades in `[cutoff, now)` grouped by `(skip_reason, asset)`.
pub fn build_opportunity_board(
    trades: &[TradeRecord],
    cutoff: DateTime<Utc>,
    lookback_hours: u64,
) -> ShadowOpportunityBoard {
    type Key = (String, String);
    let mut groups: HashMap<Key, Vec<&TradeRecord>> = HashMap::new();

    for t in trades {
        if t.timestamp < cutoff {
            continue;
        }
        if classify_shadow_eligibility(t).is_err() {
            continue;
        }
        let reason = t.skip_reason.as_deref().unwrap_or("unknown").trim().to_string();
        let key = (reason, t.asset.clone());
        groups.entry(key).or_default().push(t);
    }

    let mut cells: Vec<ShadowOpportunityCell> = groups
        .into_iter()
        .map(|((skip_reason, asset), ts)| {
            let count = ts.len();
            let wins = ts.iter().filter(|t| shadow_won(t)).count();
            let wr = if count == 0 {
                0.0
            } else {
                wins as f64 / count as f64
            };
            let pnl: f64 = ts
                .iter()
                .filter_map(|t| t.pnl.as_ref()?.parse::<f64>().ok())
                .sum();
            let avg_pnl = if count == 0 { 0.0 } else { pnl / count as f64 };
            let (tier, score) = opportunity_tier_and_score(count, wr, pnl);
            ShadowOpportunityCell {
                skip_reason,
                asset,
                count,
                wr,
                pnl,
                avg_pnl,
                opportunity_tier: tier,
                score,
            }
        })
        .collect();

    cells.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    ShadowOpportunityBoard {
        lookback_hours,
        cells,
        min_sample_high: MIN_SAMPLE_HIGH,
        min_sample_medium: MIN_SAMPLE_MEDIUM,
    }
}

// ---------------------------------------------------------------------------
// Filter → parameter mapping (documentation for operators)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct FilterParamMapping {
    pub skip_reason: String,
    pub config_parameters: Vec<String>,
    pub notes: String,
}

pub fn filter_param_mapping_matrix() -> Vec<FilterParamMapping> {
    vec![
        FilterParamMapping {
            skip_reason: "liquidity_too_low".to_string(),
            config_parameters: vec![
                "min_liquidity (global / per-asset)".to_string(),
                "[liquidity_adapt]".to_string(),
            ],
            notes: "Lower effective min_liquidity widens tradable books; use per-asset or adapt module to limit risk.".to_string(),
        },
        FilterParamMapping {
            skip_reason: "market_yes_price_out_of_band".to_string(),
            config_parameters: vec![
                "min_market_yes_price".to_string(),
                "max_market_yes_price".to_string(),
            ],
            notes: "Widen band only where shadow shows persistent profitability at skipped prices.".to_string(),
        },
        FilterParamMapping {
            skip_reason: "too_far_from_expiry".to_string(),
            config_parameters: vec!["max_secs_to_close".to_string()],
            notes: "Larger max_secs allows earlier entries; increases time/mark-to-market risk.".to_string(),
        },
        FilterParamMapping {
            skip_reason: "too_close_to_expiry".to_string(),
            config_parameters: vec!["min_secs_to_close".to_string()],
            notes: "Lower min_secs trades closer to resolution; slippage and resolution risk rise.".to_string(),
        },
        FilterParamMapping {
            skip_reason: "spot_volume_below_threshold".to_string(),
            config_parameters: vec!["volume_min_ratio (per-asset or global)".to_string()],
            notes: "Lower ratio admits thinner spot tape confirmation; do per-asset first.".to_string(),
        },
        FilterParamMapping {
            skip_reason: "edge_too_small".to_string(),
            config_parameters: vec![
                "min_edge".to_string(),
                "[adaptive] multipliers".to_string(),
            ],
            notes: "Lowering edge boosts frequency; combine with shadow/live WR guardrails.".to_string(),
        },
        FilterParamMapping {
            skip_reason: "confidence_too_low".to_string(),
            config_parameters: vec!["min_confidence".to_string()],
            notes: "Lowering min_confidence increases false positives; prefer direction penalties first.".to_string(),
        },
        FilterParamMapping {
            skip_reason: "macd_histogram_too_weak".to_string(),
            config_parameters: vec!["min_macd_histogram_abs".to_string()],
            notes: "Smaller threshold weakens momentum filter.".to_string(),
        },
        FilterParamMapping {
            skip_reason: "momentum_5m_too_weak".to_string(),
            config_parameters: vec!["min_momentum_5m_abs".to_string()],
            notes: "Lower threshold admits weaker short-term drift.".to_string(),
        },
        FilterParamMapping {
            skip_reason: "volatility_filter".to_string(),
            config_parameters: vec!["[volatility]".to_string()],
            notes: "Relax volatility caps only after checking worst-case drawdown in shadow.".to_string(),
        },
        FilterParamMapping {
            skip_reason: "taker_direction_misaligned".to_string(),
            config_parameters: vec!["taker_direction_confirm".to_string()],
            notes: "Disabling confirm loosens flow alignment requirement.".to_string(),
        },
        FilterParamMapping {
            skip_reason: "htf_trend_mismatch".to_string(),
            config_parameters: vec!["htf_enabled".to_string()],
            notes: "Turning HTF off or narrowing lookback changes regime filter.".to_string(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Tuning packages (conservative / medium / aggressive)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct TuningPackage {
    pub id: &'static str,
    pub label: &'static str,
    pub parameter_changes: Vec<String>,
    pub expected_effect: String,
    pub risk_notes: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TuningPackagesReport {
    pub packages: Vec<TuningPackage>,
}

pub fn tuning_packages() -> TuningPackagesReport {
    TuningPackagesReport {
        packages: vec![
            TuningPackage {
                id: "conservative",
                label: "Conservative — 1–2 knobs, asset-scoped",
                parameter_changes: vec![
                    "Pick highest-score (reason, asset) from shadow_opportunity_board with tier=high."
                        .to_string(),
                    "Adjust only the parameters listed in filter_param_mapping for that skip_reason."
                        .to_string(),
                    "Prefer per-asset overrides in config over global strategy section.".to_string(),
                ],
                expected_effect: "Modest increase in trade frequency; attribution stays clear.".to_string(),
                risk_notes: "If live WR drops, revert single asset block first.".to_string(),
            },
            TuningPackage {
                id: "medium",
                label: "Medium — 2 filters, still asset-scoped",
                parameter_changes: vec![
                    "Combine (e.g.) liquidity_too_low + market_yes_price_out_of_band for one asset with strong shadow PnL."
                        .to_string(),
                    "Keep [shadow_calibration] live veto on; avoid simultaneous global changes.".to_string(),
                ],
                expected_effect: "Noticeable frequency lift; slightly harder to attribute.".to_string(),
                risk_notes: "Watch order_size_below_minimum and slippage; roll back the second change first.".to_string(),
            },
            TuningPackage {
                id: "aggressive",
                label: "Aggressive — global + multiple filters",
                parameter_changes: vec![
                    "Global min_liquidity, wider YES band, and looser volume — only after 24h conservative canary passes."
                        .to_string(),
                    "Bump strategy_version to reset calibration if strategy semantics change materially."
                        .to_string(),
                ],
                expected_effect: "High activity; highest tail risk.".to_string(),
                risk_notes: "Requires strict drawdown stop and immediate rollback path; not recommended without canary."
                    .to_string(),
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// Canary playbook
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct CanaryPlaybook {
    pub duration_hours: u64,
    pub success_metrics: Vec<String>,
    pub rollback_rules: Vec<String>,
    pub monitoring_checklist: Vec<String>,
}

pub fn canary_playbook() -> CanaryPlaybook {
    CanaryPlaybook {
        duration_hours: 24,
        success_metrics: vec![
            "Realized trade count vs baseline (same clock hours).".to_string(),
            "Rolling live WR and total PnL not worse than pre-change by more than agreed tolerance (e.g. -10% WR or negative PnL spike)."
                .to_string(),
            "No explosion of new dominant skip_reasons indicating unintended loosening (e.g. bad fills)."
                .to_string(),
        ],
        rollback_rules: vec![
            "Revert last config change and bump strategy_version if calibration state must reset."
                .to_string(),
            "If daily_loss or risk caps trip, stop canary and restore previous config.toml snapshot."
                .to_string(),
            "If shadow_opportunity_board flips to negative PnL for the tuned (reason, asset), tighten before waiting full 24h."
                .to_string(),
        ],
        monitoring_checklist: vec![
            "Tail logs for skip reasons hourly; compare to inactivity_report skip_analysis.".to_string(),
            "Verify calibration_state.json only drifts as expected (shadow_calibration + liquidity_adapt)."
                .to_string(),
            "Check balance_state / drawdown vs peak after first 6h.".to_string(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    fn sample_trade(
        ts: DateTime<Utc>,
        asset: &str,
        reason: &str,
        outcome: Option<bool>,
        pnl: Option<&str>,
    ) -> TradeRecord {
        let mut t = TradeRecord::new(
            "0x1".into(),
            asset.into(),
            "15m".into(),
            Direction::Yes,
            dec!(0.5),
            dec!(5),
            dec!(10),
            dec!(0.6),
            dec!(0.8),
            dec!(0.1),
            "test".into(),
            "o1".into(),
        );
        t.timestamp = ts;
        t.outcome = outcome;
        t.pnl = pnl.map(String::from);
        t.skip_reason = Some(reason.into());
        t
    }

    #[test]
    fn eligibility_requires_outcome_pnl_reason_asset_duration() {
        let t0 = Utc.with_ymd_and_hms(2026, 5, 5, 12, 0, 0).unwrap();
        assert!(classify_shadow_eligibility(&sample_trade(t0, "eth", "edge_too_small", None, None)).is_err());
        assert!(classify_shadow_eligibility(&sample_trade(t0, "eth", "edge_too_small", Some(true), None)).is_err());
        assert!(classify_shadow_eligibility(&sample_trade(t0, "eth", "", Some(true), Some("5"))).is_err());
        assert!(classify_shadow_eligibility(&sample_trade(t0, "", "edge_too_small", Some(true), Some("5"))).is_err());
        let mut missing_dur = sample_trade(t0, "eth", "edge_too_small", Some(true), Some("5"));
        missing_dur.duration = "".into();
        assert!(classify_shadow_eligibility(&missing_dur).is_err());
        assert!(classify_shadow_eligibility(&sample_trade(t0, "eth", "edge_too_small", Some(true), Some("5"))).is_ok());
    }

    #[test]
    fn opportunity_board_ranks_positive_pnl() {
        let t0 = Utc.with_ymd_and_hms(2026, 5, 5, 12, 0, 0).unwrap();
        let cutoff = t0 - chrono::Duration::hours(24);
        let mut trades = Vec::new();
        for i in 0..10 {
            trades.push(sample_trade(
                t0 + chrono::Duration::seconds(i),
                "eth",
                "edge_too_small",
                Some(true),
                Some("3"),
            ));
        }
        trades.push(sample_trade(t0, "xrp", "liquidity_too_low", Some(true), Some("2")));
        let board = build_opportunity_board(&trades, cutoff, 24);
        assert!(!board.cells.is_empty());
        let top = &board.cells[0];
        assert_eq!(top.skip_reason, "edge_too_small");
        assert_eq!(top.asset, "eth");
        assert!(top.count >= MIN_SAMPLE_HIGH);
    }
}
