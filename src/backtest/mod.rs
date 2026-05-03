//! Offline analysis: Monte Carlo bootstrap on resolved trades and simple walk-forward splits.
//!
//! **P7 (plan):** Use historical `trades.jsonl` (resolved rows) to stress-test PnL stability.

use anyhow::{Context, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader};

/// One row from `trades.jsonl` with optional resolution (extra fields ignored if absent in older JSONL).
#[derive(Debug, Clone, Deserialize)]
pub struct ResolvedTradeRow {
    #[serde(default)]
    pub pnl: Option<String>,
    #[serde(default)]
    pub outcome: Option<bool>,
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub edge: Option<String>,
    #[serde(default)]
    pub asset: Option<String>,
    #[serde(default)]
    pub rsi: Option<f64>,
}

/// Optional filters for subset Monte Carlo (all `None` = no filter).
#[derive(Debug, Clone, Default)]
pub struct TradeFilter {
    pub asset: Option<String>,
    /// `YES` or `NO`
    pub direction: Option<String>,
    pub min_edge: Option<f64>,
    pub max_edge: Option<f64>,
    pub min_rsi: Option<f64>,
    pub max_rsi: Option<f64>,
}

impl TradeFilter {
    /// Returns true if the row passes all set filters (resolved rows with `pnl` only).
    pub fn matches(&self, row: &ResolvedTradeRow) -> bool {
        if row.pnl.is_none() {
            return false;
        }
        if let Some(ref a) = self.asset {
            let row_a = row.asset.as_deref().unwrap_or("").trim().to_lowercase();
            if row_a != a.trim().to_lowercase() {
                return false;
            }
        }
        if let Some(ref d) = self.direction {
            let row_d = row.direction.as_deref().unwrap_or("").trim().to_uppercase();
            if row_d != d.trim().to_uppercase() {
                return false;
            }
        }
        if let Some(min_e) = self.min_edge {
            let e = row
                .edge
                .as_deref()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(f64::NAN);
            if !e.is_finite() || e < min_e {
                return false;
            }
        }
        if let Some(max_e) = self.max_edge {
            let e = row
                .edge
                .as_deref()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(f64::NAN);
            if !e.is_finite() || e > max_e {
                return false;
            }
        }
        if let Some(min_r) = self.min_rsi {
            let r = row.rsi.unwrap_or(f64::NAN);
            if !r.is_finite() || r < min_r {
                return false;
            }
        }
        if let Some(max_r) = self.max_rsi {
            let r = row.rsi.unwrap_or(f64::NAN);
            if !r.is_finite() || r > max_r {
                return false;
            }
        }
        true
    }

    /// True if any filter field is set (subset Monte Carlo / walk-forward).
    pub fn is_active(&self) -> bool {
        self.asset.is_some()
            || self.direction.is_some()
            || self.min_edge.is_some()
            || self.max_edge.is_some()
            || self.min_rsi.is_some()
            || self.max_rsi.is_some()
    }
}

/// Per-fold stats (chronological chunks of resolved-with-PnL rows).
#[derive(Debug, Clone)]
pub struct FoldDetail {
    pub sum_pnl: f64,
    /// Rows in fold with `outcome` set (for win rate).
    pub n_with_outcome: usize,
    pub wins: usize,
    pub mean_edge: Option<f64>,
}

fn trade_row_won(row: &ResolvedTradeRow) -> Option<bool> {
    let outcome = row.outcome?;
    let dir = row.direction.as_deref()?;
    Some(matches!((dir, outcome), ("YES", true) | ("NO", false)))
}

/// Summary stats for a set of PnL samples.
#[derive(Debug, Clone)]
pub struct PnlSummary {
    pub n: usize,
    pub mean: f64,
    pub p50: f64,
    pub p05: f64,
    pub p95: f64,
}

impl PnlSummary {
    pub fn from_samples(mut xs: Vec<f64>) -> Option<Self> {
        if xs.is_empty() {
            return None;
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = xs.len();
        let mean = xs.iter().sum::<f64>() / n as f64;
        let p50 = percentile_sorted(&xs, 0.50);
        let p05 = percentile_sorted(&xs, 0.05);
        let p95 = percentile_sorted(&xs, 0.95);
        Some(Self {
            n,
            mean,
            p50,
            p05,
            p95,
        })
    }
}

fn percentile_sorted(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn row_pnl_f64(row: &ResolvedTradeRow) -> Option<f64> {
    let p = row.pnl.as_ref()?;
    let d: Decimal = p.parse().ok()?;
    Some(d.to_f64().unwrap_or(0.0))
}

/// Load rows that have a resolved `pnl` field (parseable USDC).
pub fn load_resolved_trade_rows(path: &str) -> Result<Vec<ResolvedTradeRow>> {
    let f = File::open(path).with_context(|| format!("open trades file: {}", path))?;
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: ResolvedTradeRow = serde_json::from_str(line).with_context(|| {
            format!(
                "parse trade line: {}",
                line.chars().take(120).collect::<String>()
            )
        })?;
        if row.pnl.is_some() {
            out.push(row);
        }
    }
    Ok(out)
}

/// **Monte Carlo:** bootstrap resample trades with replacement `iterations` times; each iteration sums `n` trades.
pub fn monte_carlo_total_pnl(trades: &[f64], iterations: usize, seed: u64) -> Vec<f64> {
    if trades.is_empty() || iterations == 0 {
        return Vec::new();
    }
    let n = trades.len();
    let mut out = Vec::with_capacity(iterations);
    // Simple deterministic PRNG (splitmix64) — good enough for analysis tooling.
    let mut state = seed;
    for _ in 0..iterations {
        let mut sum = 0.0;
        for _ in 0..n {
            state = state.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z = z ^ (z >> 31);
            let idx = (z as usize) % n;
            sum += trades[idx];
        }
        out.push(sum);
    }
    out
}

/// Walk-forward: split `trades` by chronological order into `folds` contiguous segments; return per-fold sums.
/// Requires caller to pass trades in time order (JSONL append order is OK).
pub fn walk_forward_fold_sums(trades: &[f64], folds: usize) -> Vec<f64> {
    if folds == 0 || trades.is_empty() {
        return Vec::new();
    }
    let chunk = (trades.len() + folds - 1) / folds;
    let mut sums = Vec::with_capacity(folds);
    for f in 0..folds {
        let start = f * chunk;
        if start >= trades.len() {
            break;
        }
        let end = (start + chunk).min(trades.len());
        let s: f64 = trades[start..end].iter().sum();
        sums.push(s);
    }
    sums
}

/// Walk-forward with win rate and mean edge per fold (uses full rows; PnL summed for all rows with `pnl` in chunk).
pub fn walk_forward_fold_details(rows: &[ResolvedTradeRow], folds: usize) -> Vec<FoldDetail> {
    if folds == 0 || rows.is_empty() {
        return Vec::new();
    }
    let chunk = (rows.len() + folds - 1) / folds;
    let mut out = Vec::with_capacity(folds);
    for f in 0..folds {
        let start = f * chunk;
        if start >= rows.len() {
            break;
        }
        let end = (start + chunk).min(rows.len());
        let slice = &rows[start..end];
        let mut sum_pnl = 0.0;
        let mut n_outcome = 0usize;
        let mut wins = 0usize;
        let mut edge_sum = 0.0;
        let mut edge_n = 0usize;
        for r in slice {
            if let Some(p) = row_pnl_f64(r) {
                sum_pnl += p;
            }
            if r.outcome.is_some() {
                n_outcome += 1;
                if trade_row_won(r).unwrap_or(false) {
                    wins += 1;
                }
            }
            if let Some(e) = r.edge.as_ref().and_then(|s| s.parse::<f64>().ok()) {
                edge_sum += e;
                edge_n += 1;
            }
        }
        out.push(FoldDetail {
            sum_pnl,
            n_with_outcome: n_outcome,
            wins,
            mean_edge: if edge_n > 0 {
                Some(edge_sum / edge_n as f64)
            } else {
                None
            },
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monte_carlo_non_empty() {
        let t = vec![1.0, -2.0, 3.0];
        let v = monte_carlo_total_pnl(&t, 100, 42);
        assert_eq!(v.len(), 100);
    }

    #[test]
    fn walk_forward_splits() {
        let t: Vec<f64> = (1..=10).map(|i| i as f64).collect();
        let s = walk_forward_fold_sums(&t, 3);
        assert_eq!(s.len(), 3);
        assert!((s.iter().sum::<f64>() - 55.0).abs() < 1e-9);
    }

    #[test]
    fn trade_filter_asset() {
        let mut f = TradeFilter::default();
        f.asset = Some("btc".to_string());
        let row = ResolvedTradeRow {
            pnl: Some("1".to_string()),
            outcome: Some(true),
            direction: Some("YES".to_string()),
            edge: Some("0.1".to_string()),
            asset: Some("btc".to_string()),
            rsi: Some(40.0),
        };
        assert!(f.matches(&row));
        let row_eth = ResolvedTradeRow {
            asset: Some("eth".to_string()),
            ..row.clone()
        };
        assert!(!f.matches(&row_eth));
    }
}
