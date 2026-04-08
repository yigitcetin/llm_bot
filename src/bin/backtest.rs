//! CLI: Monte Carlo + walk-forward on resolved `trades.jsonl` (P7).
//!
//! Usage:
//!   cargo run --bin backtest -- [PATH] [ITERATIONS] [--asset btc] [--direction YES] [--min-edge F] [--max-edge F] [--min-rsi F] [--max-rsi F]

use anyhow::{Context, Result};
use polymarket_llm_bot::backtest::{
    load_resolved_trade_rows, monte_carlo_total_pnl, walk_forward_fold_details,
    walk_forward_fold_sums, PnlSummary, TradeFilter,
};
use rust_decimal::prelude::ToPrimitive;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let mut path = "data/trades.jsonl".to_string();
    let mut iterations: usize = 10_000;
    let mut i = 1usize;

    if let Some(a) = args.get(1) {
        if !a.starts_with("--") {
            path = a.clone();
            i = 2;
            if let Some(b) = args.get(2) {
                if !b.starts_with("--") {
                    if let Ok(it) = b.parse::<usize>() {
                        iterations = it;
                        i = 3;
                    }
                }
            }
        }
    }

    let mut filter = TradeFilter::default();
    while i + 1 < args.len() {
        match args[i].as_str() {
            "--asset" => {
                filter.asset = Some(args[i + 1].clone());
                i += 2;
            }
            "--direction" => {
                filter.direction = Some(args[i + 1].to_uppercase());
                i += 2;
            }
            "--min-edge" => {
                filter.min_edge = Some(args[i + 1].parse().context("--min-edge must be a number")?);
                i += 2;
            }
            "--max-edge" => {
                filter.max_edge = Some(args[i + 1].parse().context("--max-edge must be a number")?);
                i += 2;
            }
            "--min-rsi" => {
                filter.min_rsi = Some(args[i + 1].parse().context("--min-rsi must be a number")?);
                i += 2;
            }
            "--max-rsi" => {
                filter.max_rsi = Some(args[i + 1].parse().context("--max-rsi must be a number")?);
                i += 2;
            }
            "--help" | "-h" => {
                eprintln!(
                    "Usage: backtest [PATH] [ITERATIONS] [--asset S] [--direction YES|NO] \\\n\
                     ~~~~~~~~~~~~ [--min-edge F] [--max-edge F] [--min-rsi F] [--max-rsi F]"
                );
                return Ok(());
            }
            other => {
                anyhow::bail!("unknown argument: {}", other);
            }
        }
    }

    let mut rows = load_resolved_trade_rows(&path).with_context(|| format!("load {}", path))?;
    if filter.is_active() {
        rows.retain(|r| filter.matches(r));
    }

    let trades: Vec<f64> = rows
        .iter()
        .filter_map(|r| {
            r.pnl
                .as_ref()
                .and_then(|p| p.parse::<rust_decimal::Decimal>().ok())
        })
        .map(|d: rust_decimal::Decimal| d.to_f64().unwrap_or(0.0))
        .collect();

    if trades.is_empty() {
        eprintln!(
            "No resolved PnL rows in {} (after filter) — wait for resolutions or relax filters.",
            path
        );
        return Ok(());
    }

    let total: f64 = trades.iter().sum();
    println!(
        "Resolved trades (PnL rows): {}  total PnL: {:.4}{}",
        trades.len(),
        total,
        if filter.is_active() {
            "  [filter active]"
        } else {
            ""
        }
    );

    let folds = 5;
    let wf = walk_forward_fold_sums(&trades, folds);
    println!("Walk-forward ({} folds, chronological chunks):", folds);
    for (i, s) in wf.iter().enumerate() {
        println!("  fold {}: sum_pnl={:.4}", i + 1, s);
    }

    let details = walk_forward_fold_details(&rows, folds);
    println!("Per-fold (outcomes in chunk):");
    for (i, d) in details.iter().enumerate() {
        let wr = if d.n_with_outcome > 0 {
            100.0 * d.wins as f64 / d.n_with_outcome as f64
        } else {
            0.0
        };
        let me = d
            .mean_edge
            .map(|x| format!("{:.4}", x))
            .unwrap_or_else(|| "n/a".to_string());
        println!(
            "  fold {}: sum_pnl={:.4}  win_rate={:.1}% ({}/{})  mean_edge={}",
            i + 1,
            d.sum_pnl,
            wr,
            d.wins,
            d.n_with_outcome,
            me
        );
    }

    let mc = monte_carlo_total_pnl(&trades, iterations, 42);
    if let Some(summary) = PnlSummary::from_samples(mc) {
        println!(
            "Monte Carlo bootstrap ({} iters, resample with replacement, same N as history):",
            iterations
        );
        println!(
            "  mean={:.4}  p50={:.4}  p05={:.4}  p95={:.4}",
            summary.mean, summary.p50, summary.p05, summary.p95
        );
    }

    Ok(())
}
