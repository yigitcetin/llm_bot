//! CLI: Monte Carlo + walk-forward on resolved `trades.jsonl` (P7).

use anyhow::{Context, Result};
use polymarket_llm_bot::backtest::{
    load_resolved_pnls, monte_carlo_total_pnl, walk_forward_fold_sums, PnlSummary,
};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "data/trades.jsonl".to_string());
    let iterations: usize = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);

    let trades = load_resolved_pnls(&path)
        .with_context(|| format!("load {}", path))?;
    if trades.is_empty() {
        eprintln!("No resolved PnL rows in {} — wait for resolutions or pass another file.", path);
        return Ok(());
    }

    let total: f64 = trades.iter().sum();
    println!("Resolved trades: {}  total PnL: {:.4}", trades.len(), total);

    let folds = 5;
    let wf = walk_forward_fold_sums(&trades, folds);
    println!("Walk-forward ({} folds, chronological chunks):", folds);
    for (i, s) in wf.iter().enumerate() {
        println!("  fold {}: sum_pnl={:.4}", i + 1, s);
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
