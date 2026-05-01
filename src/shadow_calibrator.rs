//! Shadow-to-live feedback loop: calibrate `AssetStrategy` parameters from resolved shadow trades.
//!
//! Uses three calibration strategies:
//! - **WR-driven**: adjust thresholds based on asset-level win rate
//! - **Percentile-driven**: set filter values from winner/loser trade distributions
//! - **Bool-toggle (WR-gated)**: flip feature flags when segment WR difference exceeds threshold

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::AssetStrategy;
use crate::metrics::{read_trades_from_path, TradeRecord};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ShadowCalibrationConfig {
    pub enabled: bool,
    pub min_trades: usize,
    pub cooldown_secs: u64,
    pub min_pnl_delta: Decimal,
    pub max_step_pct: f64,
    pub safety_bound_low: f64,
    pub safety_bound_high: f64,
    pub rollback_window: usize,
    pub rollback_threshold: f64,
    pub bool_toggle_min_trades: usize,
    pub exclude_params: Vec<String>,
    /// When true, strip loosening calibration deltas when live trades look poor (see `live_veto_*`).
    pub live_veto_enabled: bool,
    /// How many most recent resolved live trades to include per asset.
    pub live_veto_window: usize,
    /// Minimum resolved live trades required before live veto can apply.
    pub live_veto_min_trades: usize,
    /// Win-rate threshold (exclusive): live WR below this contributes to veto / loosening strip.
    pub live_veto_wr_threshold: f64,
    /// Strip loosening proposals when live aggregate WR is below this (e.g. shadow says loosen but live is weak).
    pub live_veto_soft_wr: f64,
    /// Sum PnL below this (USDC, typically negative) triggers loosening strip together with min trade count.
    pub live_veto_pnl_threshold: f64,
    /// Per-direction live WR below this blocks penalty *decreases* on that side when enough trades exist.
    pub live_direction_veto_wr: f64,
}

impl Default for ShadowCalibrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_trades: 20,
            cooldown_secs: 3600,
            min_pnl_delta: dec!(10),
            max_step_pct: 0.20,
            safety_bound_low: 0.50,
            safety_bound_high: 2.0,
            rollback_window: 10,
            rollback_threshold: 0.30,
            bool_toggle_min_trades: 30,
            exclude_params: Vec::new(),
            live_veto_enabled: true,
            live_veto_window: 10,
            live_veto_min_trades: 3,
            live_veto_wr_threshold: 0.40,
            live_veto_soft_wr: 0.50,
            live_veto_pnl_threshold: -10.0,
            live_direction_veto_wr: 0.30,
        }
    }
}

// ---------------------------------------------------------------------------
// Calibrated overrides (applied on top of base AssetStrategy)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CalibratedOverrides {
    pub min_edge: Option<f64>,
    pub min_confidence: Option<f64>,
    pub yes_confidence_penalty: Option<f64>,
    pub no_confidence_penalty: Option<f64>,
    pub cluster_down_confidence_add: Option<f64>,
    pub min_macd_histogram_abs: Option<f64>,
    pub volume_min_ratio: Option<f64>,
    pub min_momentum_5m_abs: Option<f64>,
    pub momentum_vol_reference: Option<f64>,
    pub rsi_yes_max: Option<f64>,
    pub rsi_no_min: Option<f64>,
    pub cluster_rsi_oversold: Option<f64>,
    pub cluster_rsi_overbought: Option<f64>,
    pub min_secs_to_close: Option<i64>,
    pub max_secs_to_close: Option<i64>,
    pub cheap_token_price_threshold: Option<f64>,
    pub cluster_tie_min_edge_multiplier: Option<f64>,
    pub neutral_taker_edge_multiplier: Option<f64>,
    pub mid_price_band_min_edge_multiplier: Option<f64>,
    pub taker_yes_min_ratio: Option<f64>,
    pub taker_no_max_ratio: Option<f64>,
    pub taker_neutral_low: Option<f64>,
    pub taker_neutral_high: Option<f64>,
    pub taker_direction_confirm: Option<bool>,
    pub htf_enabled: Option<bool>,
    pub dynamic_momentum_threshold: Option<bool>,
    pub multi_tf_enabled: Option<bool>,
}

impl CalibratedOverrides {
    /// Merge `other` into `self`: only overwrite fields where `other` has `Some`.
    /// Fields that are `None` in `other` retain their current value in `self`.
    pub fn merge_from(&mut self, other: &CalibratedOverrides) {
        macro_rules! merge_opt {
            ($field:ident) => {
                if other.$field.is_some() {
                    self.$field = other.$field;
                }
            };
        }
        merge_opt!(min_edge);
        merge_opt!(min_confidence);
        merge_opt!(yes_confidence_penalty);
        merge_opt!(no_confidence_penalty);
        merge_opt!(cluster_down_confidence_add);
        merge_opt!(min_macd_histogram_abs);
        merge_opt!(volume_min_ratio);
        merge_opt!(min_momentum_5m_abs);
        merge_opt!(momentum_vol_reference);
        merge_opt!(rsi_yes_max);
        merge_opt!(rsi_no_min);
        merge_opt!(cluster_rsi_oversold);
        merge_opt!(cluster_rsi_overbought);
        merge_opt!(min_secs_to_close);
        merge_opt!(max_secs_to_close);
        merge_opt!(cheap_token_price_threshold);
        merge_opt!(cluster_tie_min_edge_multiplier);
        merge_opt!(neutral_taker_edge_multiplier);
        merge_opt!(mid_price_band_min_edge_multiplier);
        merge_opt!(taker_yes_min_ratio);
        merge_opt!(taker_no_max_ratio);
        merge_opt!(taker_neutral_low);
        merge_opt!(taker_neutral_high);
        merge_opt!(taker_direction_confirm);
        merge_opt!(htf_enabled);
        merge_opt!(dynamic_momentum_threshold);
        merge_opt!(multi_tf_enabled);
    }

    pub fn is_empty(&self) -> bool {
        self.min_edge.is_none()
            && self.min_confidence.is_none()
            && self.yes_confidence_penalty.is_none()
            && self.no_confidence_penalty.is_none()
            && self.cluster_down_confidence_add.is_none()
            && self.min_macd_histogram_abs.is_none()
            && self.volume_min_ratio.is_none()
            && self.min_momentum_5m_abs.is_none()
            && self.momentum_vol_reference.is_none()
            && self.rsi_yes_max.is_none()
            && self.rsi_no_min.is_none()
            && self.cluster_rsi_oversold.is_none()
            && self.cluster_rsi_overbought.is_none()
            && self.min_secs_to_close.is_none()
            && self.max_secs_to_close.is_none()
            && self.cheap_token_price_threshold.is_none()
            && self.cluster_tie_min_edge_multiplier.is_none()
            && self.neutral_taker_edge_multiplier.is_none()
            && self.mid_price_band_min_edge_multiplier.is_none()
            && self.taker_yes_min_ratio.is_none()
            && self.taker_no_max_ratio.is_none()
            && self.taker_neutral_low.is_none()
            && self.taker_neutral_high.is_none()
            && self.taker_direction_confirm.is_none()
            && self.htf_enabled.is_none()
            && self.dynamic_momentum_threshold.is_none()
            && self.multi_tf_enabled.is_none()
    }
}

/// Apply calibrated overrides onto a base `AssetStrategy`, returning the modified copy.
pub fn apply_overrides(base: &mut AssetStrategy, ov: &CalibratedOverrides) {
    if let Some(v) = ov.min_edge {
        base.min_edge = Decimal::from_f64_retain(v).unwrap_or(base.min_edge);
    }
    if let Some(v) = ov.min_confidence {
        base.min_confidence = Decimal::from_f64_retain(v).unwrap_or(base.min_confidence);
    }
    if let Some(v) = ov.yes_confidence_penalty {
        base.yes_confidence_penalty = v;
    }
    if let Some(v) = ov.no_confidence_penalty {
        base.no_confidence_penalty = v;
    }
    if let Some(v) = ov.cluster_down_confidence_add {
        base.cluster_down_confidence_add = v;
    }
    if let Some(v) = ov.min_macd_histogram_abs {
        base.min_macd_histogram_abs = v;
    }
    if let Some(v) = ov.volume_min_ratio {
        base.volume_min_ratio = Some(v);
    }
    if let Some(v) = ov.min_momentum_5m_abs {
        base.min_momentum_5m_abs = v;
    }
    if let Some(v) = ov.momentum_vol_reference {
        base.momentum_vol_reference = v;
    }
    if let Some(v) = ov.rsi_yes_max {
        base.rsi_yes_max = v;
    }
    if let Some(v) = ov.rsi_no_min {
        base.rsi_no_min = v;
    }
    if let Some(v) = ov.cluster_rsi_oversold {
        base.cluster_rsi_oversold = v;
    }
    if let Some(v) = ov.cluster_rsi_overbought {
        base.cluster_rsi_overbought = v;
    }
    if let Some(v) = ov.min_secs_to_close {
        base.min_secs_to_close = Some(v);
    }
    if let Some(v) = ov.max_secs_to_close {
        base.max_secs_to_close = Some(v);
    }
    if let Some(v) = ov.cheap_token_price_threshold {
        base.cheap_token_price_threshold = Decimal::from_f64_retain(v).unwrap_or(base.cheap_token_price_threshold);
    }
    if let Some(v) = ov.cluster_tie_min_edge_multiplier {
        base.cluster_tie_min_edge_multiplier = v;
    }
    if let Some(v) = ov.neutral_taker_edge_multiplier {
        base.neutral_taker_edge_multiplier = v;
    }
    if let Some(v) = ov.mid_price_band_min_edge_multiplier {
        base.mid_price_band_min_edge_multiplier = v;
    }
    if let Some(v) = ov.taker_yes_min_ratio {
        base.taker_yes_min_ratio = v;
    }
    if let Some(v) = ov.taker_no_max_ratio {
        base.taker_no_max_ratio = v;
    }
    if let Some(v) = ov.taker_neutral_low {
        base.taker_neutral_low = v;
    }
    if let Some(v) = ov.taker_neutral_high {
        base.taker_neutral_high = v;
    }
    if let Some(v) = ov.taker_direction_confirm {
        base.taker_direction_confirm = v;
    }
    if let Some(v) = ov.htf_enabled {
        base.htf_enabled = v;
    }
    if let Some(v) = ov.dynamic_momentum_threshold {
        base.dynamic_momentum_threshold = v;
    }
    if let Some(v) = ov.multi_tf_enabled {
        base.multi_tf_enabled = v;
    }
}

// ---------------------------------------------------------------------------
// Calibration state (persisted to calibration_state.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetCalibrationState {
    pub last_calibrated_at: DateTime<Utc>,
    pub shadow_wr: f64,
    pub shadow_pnl: f64,
    pub shadow_trade_count: usize,
    pub calibration_version: u64,
    pub applied_overrides: CalibratedOverrides,
    pub base_snapshot: BaseSnapshot,
    pub trade_count_since_calibration: usize,
    pub rolled_back: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseSnapshot {
    pub min_edge: f64,
    pub min_confidence: f64,
    pub yes_confidence_penalty: f64,
    pub no_confidence_penalty: f64,
    pub cluster_down_confidence_add: f64,
    pub min_macd_histogram_abs: f64,
    pub volume_min_ratio: Option<f64>,
    pub min_momentum_5m_abs: f64,
    pub momentum_vol_reference: f64,
    pub rsi_yes_max: f64,
    pub rsi_no_min: f64,
    pub cluster_rsi_oversold: f64,
    pub cluster_rsi_overbought: f64,
    pub min_secs_to_close: Option<i64>,
    pub max_secs_to_close: Option<i64>,
    pub cheap_token_price_threshold: f64,
    pub cluster_tie_min_edge_multiplier: f64,
    pub neutral_taker_edge_multiplier: f64,
    pub mid_price_band_min_edge_multiplier: f64,
    pub taker_yes_min_ratio: f64,
    pub taker_no_max_ratio: f64,
    pub taker_neutral_low: f64,
    pub taker_neutral_high: f64,
    pub taker_direction_confirm: bool,
    pub htf_enabled: bool,
    pub dynamic_momentum_threshold: bool,
    pub multi_tf_enabled: bool,
}

impl BaseSnapshot {
    pub fn from_strategy(st: &AssetStrategy) -> Self {
        Self {
            min_edge: st.min_edge.to_f64().unwrap_or(0.06),
            min_confidence: st.min_confidence.to_f64().unwrap_or(0.70),
            yes_confidence_penalty: st.yes_confidence_penalty,
            no_confidence_penalty: st.no_confidence_penalty,
            cluster_down_confidence_add: st.cluster_down_confidence_add,
            min_macd_histogram_abs: st.min_macd_histogram_abs,
            volume_min_ratio: st.volume_min_ratio,
            min_momentum_5m_abs: st.min_momentum_5m_abs,
            momentum_vol_reference: st.momentum_vol_reference,
            rsi_yes_max: st.rsi_yes_max,
            rsi_no_min: st.rsi_no_min,
            cluster_rsi_oversold: st.cluster_rsi_oversold,
            cluster_rsi_overbought: st.cluster_rsi_overbought,
            min_secs_to_close: st.min_secs_to_close,
            max_secs_to_close: st.max_secs_to_close,
            cheap_token_price_threshold: st.cheap_token_price_threshold.to_f64().unwrap_or(0.15),
            cluster_tie_min_edge_multiplier: st.cluster_tie_min_edge_multiplier,
            neutral_taker_edge_multiplier: st.neutral_taker_edge_multiplier,
            mid_price_band_min_edge_multiplier: st.mid_price_band_min_edge_multiplier,
            taker_yes_min_ratio: st.taker_yes_min_ratio,
            taker_no_max_ratio: st.taker_no_max_ratio,
            taker_neutral_low: st.taker_neutral_low,
            taker_neutral_high: st.taker_neutral_high,
            taker_direction_confirm: st.taker_direction_confirm,
            htf_enabled: st.htf_enabled,
            dynamic_momentum_threshold: st.dynamic_momentum_threshold,
            multi_tf_enabled: st.multi_tf_enabled,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CalibrationStateFile {
    pub assets: HashMap<String, AssetCalibrationState>,
    pub global_version: u64,
}

// ---------------------------------------------------------------------------
// Shadow asset statistics
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct PercentileSet {
    pub p10: f64,
    pub p25: f64,
    pub p50: f64,
    pub p75: f64,
    pub p90: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ShadowAssetStats {
    pub wr: f64,
    pub wr_yes: f64,
    pub wr_no: f64,
    pub pnl: f64,
    pub trade_count: usize,
    pub yes_count: usize,
    pub no_count: usize,
    pub percentiles: HashMap<String, PercentileSet>,
    pub segment_wrs: HashMap<String, f64>,
    pub segment_counts: HashMap<String, usize>,
}

fn trade_won(t: &TradeRecord) -> bool {
    let Some(outcome) = t.outcome else {
        return false;
    };
    match (t.direction.as_str(), outcome) {
        ("YES", true) | ("NO", false) => true,
        _ => false,
    }
}

fn resolved_time(t: &TradeRecord) -> DateTime<Utc> {
    t.resolved_at.unwrap_or(t.timestamp)
}

/// Aggregate live performance for one asset from [`trades.jsonl`] (last `window` resolved trades).
#[derive(Debug, Clone)]
pub struct LiveAssetGate {
    pub wr: f64,
    pub pnl: f64,
    pub count: usize,
}

/// Per-direction live win rates over the same recent window as [`LiveAssetGate`].
#[derive(Debug, Clone)]
pub struct LiveDirectionStats {
    pub yes_wr: f64,
    pub no_wr: f64,
    pub yes_count: usize,
    pub no_count: usize,
}

/// Win rate and sum PnL for the last `window` resolved live trades on `asset`.
pub fn compute_live_asset_stats(trades_path: &str, asset: &str, window: usize) -> Option<LiveAssetGate> {
    let trades = read_trades_from_path(trades_path).ok()?;
    let mut resolved: Vec<&TradeRecord> = trades
        .iter()
        .filter(|t| t.asset == asset && t.outcome.is_some())
        .collect();
    if resolved.is_empty() {
        return Some(LiveAssetGate {
            wr: 0.0,
            pnl: 0.0,
            count: 0,
        });
    }
    resolved.sort_by(|a, b| resolved_time(a).cmp(&resolved_time(b)));
    let take = window.min(resolved.len());
    let slice = &resolved[resolved.len() - take..];
    let wins = slice.iter().filter(|t| trade_won(t)).count();
    let wr = wins as f64 / slice.len() as f64;
    let pnl: f64 = slice
        .iter()
        .filter_map(|t| t.pnl.as_ref()?.parse::<f64>().ok())
        .sum();
    Some(LiveAssetGate {
        wr,
        pnl,
        count: slice.len(),
    })
}

/// YES vs NO live win rates for the last `window` resolved trades on `asset`.
pub fn compute_live_direction_stats(trades_path: &str, asset: &str, window: usize) -> Option<LiveDirectionStats> {
    let trades = read_trades_from_path(trades_path).ok()?;
    let mut resolved: Vec<&TradeRecord> = trades
        .iter()
        .filter(|t| t.asset == asset && t.outcome.is_some())
        .collect();
    if resolved.is_empty() {
        return Some(LiveDirectionStats {
            yes_wr: 0.0,
            no_wr: 0.0,
            yes_count: 0,
            no_count: 0,
        });
    }
    resolved.sort_by(|a, b| resolved_time(a).cmp(&resolved_time(b)));
    let take = window.min(resolved.len());
    let slice = &resolved[resolved.len() - take..];
    let yes: Vec<_> = slice.iter().filter(|t| t.direction == "YES").copied().collect();
    let no: Vec<_> = slice.iter().filter(|t| t.direction == "NO").copied().collect();
    let yes_wins = yes.iter().filter(|t| trade_won(t)).count();
    let no_wins = no.iter().filter(|t| trade_won(t)).count();
    let yes_wr = if yes.is_empty() {
        0.0
    } else {
        yes_wins as f64 / yes.len() as f64
    };
    let no_wr = if no.is_empty() {
        0.0
    } else {
        no_wins as f64 / no.len() as f64
    };
    Some(LiveDirectionStats {
        yes_wr,
        no_wr,
        yes_count: yes.len(),
        no_count: no.len(),
    })
}

fn live_strip_loosening_trigger(live: &LiveAssetGate, cfg: &ShadowCalibrationConfig) -> bool {
    if live.count < cfg.live_veto_min_trades {
        return false;
    }
    if live.pnl < cfg.live_veto_pnl_threshold {
        return true;
    }
    live.wr < cfg.live_veto_soft_wr
}

/// High-severity live stress (for mismatch alarms): poor WR or deep loss with enough samples.
fn live_metrics_stress(live: &LiveAssetGate, cfg: &ShadowCalibrationConfig) -> bool {
    live.count >= cfg.live_veto_min_trades
        && (live.wr < cfg.live_veto_wr_threshold || live.pnl < cfg.live_veto_pnl_threshold)
}

fn classify_override_direction(
    delta: &CalibratedOverrides,
    current: &CalibratedOverrides,
    base: &BaseSnapshot,
) -> &'static str {
    let mut loosen = 0usize;
    let mut tighten = 0usize;
    let mut consider = |loosens: bool| {
        if loosens {
            loosen += 1;
        } else {
            tighten += 1;
        }
    };

    let cur_edge = current.min_edge.unwrap_or(base.min_edge);
    let cur_conf = current.min_confidence.unwrap_or(base.min_confidence);
    if let Some(v) = delta.min_edge {
        consider(v < cur_edge - 1e-12);
    }
    if let Some(v) = delta.min_confidence {
        consider(v < cur_conf - 1e-12);
    }
    if let Some(v) = delta.yes_confidence_penalty {
        let cur = current.yes_confidence_penalty.unwrap_or(base.yes_confidence_penalty);
        consider(v < cur - 1e-12);
    }
    if let Some(v) = delta.no_confidence_penalty {
        let cur = current.no_confidence_penalty.unwrap_or(base.no_confidence_penalty);
        consider(v < cur - 1e-12);
    }
    if let Some(v) = delta.cluster_down_confidence_add {
        let cur = current.cluster_down_confidence_add.unwrap_or(base.cluster_down_confidence_add);
        consider(v < cur - 1e-12);
    }
    if let Some(v) = delta.min_macd_histogram_abs {
        let cur = current.min_macd_histogram_abs.unwrap_or(base.min_macd_histogram_abs);
        consider(v < cur - 1e-12);
    }
    if let Some(v) = delta.volume_min_ratio {
        let cur = current.volume_min_ratio.unwrap_or(base.volume_min_ratio.unwrap_or(0.0));
        consider(v < cur - 1e-12);
    }
    if let Some(v) = delta.min_momentum_5m_abs {
        let cur = current.min_momentum_5m_abs.unwrap_or(base.min_momentum_5m_abs);
        consider(v < cur - 1e-12);
    }
    if let Some(v) = delta.cluster_tie_min_edge_multiplier {
        let cur = current.cluster_tie_min_edge_multiplier.unwrap_or(base.cluster_tie_min_edge_multiplier);
        consider(v < cur - 1e-12);
    }
    if let Some(v) = delta.neutral_taker_edge_multiplier {
        let cur = current.neutral_taker_edge_multiplier.unwrap_or(base.neutral_taker_edge_multiplier);
        consider(v < cur - 1e-12);
    }
    if let Some(v) = delta.mid_price_band_min_edge_multiplier {
        let cur = current.mid_price_band_min_edge_multiplier.unwrap_or(base.mid_price_band_min_edge_multiplier);
        consider(v < cur - 1e-12);
    }
    if let Some(v) = delta.rsi_yes_max {
        let cur = current.rsi_yes_max.unwrap_or(base.rsi_yes_max);
        consider(v > cur + 1e-12);
    }
    if let Some(v) = delta.rsi_no_min {
        let cur = current.rsi_no_min.unwrap_or(base.rsi_no_min);
        consider(v < cur - 1e-12);
    }
    if let Some(v) = delta.cluster_rsi_oversold {
        let cur = current.cluster_rsi_oversold.unwrap_or(base.cluster_rsi_oversold);
        consider(v > cur + 1e-12); // raising widens oversold zone = loosening
    }
    if let Some(v) = delta.cluster_rsi_overbought {
        let cur = current.cluster_rsi_overbought.unwrap_or(base.cluster_rsi_overbought);
        consider(v < cur - 1e-12); // lowering widens overbought zone = loosening
    }
    if let Some(v) = delta.momentum_vol_reference {
        let cur = current.momentum_vol_reference.unwrap_or(base.momentum_vol_reference);
        consider(v > cur + 1e-12); // denominator: raising lowers effective threshold = loosening
    }
    if let Some(v) = delta.taker_yes_min_ratio {
        let cur = current.taker_yes_min_ratio.unwrap_or(base.taker_yes_min_ratio);
        consider(v < cur - 1e-12);
    }
    if let Some(v) = delta.taker_no_max_ratio {
        let cur = current.taker_no_max_ratio.unwrap_or(base.taker_no_max_ratio);
        consider(v > cur + 1e-12);
    }
    if let Some(v) = delta.taker_direction_confirm {
        let cur = current.taker_direction_confirm.unwrap_or(base.taker_direction_confirm);
        consider(!v && cur);
    }
    if let Some(v) = delta.htf_enabled {
        let cur = current.htf_enabled.unwrap_or(base.htf_enabled);
        consider(!v && cur);
    }
    if let Some(v) = delta.multi_tf_enabled {
        let cur = current.multi_tf_enabled.unwrap_or(base.multi_tf_enabled);
        consider(!v && cur);
    }

    match (loosen, tighten) {
        (0, 0) => "none",
        (_, 0) => "loosen",
        (0, _) => "tighten",
        _ => "mixed",
    }
}

/// Remove loosening-only proposals when live trading is weak; tightening changes are kept.
fn filter_live_veto_loosening(
    delta: &mut CalibratedOverrides,
    current: &CalibratedOverrides,
    base: &BaseSnapshot,
    cfg: &ShadowCalibrationConfig,
    live: Option<&LiveAssetGate>,
) -> bool {
    if !cfg.live_veto_enabled {
        return false;
    }
    let Some(l) = live else {
        return false;
    };
    if !live_strip_loosening_trigger(l, cfg) {
        return false;
    }

    let mut stripped = false;

    let cur_edge = current.min_edge.unwrap_or(base.min_edge);
    let cur_conf = current.min_confidence.unwrap_or(base.min_confidence);
    if let Some(v) = delta.min_edge {
        if v < cur_edge - 1e-12 {
            delta.min_edge = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.min_confidence {
        if v < cur_conf - 1e-12 {
            delta.min_confidence = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.yes_confidence_penalty {
        let cur = current.yes_confidence_penalty.unwrap_or(base.yes_confidence_penalty);
        if v < cur - 1e-12 {
            delta.yes_confidence_penalty = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.no_confidence_penalty {
        let cur = current.no_confidence_penalty.unwrap_or(base.no_confidence_penalty);
        if v < cur - 1e-12 {
            delta.no_confidence_penalty = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.cluster_down_confidence_add {
        let cur = current.cluster_down_confidence_add.unwrap_or(base.cluster_down_confidence_add);
        if v < cur - 1e-12 {
            delta.cluster_down_confidence_add = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.min_macd_histogram_abs {
        let cur = current.min_macd_histogram_abs.unwrap_or(base.min_macd_histogram_abs);
        if v < cur - 1e-12 {
            delta.min_macd_histogram_abs = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.volume_min_ratio {
        let cur = current.volume_min_ratio.unwrap_or(base.volume_min_ratio.unwrap_or(0.0));
        if v < cur - 1e-12 {
            delta.volume_min_ratio = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.min_momentum_5m_abs {
        let cur = current.min_momentum_5m_abs.unwrap_or(base.min_momentum_5m_abs);
        if v < cur - 1e-12 {
            delta.min_momentum_5m_abs = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.momentum_vol_reference {
        let cur = current.momentum_vol_reference.unwrap_or(base.momentum_vol_reference);
        // Denominator in vol_std / momentum_vol_reference: raising it lowers effective threshold = loosening.
        if v > cur + 1e-12 {
            delta.momentum_vol_reference = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.cluster_tie_min_edge_multiplier {
        let cur = current.cluster_tie_min_edge_multiplier.unwrap_or(base.cluster_tie_min_edge_multiplier);
        if v < cur - 1e-12 {
            delta.cluster_tie_min_edge_multiplier = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.neutral_taker_edge_multiplier {
        let cur = current.neutral_taker_edge_multiplier.unwrap_or(base.neutral_taker_edge_multiplier);
        if v < cur - 1e-12 {
            delta.neutral_taker_edge_multiplier = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.mid_price_band_min_edge_multiplier {
        let cur = current.mid_price_band_min_edge_multiplier.unwrap_or(base.mid_price_band_min_edge_multiplier);
        if v < cur - 1e-12 {
            delta.mid_price_band_min_edge_multiplier = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.rsi_yes_max {
        let cur = current.rsi_yes_max.unwrap_or(base.rsi_yes_max);
        if v > cur + 1e-12 {
            delta.rsi_yes_max = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.rsi_no_min {
        let cur = current.rsi_no_min.unwrap_or(base.rsi_no_min);
        if v < cur - 1e-12 {
            delta.rsi_no_min = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.cluster_rsi_oversold {
        let cur = current.cluster_rsi_oversold.unwrap_or(base.cluster_rsi_oversold);
        // Used as `rsi < threshold → UP`: raising widens oversold zone = loosening.
        if v > cur + 1e-12 {
            delta.cluster_rsi_oversold = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.cluster_rsi_overbought {
        let cur = current.cluster_rsi_overbought.unwrap_or(base.cluster_rsi_overbought);
        // Used as `rsi > threshold → DOWN`: lowering widens overbought zone = loosening.
        if v < cur - 1e-12 {
            delta.cluster_rsi_overbought = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.cheap_token_price_threshold {
        let cur = current.cheap_token_price_threshold.unwrap_or(base.cheap_token_price_threshold);
        if v > cur + 1e-12 {
            delta.cheap_token_price_threshold = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.taker_yes_min_ratio {
        let cur = current.taker_yes_min_ratio.unwrap_or(base.taker_yes_min_ratio);
        if v < cur - 1e-12 {
            delta.taker_yes_min_ratio = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.taker_no_max_ratio {
        let cur = current.taker_no_max_ratio.unwrap_or(base.taker_no_max_ratio);
        if v > cur + 1e-12 {
            delta.taker_no_max_ratio = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.taker_neutral_low {
        let cur = current.taker_neutral_low.unwrap_or(base.taker_neutral_low);
        if v < cur - 1e-12 {
            delta.taker_neutral_low = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.taker_neutral_high {
        let cur = current.taker_neutral_high.unwrap_or(base.taker_neutral_high);
        if v > cur + 1e-12 {
            delta.taker_neutral_high = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.min_secs_to_close {
        let cur = current.min_secs_to_close.unwrap_or(base.min_secs_to_close.unwrap_or(0));
        // Lower min_secs = easier / looser (trade sooner).
        if v < cur {
            delta.min_secs_to_close = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.max_secs_to_close {
        let cur = current.max_secs_to_close.unwrap_or(base.max_secs_to_close.unwrap_or(0));
        if (v as f64) > (cur as f64) + 1e-12 {
            delta.max_secs_to_close = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.taker_direction_confirm {
        let cur = current.taker_direction_confirm.unwrap_or(base.taker_direction_confirm);
        if !v && cur {
            delta.taker_direction_confirm = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.htf_enabled {
        let cur = current.htf_enabled.unwrap_or(base.htf_enabled);
        if !v && cur {
            delta.htf_enabled = None;
            stripped = true;
        }
    }
    if let Some(v) = delta.multi_tf_enabled {
        let cur = current.multi_tf_enabled.unwrap_or(base.multi_tf_enabled);
        if !v && cur {
            delta.multi_tf_enabled = None;
            stripped = true;
        }
    }

    stripped
}

fn compute_percentiles(values: &mut Vec<f64>) -> Option<PercentileSet> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = values.len();
    let percentile = |p: f64| -> f64 {
        let idx = (p * (n - 1) as f64).round() as usize;
        values[idx.min(n - 1)]
    };
    Some(PercentileSet {
        p10: percentile(0.10),
        p25: percentile(0.25),
        p50: percentile(0.50),
        p75: percentile(0.75),
        p90: percentile(0.90),
    })
}

pub fn compute_shadow_stats(trades: &[TradeRecord], asset: &str) -> ShadowAssetStats {
    let resolved: Vec<&TradeRecord> = trades
        .iter()
        .filter(|t| t.asset == asset && t.outcome.is_some())
        .collect();

    let n = resolved.len();
    if n == 0 {
        return ShadowAssetStats::default();
    }

    let wins = resolved.iter().filter(|t| trade_won(t)).count();
    let wr = wins as f64 / n as f64;

    let yes_trades: Vec<&&TradeRecord> = resolved.iter().filter(|t| t.direction == "YES").collect();
    let no_trades: Vec<&&TradeRecord> = resolved.iter().filter(|t| t.direction == "NO").collect();
    let yes_wins = yes_trades.iter().filter(|t| trade_won(t)).count();
    let no_wins = no_trades.iter().filter(|t| trade_won(t)).count();
    let wr_yes = if yes_trades.is_empty() { 0.0 } else { yes_wins as f64 / yes_trades.len() as f64 };
    let wr_no = if no_trades.is_empty() { 0.0 } else { no_wins as f64 / no_trades.len() as f64 };

    let pnl: f64 = resolved
        .iter()
        .filter_map(|t| t.pnl.as_ref()?.parse::<f64>().ok())
        .sum();

    let winners: Vec<&&TradeRecord> = resolved.iter().filter(|t| trade_won(t)).collect();

    let mut percentiles = HashMap::new();

    // RSI
    let mut winner_rsi: Vec<f64> = winners.iter().filter_map(|t| t.rsi).collect();
    if let Some(p) = compute_percentiles(&mut winner_rsi) {
        percentiles.insert("rsi".to_string(), p);
    }
    // YES winners RSI (for rsi_yes_max)
    let mut yes_winner_rsi: Vec<f64> = winners
        .iter()
        .filter(|t| t.direction == "YES")
        .filter_map(|t| t.rsi)
        .collect();
    if let Some(p) = compute_percentiles(&mut yes_winner_rsi) {
        percentiles.insert("rsi_yes_winners".to_string(), p);
    }
    // NO winners RSI (for rsi_no_min)
    let mut no_winner_rsi: Vec<f64> = winners
        .iter()
        .filter(|t| t.direction == "NO")
        .filter_map(|t| t.rsi)
        .collect();
    if let Some(p) = compute_percentiles(&mut no_winner_rsi) {
        percentiles.insert("rsi_no_winners".to_string(), p);
    }

    // MACD histogram
    let mut winner_macd: Vec<f64> = winners.iter().filter_map(|t| t.macd_histogram.map(|v| v.abs())).collect();
    if let Some(p) = compute_percentiles(&mut winner_macd) {
        percentiles.insert("macd_histogram_abs".to_string(), p);
    }

    // Volume ratio
    let mut winner_vol: Vec<f64> = winners.iter().filter_map(|t| t.volume_ratio).collect();
    if let Some(p) = compute_percentiles(&mut winner_vol) {
        percentiles.insert("volume_ratio".to_string(), p);
    }

    // Momentum 5m
    let mut winner_mom: Vec<f64> = winners.iter().filter_map(|t| t.momentum_5m.map(|v| v.abs())).collect();
    if let Some(p) = compute_percentiles(&mut winner_mom) {
        percentiles.insert("momentum_5m_abs".to_string(), p);
    }

    // Taker buy ratio
    let mut winner_tbr: Vec<f64> = winners.iter().filter_map(|t| t.taker_buy_ratio).collect();
    if let Some(p) = compute_percentiles(&mut winner_tbr) {
        percentiles.insert("taker_buy_ratio".to_string(), p);
    }
    let mut yes_winner_tbr: Vec<f64> = winners
        .iter()
        .filter(|t| t.direction == "YES")
        .filter_map(|t| t.taker_buy_ratio)
        .collect();
    if let Some(p) = compute_percentiles(&mut yes_winner_tbr) {
        percentiles.insert("tbr_yes_winners".to_string(), p);
    }
    let mut no_winner_tbr: Vec<f64> = winners
        .iter()
        .filter(|t| t.direction == "NO")
        .filter_map(|t| t.taker_buy_ratio)
        .collect();
    if let Some(p) = compute_percentiles(&mut no_winner_tbr) {
        percentiles.insert("tbr_no_winners".to_string(), p);
    }

    // Secs to close
    let mut winner_secs: Vec<f64> = winners
        .iter()
        .filter_map(|t| t.secs_to_close.map(|s| s as f64))
        .collect();
    if let Some(p) = compute_percentiles(&mut winner_secs) {
        percentiles.insert("secs_to_close".to_string(), p);
    }

    // Volatility (for momentum_vol_reference)
    let mut all_vol: Vec<f64> = resolved.iter().filter_map(|t| t.volatility_std_pct).collect();
    if let Some(p) = compute_percentiles(&mut all_vol) {
        percentiles.insert("volatility_std_pct".to_string(), p);
    }

    // Entry price for cheap token threshold
    let mut winner_prices: Vec<f64> = winners
        .iter()
        .filter_map(|t| t.entry_price.parse::<f64>().ok())
        .filter(|p| *p < 0.50)
        .collect();
    if let Some(p) = compute_percentiles(&mut winner_prices) {
        percentiles.insert("cheap_entry_price".to_string(), p);
    }

    // Segment WRs
    let mut segment_wrs = HashMap::new();
    let mut segment_counts = HashMap::new();

    // Cluster direction segments
    for segment in ["DOWN", "TIE"] {
        let seg_trades: Vec<&&TradeRecord> = resolved
            .iter()
            .filter(|t| t.cluster_direction.as_deref() == Some(segment))
            .collect();
        if seg_trades.len() >= 5 {
            let seg_wins = seg_trades.iter().filter(|t| trade_won(t)).count();
            segment_wrs.insert(format!("cluster_{}", segment.to_lowercase()), seg_wins as f64 / seg_trades.len() as f64);
            segment_counts.insert(format!("cluster_{}", segment.to_lowercase()), seg_trades.len());
        }
    }

    // Neutral TBR segment
    let neutral_tbr: Vec<&&TradeRecord> = resolved
        .iter()
        .filter(|t| {
            t.taker_buy_ratio
                .map(|r| r >= 0.45 && r <= 0.55)
                .unwrap_or(false)
        })
        .collect();
    if neutral_tbr.len() >= 5 {
        let seg_wins = neutral_tbr.iter().filter(|t| trade_won(t)).count();
        segment_wrs.insert("neutral_tbr".to_string(), seg_wins as f64 / neutral_tbr.len() as f64);
        segment_counts.insert("neutral_tbr".to_string(), neutral_tbr.len());
    }

    // Mid-price band segment
    let mid_band: Vec<&&TradeRecord> = resolved
        .iter()
        .filter(|t| {
            t.market_yes_price
                .as_ref()
                .and_then(|s| s.parse::<f64>().ok())
                .map(|p| p >= 0.20 && p <= 0.35)
                .unwrap_or(false)
        })
        .collect();
    if mid_band.len() >= 5 {
        let seg_wins = mid_band.iter().filter(|t| trade_won(t)).count();
        segment_wrs.insert("mid_price_band".to_string(), seg_wins as f64 / mid_band.len() as f64);
        segment_counts.insert("mid_price_band".to_string(), mid_band.len());
    }

    // HTF aligned segment
    let htf_aligned: Vec<&&TradeRecord> = resolved
        .iter()
        .filter(|t| t.htf_aligned == Some(true))
        .collect();
    let htf_misaligned: Vec<&&TradeRecord> = resolved
        .iter()
        .filter(|t| t.htf_aligned == Some(false))
        .collect();
    if htf_aligned.len() >= 5 {
        let w = htf_aligned.iter().filter(|t| trade_won(t)).count();
        segment_wrs.insert("htf_aligned".to_string(), w as f64 / htf_aligned.len() as f64);
        segment_counts.insert("htf_aligned".to_string(), htf_aligned.len());
    }
    if htf_misaligned.len() >= 5 {
        let w = htf_misaligned.iter().filter(|t| trade_won(t)).count();
        segment_wrs.insert("htf_misaligned".to_string(), w as f64 / htf_misaligned.len() as f64);
        segment_counts.insert("htf_misaligned".to_string(), htf_misaligned.len());
    }

    // Taker direction aligned segment
    let taker_aligned: Vec<&&TradeRecord> = resolved
        .iter()
        .filter(|t| t.taker_direction_aligned == Some(true))
        .collect();
    let taker_misaligned: Vec<&&TradeRecord> = resolved
        .iter()
        .filter(|t| t.taker_direction_aligned == Some(false))
        .collect();
    if taker_aligned.len() >= 5 {
        let w = taker_aligned.iter().filter(|t| trade_won(t)).count();
        segment_wrs.insert("taker_aligned".to_string(), w as f64 / taker_aligned.len() as f64);
        segment_counts.insert("taker_aligned".to_string(), taker_aligned.len());
    }
    if taker_misaligned.len() >= 5 {
        let w = taker_misaligned.iter().filter(|t| trade_won(t)).count();
        segment_wrs.insert("taker_misaligned".to_string(), w as f64 / taker_misaligned.len() as f64);
        segment_counts.insert("taker_misaligned".to_string(), taker_misaligned.len());
    }

    // Multi-TF agreement segment
    let mtf_agree: Vec<&&TradeRecord> = resolved
        .iter()
        .filter(|t| t.multi_tf_direction_agreement == Some(true))
        .collect();
    let mtf_disagree: Vec<&&TradeRecord> = resolved
        .iter()
        .filter(|t| t.multi_tf_direction_agreement == Some(false))
        .collect();
    if mtf_agree.len() >= 5 {
        let w = mtf_agree.iter().filter(|t| trade_won(t)).count();
        segment_wrs.insert("multi_tf_agree".to_string(), w as f64 / mtf_agree.len() as f64);
        segment_counts.insert("multi_tf_agree".to_string(), mtf_agree.len());
    }
    if mtf_disagree.len() >= 5 {
        let w = mtf_disagree.iter().filter(|t| trade_won(t)).count();
        segment_wrs.insert("multi_tf_disagree".to_string(), w as f64 / mtf_disagree.len() as f64);
        segment_counts.insert("multi_tf_disagree".to_string(), mtf_disagree.len());
    }

    ShadowAssetStats {
        wr,
        wr_yes,
        wr_no,
        pnl,
        trade_count: n,
        yes_count: yes_trades.len(),
        no_count: no_trades.len(),
        percentiles,
        segment_wrs,
        segment_counts,
    }
}

// ---------------------------------------------------------------------------
// Proposal engine
// ---------------------------------------------------------------------------

fn clamp_step(current: f64, proposed: f64, max_step_pct: f64) -> f64 {
    if current == 0.0 {
        return proposed;
    }
    let max_delta = current.abs() * max_step_pct;
    let delta = proposed - current;
    current + delta.clamp(-max_delta, max_delta)
}

fn clamp_bounds(value: f64, base: f64, low_mult: f64, high_mult: f64) -> f64 {
    if base == 0.0 {
        return value;
    }
    let lo = base * low_mult;
    let hi = base * high_mult;
    value.clamp(lo.min(hi), lo.max(hi))
}

fn safe_propose(current: f64, proposed: f64, base: f64, cfg: &ShadowCalibrationConfig) -> f64 {
    let stepped = clamp_step(current, proposed, cfg.max_step_pct);
    clamp_bounds(stepped, base, cfg.safety_bound_low, cfg.safety_bound_high)
}

fn wr_driven_nudge(current: f64, wr: f64, base: f64, cfg: &ShadowCalibrationConfig) -> Option<f64> {
    let proposed = if wr > 0.55 {
        current * 0.9
    } else if wr < 0.40 {
        current * 1.15
    } else {
        return None;
    };
    Some(safe_propose(current, proposed, base, cfg))
}

fn wr_driven_multiplier_nudge(current: f64, wr: f64, base: f64, cfg: &ShadowCalibrationConfig) -> Option<f64> {
    let proposed = if wr > 0.55 {
        current * 0.92
    } else if wr < 0.40 {
        current * 1.10
    } else {
        return None;
    };
    Some(safe_propose(current, proposed, base, cfg))
}

/// Clear penalty *decreases* on a side when live trades show that direction is already failing.
fn strip_penalty_loosening_for_live_direction(
    ov: &mut CalibratedOverrides,
    current: &CalibratedOverrides,
    base: &BaseSnapshot,
    cfg: &ShadowCalibrationConfig,
    live_dir: Option<&LiveDirectionStats>,
) {
    let Some(ld) = live_dir else {
        return;
    };
    if let Some(v) = ov.no_confidence_penalty {
        let cur = current.no_confidence_penalty.unwrap_or(base.no_confidence_penalty);
        if v < cur - 1e-12 && ld.no_count >= 2 && ld.no_wr < cfg.live_direction_veto_wr {
            ov.no_confidence_penalty = None;
        }
    }
    if let Some(v) = ov.yes_confidence_penalty {
        let cur = current.yes_confidence_penalty.unwrap_or(base.yes_confidence_penalty);
        if v < cur - 1e-12 && ld.yes_count >= 2 && ld.yes_wr < cfg.live_direction_veto_wr {
            ov.yes_confidence_penalty = None;
        }
    }
}

pub fn propose_overrides(
    stats: &ShadowAssetStats,
    base: &BaseSnapshot,
    current: &CalibratedOverrides,
    cfg: &ShadowCalibrationConfig,
    live_dir: Option<&LiveDirectionStats>,
) -> CalibratedOverrides {
    let excluded = |name: &str| cfg.exclude_params.iter().any(|p| p == name);
    let mut ov = CalibratedOverrides::default();

    let cur_min_edge = current.min_edge.unwrap_or(base.min_edge);
    let cur_min_conf = current.min_confidence.unwrap_or(base.min_confidence);

    // --- A) WR-driven ---
    if !excluded("min_edge") {
        ov.min_edge = wr_driven_nudge(cur_min_edge, stats.wr, base.min_edge, cfg);
    }
    if !excluded("min_confidence") {
        ov.min_confidence = wr_driven_nudge(cur_min_conf, stats.wr, base.min_confidence, cfg);
    }

    if !excluded("yes_confidence_penalty") || !excluded("no_confidence_penalty") {
        let wr_diff = stats.wr_yes - stats.wr_no;
        if wr_diff.abs() > 0.10 {
            if wr_diff > 0.0 && !excluded("no_confidence_penalty") {
                let cur = current.no_confidence_penalty.unwrap_or(base.no_confidence_penalty);
                let proposed = (cur + 0.02).min(0.50);
                ov.no_confidence_penalty = Some(safe_propose(cur, proposed, base.no_confidence_penalty, cfg));
            } else if wr_diff < 0.0 && !excluded("yes_confidence_penalty") {
                let cur = current.yes_confidence_penalty.unwrap_or(base.yes_confidence_penalty);
                let proposed = (cur + 0.02).min(0.50);
                ov.yes_confidence_penalty = Some(safe_propose(cur, proposed, base.yes_confidence_penalty, cfg));
            }
        }
    }

    if !excluded("cluster_down_confidence_add") {
        if let Some(&seg_wr) = stats.segment_wrs.get("cluster_down") {
            let cur = current.cluster_down_confidence_add.unwrap_or(base.cluster_down_confidence_add);
            let proposed = if seg_wr > 0.55 {
                (cur + 0.01).min(0.30)
            } else if seg_wr < 0.40 {
                (cur - 0.01).max(0.0)
            } else {
                cur
            };
            if (proposed - cur).abs() > 1e-9 {
                ov.cluster_down_confidence_add = Some(safe_propose(cur, proposed, base.cluster_down_confidence_add, cfg));
            }
        }
    }

    // WR-driven multipliers
    if !excluded("cluster_tie_min_edge_multiplier") {
        if let Some(&seg_wr) = stats.segment_wrs.get("cluster_tie") {
            let cur = current.cluster_tie_min_edge_multiplier.unwrap_or(base.cluster_tie_min_edge_multiplier);
            ov.cluster_tie_min_edge_multiplier = wr_driven_multiplier_nudge(cur, seg_wr, base.cluster_tie_min_edge_multiplier, cfg);
        }
    }
    if !excluded("neutral_taker_edge_multiplier") {
        if let Some(&seg_wr) = stats.segment_wrs.get("neutral_tbr") {
            let cur = current.neutral_taker_edge_multiplier.unwrap_or(base.neutral_taker_edge_multiplier);
            ov.neutral_taker_edge_multiplier = wr_driven_multiplier_nudge(cur, seg_wr, base.neutral_taker_edge_multiplier, cfg);
        }
    }
    if !excluded("mid_price_band_min_edge_multiplier") {
        if let Some(&seg_wr) = stats.segment_wrs.get("mid_price_band") {
            let cur = current.mid_price_band_min_edge_multiplier.unwrap_or(base.mid_price_band_min_edge_multiplier);
            ov.mid_price_band_min_edge_multiplier = wr_driven_multiplier_nudge(cur, seg_wr, base.mid_price_band_min_edge_multiplier, cfg);
        }
    }

    // --- B) Percentile-driven ---
    if !excluded("min_macd_histogram_abs") {
        if let Some(p) = stats.percentiles.get("macd_histogram_abs") {
            let cur = current.min_macd_histogram_abs.unwrap_or(base.min_macd_histogram_abs);
            ov.min_macd_histogram_abs = Some(safe_propose(cur, p.p25, base.min_macd_histogram_abs, cfg));
        }
    }
    if !excluded("volume_min_ratio") {
        if let Some(p) = stats.percentiles.get("volume_ratio") {
            let cur = current.volume_min_ratio.unwrap_or(base.volume_min_ratio.unwrap_or(0.0));
            let base_v = base.volume_min_ratio.unwrap_or(0.5);
            ov.volume_min_ratio = Some(safe_propose(cur, p.p25, base_v, cfg));
        }
    }
    if !excluded("min_momentum_5m_abs") {
        if let Some(p) = stats.percentiles.get("momentum_5m_abs") {
            let cur = current.min_momentum_5m_abs.unwrap_or(base.min_momentum_5m_abs);
            ov.min_momentum_5m_abs = Some(safe_propose(cur, p.p25, base.min_momentum_5m_abs, cfg));
        }
    }
    if !excluded("momentum_vol_reference") {
        if let Some(p) = stats.percentiles.get("volatility_std_pct") {
            let cur = current.momentum_vol_reference.unwrap_or(base.momentum_vol_reference);
            ov.momentum_vol_reference = Some(safe_propose(cur, p.p50, base.momentum_vol_reference, cfg));
        }
    }
    if !excluded("rsi_yes_max") {
        if let Some(p) = stats.percentiles.get("rsi_yes_winners") {
            let cur = current.rsi_yes_max.unwrap_or(base.rsi_yes_max);
            ov.rsi_yes_max = Some(safe_propose(cur, p.p90, base.rsi_yes_max, cfg));
        }
    }
    if !excluded("rsi_no_min") {
        if let Some(p) = stats.percentiles.get("rsi_no_winners") {
            let cur = current.rsi_no_min.unwrap_or(base.rsi_no_min);
            ov.rsi_no_min = Some(safe_propose(cur, p.p10, base.rsi_no_min, cfg));
        }
    }
    if !excluded("cluster_rsi_oversold") {
        if let Some(p) = stats.percentiles.get("rsi") {
            let cur = current.cluster_rsi_oversold.unwrap_or(base.cluster_rsi_oversold);
            ov.cluster_rsi_oversold = Some(safe_propose(cur, p.p10, base.cluster_rsi_oversold, cfg));
        }
    }
    if !excluded("cluster_rsi_overbought") {
        if let Some(p) = stats.percentiles.get("rsi") {
            let cur = current.cluster_rsi_overbought.unwrap_or(base.cluster_rsi_overbought);
            ov.cluster_rsi_overbought = Some(safe_propose(cur, p.p90, base.cluster_rsi_overbought, cfg));
        }
    }
    if !excluded("min_secs_to_close") {
        if let Some(p) = stats.percentiles.get("secs_to_close") {
            let proposed = p.p10 as i64;
            if proposed > 0 {
                ov.min_secs_to_close = Some(proposed);
            }
        }
    }
    if !excluded("max_secs_to_close") {
        if let Some(p) = stats.percentiles.get("secs_to_close") {
            let proposed = p.p90 as i64;
            if proposed > 0 {
                ov.max_secs_to_close = Some(proposed);
            }
        }
    }
    if !excluded("cheap_token_price_threshold") {
        if let Some(p) = stats.percentiles.get("cheap_entry_price") {
            let cur = current.cheap_token_price_threshold.unwrap_or(base.cheap_token_price_threshold);
            ov.cheap_token_price_threshold = Some(safe_propose(cur, p.p75, base.cheap_token_price_threshold, cfg));
        }
    }
    if !excluded("taker_yes_min_ratio") {
        if let Some(p) = stats.percentiles.get("tbr_yes_winners") {
            let cur = current.taker_yes_min_ratio.unwrap_or(base.taker_yes_min_ratio);
            ov.taker_yes_min_ratio = Some(safe_propose(cur, p.p25, base.taker_yes_min_ratio, cfg));
        }
    }
    if !excluded("taker_no_max_ratio") {
        if let Some(p) = stats.percentiles.get("tbr_no_winners") {
            let cur = current.taker_no_max_ratio.unwrap_or(base.taker_no_max_ratio);
            ov.taker_no_max_ratio = Some(safe_propose(cur, p.p75, base.taker_no_max_ratio, cfg));
        }
    }
    if !excluded("taker_neutral_low") {
        if let Some(p) = stats.percentiles.get("taker_buy_ratio") {
            let cur = current.taker_neutral_low.unwrap_or(base.taker_neutral_low);
            ov.taker_neutral_low = Some(safe_propose(cur, p.p25, base.taker_neutral_low, cfg));
        }
    }
    if !excluded("taker_neutral_high") {
        if let Some(p) = stats.percentiles.get("taker_buy_ratio") {
            let cur = current.taker_neutral_high.unwrap_or(base.taker_neutral_high);
            ov.taker_neutral_high = Some(safe_propose(cur, p.p75, base.taker_neutral_high, cfg));
        }
    }

    // --- C) Bool toggles (WR-gated) ---
    let min_bool = cfg.bool_toggle_min_trades;

    if !excluded("taker_direction_confirm") {
        let aligned_wr = stats.segment_wrs.get("taker_aligned");
        let misaligned_wr = stats.segment_wrs.get("taker_misaligned");
        let aligned_n = stats.segment_counts.get("taker_aligned").copied().unwrap_or(0);
        let misaligned_n = stats.segment_counts.get("taker_misaligned").copied().unwrap_or(0);
        if aligned_n + misaligned_n >= min_bool {
            if let (Some(&a_wr), Some(&m_wr)) = (aligned_wr, misaligned_wr) {
                if (a_wr - m_wr).abs() > 0.05 {
                    ov.taker_direction_confirm = Some(a_wr > m_wr);
                }
            }
        }
    }
    if !excluded("htf_enabled") {
        let aligned_wr = stats.segment_wrs.get("htf_aligned");
        let aligned_n = stats.segment_counts.get("htf_aligned").copied().unwrap_or(0);
        let misaligned_n = stats.segment_counts.get("htf_misaligned").copied().unwrap_or(0);
        if aligned_n + misaligned_n >= min_bool {
            if let Some(&a_wr) = aligned_wr {
                if a_wr > stats.wr + 0.05 {
                    ov.htf_enabled = Some(true);
                } else if a_wr < stats.wr - 0.05 {
                    ov.htf_enabled = Some(false);
                }
            }
        }
    }
    if !excluded("multi_tf_enabled") {
        let agree_wr = stats.segment_wrs.get("multi_tf_agree");
        let agree_n = stats.segment_counts.get("multi_tf_agree").copied().unwrap_or(0);
        let disagree_n = stats.segment_counts.get("multi_tf_disagree").copied().unwrap_or(0);
        if agree_n + disagree_n >= min_bool {
            if let Some(&a_wr) = agree_wr {
                if a_wr > stats.wr + 0.05 {
                    ov.multi_tf_enabled = Some(true);
                } else if a_wr < stats.wr - 0.05 {
                    ov.multi_tf_enabled = Some(false);
                }
            }
        }
    }

    strip_penalty_loosening_for_live_direction(&mut ov, current, base, cfg, live_dir);

    ov
}

// ---------------------------------------------------------------------------
// Rollback monitor
// ---------------------------------------------------------------------------

pub fn should_rollback(
    trades_path: &str,
    asset: &str,
    calibration_version: u64,
    rollback_window: usize,
    rollback_threshold: f64,
) -> bool {
    let trades = match read_trades_from_path(trades_path) {
        Ok(t) => t,
        Err(_) => return false,
    };

    let calibrated: Vec<&TradeRecord> = trades
        .iter()
        .filter(|t| {
            t.asset == asset
                && t.outcome.is_some()
                && t.calibration_version == Some(calibration_version)
        })
        .collect();

    if calibrated.len() < rollback_window {
        return false;
    }

    let recent = &calibrated[calibrated.len().saturating_sub(rollback_window)..];
    let wins = recent.iter().filter(|t| trade_won(t)).count();
    let wr = wins as f64 / recent.len() as f64;
    wr < rollback_threshold
}

// ---------------------------------------------------------------------------
// ShadowCalibrator (main orchestrator)
// ---------------------------------------------------------------------------

pub struct ShadowCalibrator {
    pub config: ShadowCalibrationConfig,
    pub state: CalibrationStateFile,
    state_path: String,
    shadow_trades_path: String,
    trades_path: String,
}

impl ShadowCalibrator {
    pub fn new(data_dir: &str, config: ShadowCalibrationConfig) -> Self {
        let state_path = format!("{}/calibration_state.json", data_dir);
        let shadow_trades_path = format!("{}/shadow_trades.jsonl", data_dir);
        let trades_path = format!("{}/trades.jsonl", data_dir);

        let state = Self::load_state(&state_path).unwrap_or_default();

        Self {
            config,
            state,
            state_path,
            shadow_trades_path,
            trades_path,
        }
    }

    fn load_state(path: &str) -> Result<CalibrationStateFile> {
        if !Path::new(path).exists() {
            return Ok(CalibrationStateFile::default());
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("read calibration state: {}", path))?;
        serde_json::from_str(&content)
            .with_context(|| format!("parse calibration state: {}", path))
    }

    fn save_state(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.state)
            .context("serialize calibration state")?;
        std::fs::write(&self.state_path, json)
            .with_context(|| format!("write calibration state: {}", self.state_path))
    }

    pub fn current_version_for_asset(&self, asset: &str) -> Option<u64> {
        self.state
            .assets
            .get(asset)
            .filter(|s| !s.rolled_back)
            .map(|s| s.calibration_version)
    }

    pub fn get_overrides(&self, asset: &str) -> Option<&CalibratedOverrides> {
        self.state
            .assets
            .get(asset)
            .filter(|s| !s.rolled_back)
            .map(|s| &s.applied_overrides)
    }

    fn should_recalibrate_asset(&self, asset: &str, shadow_pnl: f64) -> bool {
        let Some(prev) = self.state.assets.get(asset) else {
            return true;
        };
        if prev.rolled_back {
            return true;
        }
        let elapsed = Utc::now()
            .signed_duration_since(prev.last_calibrated_at)
            .num_seconds() as u64;
        if elapsed < self.config.cooldown_secs {
            return false;
        }
        let pnl_delta = (shadow_pnl - prev.shadow_pnl).abs();
        Decimal::from_f64_retain(pnl_delta).unwrap_or(Decimal::ZERO) >= self.config.min_pnl_delta
    }

    /// Run calibration check for all assets. Call after shadow trade resolution each cycle.
    pub fn maybe_recalibrate(
        &mut self,
        assets: &[String],
        base_strategies: &HashMap<String, AssetStrategy>,
    ) {
        if !self.config.enabled {
            return;
        }

        let shadow_trades = match read_trades_from_path(&self.shadow_trades_path) {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "failed to read shadow trades for calibration");
                return;
            }
        };

        let mut changed = false;

        for asset in assets {
            // Rollback check first
            if let Some(state) = self.state.assets.get(asset) {
                if !state.rolled_back {
                    if should_rollback(
                        &self.trades_path,
                        asset,
                        state.calibration_version,
                        self.config.rollback_window,
                        self.config.rollback_threshold,
                    ) {
                        info!(
                            asset = %asset,
                            version = state.calibration_version,
                            "rolling back calibration — poor live performance"
                        );
                        if let Some(s) = self.state.assets.get_mut(asset) {
                            s.rolled_back = true;
                            s.applied_overrides = CalibratedOverrides::default();
                        }
                        changed = true;
                        continue;
                    }
                }
            }

            let stats = compute_shadow_stats(&shadow_trades, asset);
            if stats.trade_count < self.config.min_trades {
                debug!(
                    asset = %asset,
                    count = stats.trade_count,
                    min = self.config.min_trades,
                    "not enough shadow trades for calibration"
                );
                continue;
            }

            if !self.should_recalibrate_asset(asset, stats.pnl) {
                continue;
            }

            let Some(base_st) = base_strategies.get(asset.as_str()) else {
                continue;
            };

            let base_snap = BaseSnapshot::from_strategy(base_st);
            let current_ov = self
                .state
                .assets
                .get(asset.as_str())
                .map(|s| s.applied_overrides.clone())
                .unwrap_or_default();

            let live_agg = compute_live_asset_stats(&self.trades_path, asset, self.config.live_veto_window);
            let live_dir = compute_live_direction_stats(&self.trades_path, asset, self.config.live_veto_window);

            let mut delta = propose_overrides(
                &stats,
                &base_snap,
                &current_ov,
                &self.config,
                live_dir.as_ref(),
            );

            let veto_stripped = filter_live_veto_loosening(
                &mut delta,
                &current_ov,
                &base_snap,
                &self.config,
                live_agg.as_ref(),
            );

            if let Some(ref l) = live_agg {
                if veto_stripped && live_metrics_stress(l, &self.config) {
                    warn!(
                        asset = %asset,
                        live_wr = format!("{:.2}", l.wr),
                        live_pnl = format!("{:.2}", l.pnl),
                        live_count = l.count,
                        "shadow-live mismatch — live veto stripped loosening calibration proposals"
                    );
                }
            }

            if delta.is_empty() {
                debug!(asset = %asset, "no parameter changes proposed");
                continue;
            }

            let overrides_direction = classify_override_direction(&delta, &current_ov, &base_snap);

            let mut merged_ov = current_ov.clone();
            merged_ov.merge_from(&delta);

            self.state.global_version += 1;
            let version = self.state.global_version;

            info!(
                asset = %asset,
                version = version,
                shadow_wr = format!("{:.2}", stats.wr),
                shadow_pnl = format!("{:.2}", stats.pnl),
                trade_count = stats.trade_count,
                live_wr = live_agg.as_ref().map(|l| format!("{:.2}", l.wr)).unwrap_or_else(|| "n/a".to_string()),
                live_pnl = live_agg.as_ref().map(|l| format!("{:.2}", l.pnl)).unwrap_or_else(|| "n/a".to_string()),
                live_count = live_agg.as_ref().map(|l| l.count).unwrap_or(0),
                veto_active = veto_stripped,
                overrides_direction = overrides_direction,
                "applying shadow calibration"
            );

            self.state.assets.insert(
                asset.clone(),
                AssetCalibrationState {
                    last_calibrated_at: Utc::now(),
                    shadow_wr: stats.wr,
                    shadow_pnl: stats.pnl,
                    shadow_trade_count: stats.trade_count,
                    calibration_version: version,
                    applied_overrides: merged_ov,
                    base_snapshot: base_snap,
                    trade_count_since_calibration: 0,
                    rolled_back: false,
                },
            );
            changed = true;
        }

        if changed {
            if let Err(e) = self.save_state() {
                warn!(error = %e, "failed to persist calibration state");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Direction;
    use rust_decimal_macros::dec;

    fn sample_shadow(asset: &str, direction: Direction, outcome: bool, rsi: f64) -> TradeRecord {
        let mut r = TradeRecord::new(
            format!("c_{}", uuid::Uuid::new_v4()),
            asset.to_string(),
            "15m".to_string(),
            direction,
            dec!(0.5),
            dec!(5),
            dec!(10),
            dec!(0.6),
            dec!(0.8),
            dec!(0.1),
            "test".to_string(),
            format!("o_{}", uuid::Uuid::new_v4()),
        );
        r.outcome = Some(outcome);
        r.pnl = Some(if outcome { "5.0" } else { "-5.0" }.to_string());
        r.rsi = Some(rsi);
        r.skip_reason = Some("edge_too_low".to_string());
        r
    }

    #[test]
    fn compute_stats_basic() {
        let mut trades = Vec::new();
        for _ in 0..15 {
            trades.push(sample_shadow("btc", Direction::Yes, true, 55.0));
        }
        for _ in 0..5 {
            trades.push(sample_shadow("btc", Direction::Yes, false, 65.0));
        }

        let stats = compute_shadow_stats(&trades, "btc");
        assert_eq!(stats.trade_count, 20);
        assert!((stats.wr - 0.75).abs() < 0.01);
        assert!((stats.pnl - 50.0).abs() < 0.01);
    }

    #[test]
    fn wr_nudge_high_wr_lowers_threshold() {
        let cfg = ShadowCalibrationConfig::default();
        let result = wr_driven_nudge(0.06, 0.60, 0.06, &cfg);
        assert!(result.is_some());
        assert!(result.unwrap() < 0.06);
    }

    #[test]
    fn wr_nudge_low_wr_raises_threshold() {
        let cfg = ShadowCalibrationConfig::default();
        let result = wr_driven_nudge(0.06, 0.35, 0.06, &cfg);
        assert!(result.is_some());
        assert!(result.unwrap() > 0.06);
    }

    #[test]
    fn wr_nudge_mid_wr_returns_none() {
        let cfg = ShadowCalibrationConfig::default();
        let result = wr_driven_nudge(0.06, 0.50, 0.06, &cfg);
        assert!(result.is_none());
    }

    #[test]
    fn safety_bounds_clamp() {
        let cfg = ShadowCalibrationConfig {
            max_step_pct: 1.0,
            safety_bound_low: 0.5,
            safety_bound_high: 2.0,
            ..Default::default()
        };
        let result = safe_propose(0.06, 0.01, 0.06, &cfg);
        assert!(result >= 0.03);

        let result2 = safe_propose(0.06, 0.20, 0.06, &cfg);
        assert!(result2 <= 0.12);
    }

    #[test]
    fn step_limit_enforced() {
        let _cfg = ShadowCalibrationConfig {
            max_step_pct: 0.20,
            safety_bound_low: 0.1,
            safety_bound_high: 10.0,
            ..Default::default()
        };
        let result = clamp_step(1.0, 2.0, 0.20);
        assert!((result - 1.20).abs() < 1e-9);
    }

    #[test]
    fn overrides_apply_correctly() {
        let mut st = AssetStrategy {
            min_edge: dec!(0.06),
            min_confidence: dec!(0.70),
            yes_confidence_penalty: 0.0,
            no_confidence_penalty: 0.0,
            rsi_yes_max: 70.0,
            cluster_rsi_oversold: 40.0,
            cluster_rsi_overbought: 60.0,
            ..default_test_strategy()
        };

        let ov = CalibratedOverrides {
            min_edge: Some(0.05),
            rsi_yes_max: Some(75.0),
            ..Default::default()
        };

        apply_overrides(&mut st, &ov);
        let diff = (st.min_edge - dec!(0.05)).abs();
        assert!(diff < dec!(0.0001), "min_edge should be ~0.05, got {}", st.min_edge);
        assert_eq!(st.rsi_yes_max, 75.0);
        assert_eq!(st.min_confidence, dec!(0.70));
    }

    fn default_test_strategy() -> AssetStrategy {
        use crate::volatility::VolatilityFilterConfig;
        AssetStrategy {
            min_edge: dec!(0.06),
            min_confidence: dec!(0.70),
            min_order_usdc: dec!(5),
            min_order_usdc_floor: dec!(2),
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
            volatility_filter: VolatilityFilterConfig {
                min_std_pct: None,
                max_std_pct: None,
                sample_bars: 20,
            },
            htf_enabled: false,
            htf_interval: "15m".to_string(),
            htf_lookback: 50,
            htf_ema_period: 20,
            adaptive_thresholds: false,
            adaptive_trade_window: 50,
            min_secs_to_close: None,
            expiry_dampen_last_secs: None,
            min_market_yes_price: None,
            max_market_yes_price: None,
            min_liquidity_usdc: dec!(1000),
            cheap_token_price_threshold: dec!(0.15),
            cheap_token_max_usdc: dec!(5),
            large_order_usdc_hard_cap: None,
            volume_use_closed_candle_only: true,
            cluster_rsi_oversold: 40.0,
            cluster_rsi_overbought: 60.0,
            cluster_mom5_abs: 0.003,
            cluster_mom15_abs: 0.005,
            cluster_tie_min_edge_multiplier: 1.0,
            min_momentum_5m_abs: 0.0008,
            neutral_taker_edge_multiplier: 1.5,
            rsi_yes_max: 70.0,
            rsi_no_min: 30.0,
            min_macd_histogram_abs: 0.0,
            taker_direction_confirm: false,
            yes_confidence_penalty: 0.0,
            no_confidence_penalty: 0.0,
            direction_override_edge_fraction: 0.50,
            slippage_bps: dec!(0.002),
            max_secs_to_close: None,
            blocked_direction: None,
            dynamic_momentum_threshold: false,
            momentum_vol_reference: 0.03,
            adaptive_direction_penalty: false,
            adaptive_penalty_window: 10,
            multi_tf_enabled: false,
            multi_tf_interval: "5m".to_string(),
            multi_tf_lookback: 30,
            cluster_down_confidence_add: 0.0,
            mid_price_band_low: dec!(0.20),
            mid_price_band_high: dec!(0.35),
            mid_price_band_min_edge_multiplier: 1.0,
            taker_yes_min_ratio: 0.55,
            taker_no_max_ratio: 0.45,
            taker_neutral_low: 0.45,
            taker_neutral_high: 0.55,
        }
    }

    #[test]
    fn compute_live_asset_stats_basic() {
        use chrono::{Duration, Utc};
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("live_gate_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trades.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let base_time = Utc::now();
        for i in 0..4 {
            let mut t = TradeRecord::new(
                format!("c{}", i),
                "btc".to_string(),
                "15m".to_string(),
                Direction::Yes,
                dec!(0.5),
                dec!(5),
                dec!(10),
                dec!(0.6),
                dec!(0.8),
                dec!(0.1),
                "test".to_string(),
                format!("o{}", i),
            );
            t.outcome = Some(true);
            t.pnl = Some("2".into());
            t.resolved_at = Some(base_time + Duration::seconds(i));
            writeln!(f, "{}", serde_json::to_string(&t).unwrap()).unwrap();
        }
        for i in 0..6 {
            let mut t = TradeRecord::new(
                format!("c{}", i + 100),
                "btc".to_string(),
                "15m".to_string(),
                Direction::Yes,
                dec!(0.5),
                dec!(5),
                dec!(10),
                dec!(0.6),
                dec!(0.8),
                dec!(0.1),
                "test".to_string(),
                format!("ox{}", i),
            );
            t.outcome = Some(false);
            t.pnl = Some("-3".into());
            t.resolved_at = Some(base_time + Duration::seconds(20 + i));
            writeln!(f, "{}", serde_json::to_string(&t).unwrap()).unwrap();
        }

        let g = compute_live_asset_stats(path.to_str().unwrap(), "btc", 10).unwrap();
        assert_eq!(g.count, 10);
        assert!((g.wr - 0.4).abs() < 1e-9);
        let expected_pnl = 4.0 * 2.0 + 6.0 * (-3.0);
        assert!((g.pnl - expected_pnl).abs() < 1e-9);
    }

    #[test]
    fn compute_live_direction_stats_yes_no() {
        use chrono::{Duration, Utc};
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("live_dir_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trades.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let base_time = Utc::now();
        let mut ts = 0i64;
        for _ in 0..3 {
            let mut t = TradeRecord::new(
                format!("cy{}", ts),
                "eth".to_string(),
                "15m".to_string(),
                Direction::Yes,
                dec!(0.5),
                dec!(5),
                dec!(10),
                dec!(0.6),
                dec!(0.8),
                dec!(0.1),
                "test".to_string(),
                format!("oy{}", ts),
            );
            t.outcome = Some(true);
            t.resolved_at = Some(base_time + Duration::seconds(ts));
            writeln!(f, "{}", serde_json::to_string(&t).unwrap()).unwrap();
            ts += 1;
        }
        let mut t = TradeRecord::new(
            format!("cy{}", ts),
            "eth".to_string(),
            "15m".to_string(),
            Direction::Yes,
            dec!(0.5),
            dec!(5),
            dec!(10),
            dec!(0.6),
            dec!(0.8),
            dec!(0.1),
            "test".to_string(),
            format!("oy{}", ts),
        );
        t.outcome = Some(false);
        t.resolved_at = Some(base_time + Duration::seconds(ts));
        writeln!(f, "{}", serde_json::to_string(&t).unwrap()).unwrap();
        ts += 1;

        for _ in 0..2 {
            let mut t = TradeRecord::new(
                format!("cn{}", ts),
                "eth".to_string(),
                "15m".to_string(),
                Direction::No,
                dec!(0.5),
                dec!(5),
                dec!(10),
                dec!(0.6),
                dec!(0.8),
                dec!(0.1),
                "test".to_string(),
                format!("on{}", ts),
            );
            t.outcome = Some(false);
            t.resolved_at = Some(base_time + Duration::seconds(ts));
            writeln!(f, "{}", serde_json::to_string(&t).unwrap()).unwrap();
            ts += 1;
        }
        for _ in 0..2 {
            let mut t = TradeRecord::new(
                format!("cn{}", ts),
                "eth".to_string(),
                "15m".to_string(),
                Direction::No,
                dec!(0.5),
                dec!(5),
                dec!(10),
                dec!(0.6),
                dec!(0.8),
                dec!(0.1),
                "test".to_string(),
                format!("on{}", ts),
            );
            t.outcome = Some(true);
            t.resolved_at = Some(base_time + Duration::seconds(ts));
            writeln!(f, "{}", serde_json::to_string(&t).unwrap()).unwrap();
            ts += 1;
        }

        let d = compute_live_direction_stats(path.to_str().unwrap(), "eth", 10).unwrap();
        assert_eq!(d.yes_count, 4);
        assert_eq!(d.no_count, 4);
        assert!((d.yes_wr - 0.75).abs() < 1e-9);
        assert!((d.no_wr - 0.50).abs() < 1e-9);
    }

    #[test]
    fn live_veto_strips_loosening_min_edge() {
        let cfg = ShadowCalibrationConfig::default();
        let live = LiveAssetGate {
            wr: 0.35,
            pnl: -5.0,
            count: 5,
        };
        let base = BaseSnapshot::from_strategy(&default_test_strategy());
        let current = CalibratedOverrides::default();
        let mut delta = CalibratedOverrides {
            min_edge: Some(0.05),
            ..Default::default()
        };
        assert!(filter_live_veto_loosening(
            &mut delta,
            &current,
            &base,
            &cfg,
            Some(&live)
        ));
        assert!(delta.min_edge.is_none());
    }

    #[test]
    fn live_veto_keeps_tightening_min_edge() {
        let cfg = ShadowCalibrationConfig::default();
        let live = LiveAssetGate {
            wr: 0.35,
            pnl: -5.0,
            count: 5,
        };
        let base = BaseSnapshot::from_strategy(&default_test_strategy());
        let current = CalibratedOverrides::default();
        let mut delta = CalibratedOverrides {
            min_edge: Some(0.08),
            ..Default::default()
        };
        let cleared = filter_live_veto_loosening(
            &mut delta,
            &current,
            &base,
            &cfg,
            Some(&live)
        );
        assert!(!cleared, "tightening proposals should not be stripped");
        assert_eq!(delta.min_edge, Some(0.08));
    }

    #[test]
    fn strip_penalty_loosening_for_live_direction_clears_bad_yes_live() {
        let cfg = ShadowCalibrationConfig::default();
        let base = BaseSnapshot::from_strategy(&default_test_strategy());
        let current = CalibratedOverrides {
            yes_confidence_penalty: Some(0.10),
            ..Default::default()
        };
        let mut ov = CalibratedOverrides {
            yes_confidence_penalty: Some(0.02),
            ..Default::default()
        };
        let live = LiveDirectionStats {
            yes_wr: 0.2,
            no_wr: 0.6,
            yes_count: 4,
            no_count: 3,
        };
        strip_penalty_loosening_for_live_direction(&mut ov, &current, &base, &cfg, Some(&live));
        assert!(ov.yes_confidence_penalty.is_none());
    }

    #[test]
    fn strip_penalty_loosening_keeps_when_live_direction_ok() {
        let cfg = ShadowCalibrationConfig::default();
        let base = BaseSnapshot::from_strategy(&default_test_strategy());
        let current = CalibratedOverrides {
            yes_confidence_penalty: Some(0.10),
            ..Default::default()
        };
        let mut ov = CalibratedOverrides {
            yes_confidence_penalty: Some(0.02),
            ..Default::default()
        };
        let live = LiveDirectionStats {
            yes_wr: 0.55,
            no_wr: 0.5,
            yes_count: 4,
            no_count: 3,
        };
        strip_penalty_loosening_for_live_direction(&mut ov, &current, &base, &cfg, Some(&live));
        assert_eq!(ov.yes_confidence_penalty, Some(0.02));
    }

    #[test]
    fn classify_override_direction_loosen_vs_tighten() {
        let base = BaseSnapshot::from_strategy(&default_test_strategy());
        let cur = CalibratedOverrides::default();
        let loosen = CalibratedOverrides {
            min_edge: Some(0.05),
            ..Default::default()
        };
        assert_eq!(classify_override_direction(&loosen, &cur, &base), "loosen");
        let tighten = CalibratedOverrides {
            min_edge: Some(0.08),
            ..Default::default()
        };
        assert_eq!(classify_override_direction(&tighten, &cur, &base), "tighten");
    }
}
