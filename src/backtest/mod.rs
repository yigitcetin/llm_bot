//! Offline analysis: Monte Carlo bootstrap on resolved trades and simple walk-forward splits.
//!
//! **P7 (plan):** Use historical `trades.jsonl` (resolved rows) to stress-test PnL stability.

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader};

/// One row from `trades.jsonl` with optional resolution.
#[derive(Debug, Clone, Deserialize)]
pub struct ResolvedTradeRow {
    #[serde(default)]
    pub pnl: Option<String>,
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

/// Load resolved PnL values (USDC) from a JSONL file.
pub fn load_resolved_pnls(path: &str) -> Result<Vec<f64>> {
    let f = File::open(path).with_context(|| format!("open trades file: {}", path))?;
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: ResolvedTradeRow = serde_json::from_str(line)
            .with_context(|| format!("parse trade line: {}", line.chars().take(120).collect::<String>()))?;
        if let Some(p) = row.pnl {
            let d: Decimal = p.parse().context("parse pnl decimal")?;
            out.push(d.to_f64().unwrap_or(0.0));
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
}
