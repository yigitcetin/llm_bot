//! Offline research CLI: backtest and walk-forward on historical spot candles (default: Binance).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use polymarket_llm_bot::backtest::{BacktestConfig, run_backtest};
use polymarket_llm_bot::spot_price::{Candle, SpotPriceClient};
use polymarket_llm_bot::walk_forward::{WalkForwardConfig, run_walk_forward};

#[derive(Parser)]
#[command(name = "research", about = "Backtest and walk-forward analysis on spot candles", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Single backtest over recent candles (fetches from exchange)
    Backtest {
        #[arg(long, default_value = "btc")]
        asset: String,
        #[arg(long, default_value = "binance")]
        exchange: String,
        #[arg(long, default_value = "1m")]
        interval: String,
        /// Number of candles to fetch (Binance pages automatically beyond 1000 per HTTP call)
        #[arg(long, default_value_t = 500)]
        limit: usize,
        #[arg(long, default_value_t = 100)]
        lookback: usize,
        #[arg(long, default_value_t = 5)]
        holding_period: usize,
        /// Print full JSON result
        #[arg(long)]
        json: bool,
    },
    /// Walk-forward train/test windows over historical candles
    WalkForward {
        #[arg(long, default_value = "btc")]
        asset: String,
        #[arg(long, default_value = "binance")]
        exchange: String,
        #[arg(long, default_value = "1m")]
        interval: String,
        #[arg(long, default_value_t = 1000)]
        limit: usize,
        #[arg(long, default_value_t = 400)]
        train_window: usize,
        #[arg(long, default_value_t = 300)]
        test_window: usize,
        #[arg(long, default_value_t = 150)]
        step_size: usize,
        #[arg(long, default_value_t = 5)]
        holding_period: usize,
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Backtest {
            asset,
            exchange,
            interval,
            limit,
            lookback,
            holding_period,
            json,
        } => {
            let candles = fetch_candles(&exchange, &asset, &interval, limit).await?;
            let n = candles.len();
            let config = BacktestConfig::default();

            let result = run_backtest(&candles, &asset, &config, lookback, holding_period)
                .context("backtest failed — check lookback/holding_period vs candle count")?;

            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                print_backtest_summary(&result, n, &interval);
            }
        }
        Commands::WalkForward {
            asset,
            exchange,
            interval,
            limit,
            train_window,
            test_window,
            step_size,
            holding_period,
            json,
        } => {
            let required = train_window + test_window;
            let candles = fetch_candles(&exchange, &asset, &interval, limit).await?;
            let n = candles.len();
            if n < required {
                anyhow::bail!(
                    "need at least {} candles (train_window + test_window), got {}",
                    required,
                    n
                );
            }

            let wf_config = WalkForwardConfig {
                train_window,
                test_window,
                step_size,
                holding_period,
                ..WalkForwardConfig::default()
            };

            let result = run_walk_forward(&candles, &asset, &wf_config)
                .context("walk-forward failed")?;

            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                print_walk_forward_summary(&result, n, &interval);
            }
        }
    }

    Ok(())
}

async fn fetch_candles(
    exchange: &str,
    asset: &str,
    interval: &str,
    limit: usize,
) -> Result<Vec<Candle>> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("HTTP client build failed")?;

    let client = SpotPriceClient::new(http, exchange.to_string());
    client
        .fetch_candles(asset, interval, limit)
        .await
        .with_context(|| format!("fetch_candles {exchange} {asset} {interval}"))
}

fn print_backtest_summary(
    result: &polymarket_llm_bot::backtest::BacktestResult,
    candle_count: usize,
    interval: &str,
) {
    let m = &result.metrics;
    println!("Backtest summary");
    println!(
        "  candles used: {} (interval {}) — approx {:.2} days of history",
        candle_count,
        interval,
        approx_interval_days(candle_count, interval)
    );
    println!("  trades: {}", m.total_trades);
    println!(
        "  vol filter: {} bars skipped / {} with signal ({:.2}%)",
        result.volatility_filter_skips,
        result.bars_with_signal,
        if result.bars_with_signal > 0 {
            100.0 * result.volatility_filter_skips as f64 / result.bars_with_signal as f64
        } else {
            0.0
        }
    );
    println!(
        "  volume low skips: {} | no clear signal skips: {}",
        result.volume_low_skips, result.no_clear_signal_skips
    );
    println!("  win rate: {:.2}%", m.win_rate * 100.0);
    println!("  total PnL: {}", m.total_pnl);
    println!("  total return: {:.2}%", m.total_return_pct);
    println!("  Sharpe: {:.3}", m.sharpe_ratio);
    println!("  max drawdown: {:.2}%", m.max_drawdown_pct);
    println!("  profit factor: {:.2}", m.profit_factor);
    println!("  duration (days): {:.2}", m.duration_days);
}

fn print_walk_forward_summary(
    result: &polymarket_llm_bot::walk_forward::WalkForwardResult,
    candle_count: usize,
    interval: &str,
) {
    let a = &result.aggregate_metrics;
    println!("Walk-forward summary");
    println!(
        "  candles used: {} (interval {}) — approx {:.2} days of history",
        candle_count,
        interval,
        approx_interval_days(candle_count, interval)
    );
    println!("  iterations: {}", a.total_iterations);
    println!("  avg test Sharpe: {:.3}", a.avg_test_sharpe);
    println!("  avg test win rate: {:.2}%", a.avg_test_win_rate * 100.0);
    println!("  avg test return: {:.2}%", a.avg_test_return);
    println!("  total test trades: {}", a.total_test_trades);
    println!("  cumulative test PnL: {}", a.cumulative_pnl);
    println!("  consistency score: {:.3}", a.consistency_score);

    println!("\nPer iteration (test window):");
    for it in &result.iterations {
        println!(
            "  #{}: train Sharpe {:.3} → test Sharpe {:.3} | test return {:.2}% | RSI {} MACD {}/{}",
            it.iteration,
            it.train_sharpe,
            it.test_sharpe,
            it.test_total_return,
            it.optimal_rsi_period,
            it.optimal_macd_fast,
            it.optimal_macd_slow
        );
    }
}

/// Rough calendar span for summary line (Binance interval strings).
fn approx_interval_days(candles: usize, interval: &str) -> f64 {
    let minutes_per_candle = match interval {
        "1m" => 1.0,
        "3m" => 3.0,
        "5m" => 5.0,
        "15m" => 15.0,
        "1h" | "60m" => 60.0,
        "4h" => 240.0,
        "1d" | "1D" => 1440.0,
        _ => 1.0,
    };
    (candles as f64 * minutes_per_candle) / 60.0 / 24.0
}
