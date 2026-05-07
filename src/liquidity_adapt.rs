//! Per-asset `min_liquidity_usdc` adaptation from recent `skip_reasons.jsonl` windows.
//!
//! Uses a rolling **last-N-skips** view per asset (not time-based) so sample sizes stay stable
//! across quiet and busy periods.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use rust_decimal::prelude::ToPrimitive;
use tracing::{debug, info};

use crate::config::{AssetStrategy, LiquidityAdaptConfig};
use crate::metrics::SkipRecord;
use crate::shadow_calibrator::{
    compute_live_asset_stats, AssetCalibrationState, BaseSnapshot, CalibratedOverrides,
    CalibrationStateFile, LiveAssetGate, ShadowCalibrationConfig,
};

const LIQUIDITY_SKIP_REASON: &str = "liquidity_too_low";
const EPS_EQ: f64 = 0.01;
const EPS_STEP: f64 = 0.5;

/// Returns `true` if `calibration_state.json` should be persisted.
#[must_use]
pub fn maybe_adapt_liquidity(
    cycle: u64,
    state: &mut CalibrationStateFile,
    data_dir: &str,
    base_strategies: &HashMap<String, AssetStrategy>,
    assets: &[String],
    config: &LiquidityAdaptConfig,
    shadow_cfg: &ShadowCalibrationConfig,
    strategy_version: &str,
) -> bool {
    if !config.enabled {
        return false;
    }

    let path = Path::new(data_dir).join("skip_reasons.jsonl");
    let records = match read_skip_tail(&path, config.tail_read_bytes) {
        Ok(r) => r,
        Err(e) => {
            debug!(
                error = %e,
                path = %path.display(),
                "liquidity_adapt: skip file read failed"
            );
            return false;
        }
    };

    if records.is_empty() {
        debug!("liquidity_adapt: no skip records in tail");
        return false;
    }

    let trades_path = format!("{}/trades.jsonl", data_dir.trim_end_matches('/'));
    let mut any_changed = false;

    for asset in assets {
        if let Some(row) = state.assets.get(asset) {
            if row.rolled_back {
                continue;
            }
        }

        if let Some(&last) = state.liquidity_adapt_last_cycle.get(asset) {
            // `wrapping_sub` counts elapsed cycles correctly if `cycle` overflows u64;
            // `saturating_sub` would yield 0 when `cycle < last` and block forever.
            if cycle.wrapping_sub(last) < config.cooldown_cycles {
                continue;
            }
        }

        let Some(base_st) = base_strategies.get(asset) else {
            continue;
        };

        let window = recent_skips_for_asset(&records, asset, config.window_n);
        if window.len() < config.min_skips_in_window {
            continue;
        }

        let liq_n = window
            .iter()
            .filter(|s| s.reason == LIQUIDITY_SKIP_REASON)
            .count();
        let ratio = liq_n as f64 / window.len() as f64;

        let base_liq = base_st
            .min_liquidity_usdc
            .to_f64()
            .unwrap_or(3_000.0);
        let cap_liq = base_liq * config.ceiling_multiplier;
        let floor_f = config.floor_usdc.to_f64().unwrap_or(800.0);

        ensure_asset_row(state, asset, base_st, strategy_version);
        let row = state
            .assets
            .get_mut(asset)
            .expect("ensure_asset_row inserts");

        let eff = row
            .applied_overrides
            .min_liquidity_usdc
            .unwrap_or(base_liq);

        let live = compute_live_asset_stats(&trades_path, asset, shadow_cfg.live_veto_window);

        if ratio >= config.loosen_share_threshold {
            if live_loosening_blocked(live.as_ref(), shadow_cfg) {
                debug!(asset = %asset, ratio, "liquidity_adapt: skip loosen (live veto)");
                continue;
            }
            let proposed = eff * (1.0 - config.step_down_pct);
            let clamped = proposed.max(floor_f).min(cap_liq);
            if (clamped - eff).abs() < EPS_STEP {
                continue;
            }
            row.applied_overrides.min_liquidity_usdc = if (clamped - base_liq).abs() <= EPS_EQ {
                None
            } else {
                Some(clamped)
            };
            state.liquidity_adapt_last_cycle.insert(asset.clone(), cycle);
            any_changed = true;
            info!(
                asset = %asset,
                action = "loosen",
                ratio = format!("{:.3}", ratio),
                liq_skips = liq_n,
                window = window.len(),
                old = eff,
                new = clamped,
                "liquidity_adapt: adjusted min_liquidity_usdc"
            );
            continue;
        }

        if ratio <= config.tighten_share_threshold {
            let proposed = eff * (1.0 + config.step_up_pct);
            let clamped = proposed.max(floor_f).min(cap_liq);
            if (clamped - base_liq).abs() <= EPS_EQ {
                if row.applied_overrides.min_liquidity_usdc.is_some() {
                    row.applied_overrides.min_liquidity_usdc = None;
                    state.liquidity_adapt_last_cycle.insert(asset.clone(), cycle);
                    any_changed = true;
                    info!(
                        asset = %asset,
                        ratio = format!("{:.3}", ratio),
                        liq_skips = liq_n,
                        window = window.len(),
                        "liquidity_adapt: cleared min_liquidity override (tighten to base)"
                    );
                }
                continue;
            }
            // Match loosen branch: skip only when clamping yields no material change.
            // `clamped <= eff + EPS_STEP` wrongly skipped when `cap_liq` pulled `clamped`
            // below `eff` (enforce ceiling), stalling until another path moved `eff`.
            if (clamped - eff).abs() < EPS_STEP {
                continue;
            }
            row.applied_overrides.min_liquidity_usdc = Some(clamped);
            state.liquidity_adapt_last_cycle.insert(asset.clone(), cycle);
            any_changed = true;
            info!(
                asset = %asset,
                action = "tighten",
                ratio = format!("{:.3}", ratio),
                liq_skips = liq_n,
                window = window.len(),
                old = eff,
                new = clamped,
                "liquidity_adapt: adjusted min_liquidity_usdc"
            );
        }
    }

    any_changed
}

fn live_loosening_blocked(live: Option<&LiveAssetGate>, cfg: &ShadowCalibrationConfig) -> bool {
    let Some(l) = live else {
        return false;
    };
    if !cfg.live_veto_enabled {
        return false;
    }
    if l.count < cfg.live_veto_min_trades {
        return false;
    }
    if l.pnl < cfg.live_veto_pnl_threshold {
        return true;
    }
    l.wr < cfg.live_veto_soft_wr
}

fn ensure_asset_row(
    state: &mut CalibrationStateFile,
    asset: &str,
    base: &AssetStrategy,
    strategy_version: &str,
) {
    if state.assets.contains_key(asset) {
        return;
    }
    state.assets.insert(
        asset.to_string(),
        AssetCalibrationState {
            last_calibrated_at: Utc::now(),
            shadow_wr: 0.0,
            shadow_pnl: 0.0,
            shadow_trade_count: 0,
            calibration_version: state.global_version,
            strategy_version: Some(strategy_version.to_string()),
            applied_overrides: CalibratedOverrides::default(),
            base_snapshot: BaseSnapshot::from_strategy(base),
            trade_count_since_calibration: 0,
            rolled_back: false,
        },
    );
}

fn recent_skips_for_asset<'a>(records: &'a [SkipRecord], asset: &str, n: usize) -> Vec<&'a SkipRecord> {
    let mut v: Vec<&SkipRecord> = records.iter().filter(|r| r.asset == asset).collect();
    v.sort_by_key(|r| r.timestamp);
    if v.len() > n {
        let drop = v.len() - n;
        v.drain(0..drop);
    }
    v
}

fn read_skip_tail(path: &Path, max_bytes: u64) -> Result<Vec<SkipRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max_bytes.max(1));
    f.seek(SeekFrom::Start(start))
        .with_context(|| format!("seek {}", path.display()))?;
    let mut reader = BufReader::new(f);
    let mut first = String::new();
    if start > 0 {
        let _ = reader.read_line(&mut first)?;
    }
    let mut rest = String::new();
    reader
        .read_to_string(&mut rest)
        .with_context(|| format!("read {}", path.display()))?;
    let content = first + &rest;

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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn skip(asset: &str, reason: &str, ts_mins: i64) -> SkipRecord {
        SkipRecord {
            timestamp: Utc
                .with_ymd_and_hms(2026, 5, 5, 12, 0, 0)
                .unwrap()
                + chrono::Duration::minutes(ts_mins),
            condition_id: "c".into(),
            asset: asset.into(),
            duration: "15m".into(),
            question: "q".into(),
            reason: reason.into(),
            details: None,
        }
    }

    #[test]
    fn recent_skips_takes_last_n_per_asset() {
        let mut v = vec![];
        for i in 0..200 {
            v.push(skip("eth", "liquidity_too_low", i));
        }
        v.push(skip("eth", "other", 201));
        let w = recent_skips_for_asset(&v, "eth", 150);
        assert_eq!(w.len(), 150);
        // Window is last 150 by time: minutes 51..199 (liquidity) + minute 201 (other).
        assert_eq!(w.last().unwrap().reason, "other");
    }

    #[test]
    fn cooldown_elapsed_uses_wrapping_sub_not_saturating() {
        let last = u64::MAX - 2;
        let cycle = 5_u64;
        // Bug: saturating_sub treats wrap as "0 elapsed" and would block adaptation forever.
        assert_eq!(cycle.saturating_sub(last), 0);
        let elapsed = cycle.wrapping_sub(last);
        assert!(elapsed >= 8, "expected wrap-around distance, got {elapsed}");
    }

    #[test]
    fn live_blocked_when_weak_wr() {
        let cfg = ShadowCalibrationConfig {
            live_veto_enabled: true,
            live_veto_min_trades: 3,
            live_veto_soft_wr: 0.50,
            live_veto_pnl_threshold: -10.0,
            ..ShadowCalibrationConfig::default()
        };
        let live = LiveAssetGate {
            wr: 0.40,
            pnl: 0.0,
            count: 5,
        };
        assert!(live_loosening_blocked(Some(&live), &cfg));
    }
}
