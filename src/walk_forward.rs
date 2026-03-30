use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::spot_price::Candle;
use crate::signals::SignalConfig;
use crate::backtest::{BacktestConfig, BacktestResult, run_backtest};
use crate::volatility::VolatilityFilterConfig;

/// Walk-forward analysis configuration
#[derive(Debug, Clone)]
pub struct WalkForwardConfig {
    pub train_window: usize,      // Number of candles for training window
    pub test_window: usize,        // Number of candles for testing window
    pub step_size: usize,          // How many candles to advance each iteration
    pub min_edge: Decimal,
    pub min_confidence: Decimal,
    pub max_position_pct: Decimal,
    pub initial_balance: Decimal,
    pub holding_period: usize,
    pub volatility_filter: VolatilityFilterConfig,
    /// Grid aramasında RSI/MACD dışında sabit tutulan hacim ayarları (analyze `.env` ile hizalı).
    pub volume_min_ratio: Option<f64>,
    pub volume_avg_bars: usize,
}

impl Default for WalkForwardConfig {
    fn default() -> Self {
        Self {
            train_window: 1000,       // ~16 hours at 1m candles
            test_window: 500,         // ~8 hours at 1m candles
            step_size: 250,           // ~4 hours forward
            min_edge: dec!(0.06),
            min_confidence: dec!(0.70),
            max_position_pct: dec!(0.05),
            initial_balance: dec!(1000),
            holding_period: 5,
            volatility_filter: VolatilityFilterConfig::default(),
            volume_min_ratio: None,
            volume_avg_bars: 20,
        }
    }
}

/// Results from a single walk-forward iteration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkForwardIteration {
    pub iteration: usize,
    pub train_start: DateTime<Utc>,
    pub train_end: DateTime<Utc>,
    pub test_start: DateTime<Utc>,
    pub test_end: DateTime<Utc>,

    // Optimized parameters from training
    pub optimal_rsi_period: usize,
    pub optimal_macd_fast: usize,
    pub optimal_macd_slow: usize,

    // Training performance
    pub train_sharpe: f64,
    pub train_win_rate: f64,
    pub train_total_return: f64,

    // Out-of-sample testing performance
    pub test_sharpe: f64,
    pub test_win_rate: f64,
    pub test_total_return: f64,
    pub test_total_trades: usize,
    pub test_pnl: Decimal,
}

/// Complete walk-forward analysis results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkForwardResult {
    pub iterations: Vec<WalkForwardIteration>,
    pub aggregate_metrics: WalkForwardMetrics,
}

/// Aggregate metrics across all walk-forward iterations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkForwardMetrics {
    pub total_iterations: usize,
    pub avg_test_sharpe: f64,
    pub avg_test_win_rate: f64,
    pub avg_test_return: f64,
    pub total_test_trades: usize,
    pub cumulative_pnl: Decimal,
    pub consistency_score: f64,  // How consistent are test results across iterations
}

/// Run walk-forward validation
///
/// Process:
/// 1. Divide data into rolling train/test windows
/// 2. For each iteration:
///    a. Optimize parameters on training window
///    b. Validate on out-of-sample test window
///    c. Record both in-sample and out-of-sample performance
/// 3. Aggregate results to assess strategy robustness
pub fn run_walk_forward(
    candles: &[Candle],
    asset: &str,
    config: &WalkForwardConfig,
) -> Result<WalkForwardResult> {
    let total_required = config.train_window + config.test_window;

    if candles.len() < total_required {
        anyhow::bail!(
            "Not enough data: need {} candles, have {}",
            total_required,
            candles.len()
        );
    }

    let mut iterations = Vec::new();
    let mut iteration_num = 0;
    let mut position = 0;

    // Walk forward through the data
    while position + total_required <= candles.len() {
        let train_end_idx = position + config.train_window;
        let test_end_idx = train_end_idx + config.test_window;

        let train_data = &candles[position..train_end_idx];
        let test_data = &candles[train_end_idx..test_end_idx];

        // Optimize parameters on training data
        let (optimal_params, train_result) = optimize_parameters(
            train_data,
            asset,
            config,
        )?;

        // Validate on test data with optimized parameters
        let test_result = run_validation(
            test_data,
            asset,
            config,
            &optimal_params,
        )?;

        iterations.push(WalkForwardIteration {
            iteration: iteration_num,
            train_start: train_data.first().unwrap().timestamp,
            train_end: train_data.last().unwrap().timestamp,
            test_start: test_data.first().unwrap().timestamp,
            test_end: test_data.last().unwrap().timestamp,
            optimal_rsi_period: optimal_params.rsi_period,
            optimal_macd_fast: optimal_params.macd_fast,
            optimal_macd_slow: optimal_params.macd_slow,
            train_sharpe: train_result.metrics.sharpe_ratio,
            train_win_rate: train_result.metrics.win_rate,
            train_total_return: train_result.metrics.total_return_pct,
            test_sharpe: test_result.metrics.sharpe_ratio,
            test_win_rate: test_result.metrics.win_rate,
            test_total_return: test_result.metrics.total_return_pct,
            test_total_trades: test_result.metrics.total_trades,
            test_pnl: test_result.metrics.total_pnl,
        });

        iteration_num += 1;
        position += config.step_size;
    }

    if iterations.is_empty() {
        anyhow::bail!("No complete iterations generated");
    }

    // Calculate aggregate metrics
    let aggregate = calculate_aggregate_metrics(&iterations);

    Ok(WalkForwardResult {
        iterations,
        aggregate_metrics: aggregate,
    })
}

/// Optimize parameters on training data using grid search
fn optimize_parameters(
    train_data: &[Candle],
    asset: &str,
    config: &WalkForwardConfig,
) -> Result<(SignalConfig, BacktestResult)> {
    // Grid search over parameter space
    let rsi_periods = vec![10, 14, 20];
    let macd_fast_periods = vec![8, 12, 16];
    let macd_slow_periods = vec![21, 26, 30];

    let mut best_sharpe = f64::NEG_INFINITY;
    let mut best_params = SignalConfig::default();
    let mut best_result = None;

    for &rsi in &rsi_periods {
        for &fast in &macd_fast_periods {
            for &slow in &macd_slow_periods {
                if fast >= slow {
                    continue; // Invalid: fast must be < slow
                }

                let signal_config = SignalConfig {
                    rsi_period: rsi,
                    macd_fast: fast,
                    macd_slow: slow,
                    macd_signal: 9,
                    volume_min_ratio: config.volume_min_ratio,
                    volume_avg_bars: config.volume_avg_bars.max(5),
                };

                let backtest_config = BacktestConfig {
                    initial_balance: config.initial_balance,
                    min_edge: config.min_edge,
                    min_confidence: config.min_confidence,
                    max_position_pct: config.max_position_pct,
                    signal_config: signal_config.clone(),
                    volatility_filter: config.volatility_filter.clone(),
                };

                // Run backtest with these parameters
                let result = match run_backtest(
                    train_data,
                    asset,
                    &backtest_config,
                    100,  // lookback
                    config.holding_period,
                ) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                // Prefer higher Sharpe; if all runs have zero trades, keep the first Ok result.
                let sharpe = result.metrics.sharpe_ratio;
                if best_result.is_none() || sharpe > best_sharpe {
                    best_sharpe = sharpe;
                    best_params = signal_config;
                    best_result = Some(result);
                }
            }
        }
    }

    best_result
        .map(|r| (best_params, r))
        .ok_or_else(|| anyhow::anyhow!("Failed to optimize parameters"))
}

/// Run validation on test data with given parameters
fn run_validation(
    test_data: &[Candle],
    asset: &str,
    config: &WalkForwardConfig,
    params: &SignalConfig,
) -> Result<BacktestResult> {
    let backtest_config = BacktestConfig {
        initial_balance: config.initial_balance,
        min_edge: config.min_edge,
        min_confidence: config.min_confidence,
        max_position_pct: config.max_position_pct,
        signal_config: params.clone(),
        volatility_filter: config.volatility_filter.clone(),
    };

    run_backtest(
        test_data,
        asset,
        &backtest_config,
        100,
        config.holding_period,
    )
}

/// Calculate aggregate metrics across all iterations.
/// Sharpe / win rate / return ortalamaları yalnızca testte en az bir işlem olan pencerelerden hesaplanır
/// (işlemsiz pencereler ortalamayı kirletmez).
fn calculate_aggregate_metrics(iterations: &[WalkForwardIteration]) -> WalkForwardMetrics {
    let total_iterations = iterations.len();

    let active: Vec<&WalkForwardIteration> = iterations
        .iter()
        .filter(|i| i.test_total_trades > 0)
        .collect();

    let n_active = active.len();

    let avg_test_sharpe = if n_active == 0 {
        0.0
    } else {
        let finite: Vec<f64> = active
            .iter()
            .map(|i| i.test_sharpe)
            .filter(|s| s.is_finite())
            .collect();
        if finite.is_empty() {
            0.0
        } else {
            finite.iter().sum::<f64>() / finite.len() as f64
        }
    };

    let avg_test_win_rate = if n_active == 0 {
        0.0
    } else {
        active.iter().map(|i| i.test_win_rate).sum::<f64>() / n_active as f64
    };

    let avg_test_return = if n_active == 0 {
        0.0
    } else {
        active.iter().map(|i| i.test_total_return).sum::<f64>() / n_active as f64
    };

    let total_test_trades = iterations.iter().map(|i| i.test_total_trades).sum();

    let cumulative_pnl = iterations.iter().map(|i| i.test_pnl).sum();

    let returns: Vec<f64> = active.iter().map(|i| i.test_total_return).collect();

    let consistency_score = if returns.len() < 2 {
        0.0
    } else {
        let mean = avg_test_return;
        let variance = returns
            .iter()
            .map(|r| (r - mean).powi(2))
            .sum::<f64>()
            / returns.len() as f64;
        let std_dev = variance.sqrt();
        if std_dev > 0.0 && mean.abs() > 0.0 {
            1.0 / (std_dev / mean.abs()).max(0.1)
        } else {
            0.0
        }
    };

    WalkForwardMetrics {
        total_iterations,
        avg_test_sharpe,
        avg_test_win_rate,
        avg_test_return,
        total_test_trades,
        cumulative_pnl,
        consistency_score,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn mock_candles_sine_wave(count: usize) -> Vec<Candle> {
        use std::f64::consts::PI;
        (0..count)
            .map(|i| {
                let angle = (i as f64) * 2.0 * PI / 50.0; // Period of 50 candles
                let price = 100.0 + 10.0 * angle.sin();
                Candle {
                    timestamp: Utc::now() + chrono::Duration::minutes(i as i64),
                    open: Decimal::try_from(price).unwrap(),
                    high: Decimal::try_from(price + 1.0).unwrap(),
                    low: Decimal::try_from(price - 1.0).unwrap(),
                    close: Decimal::try_from(price).unwrap(),
                    volume: Decimal::from(1000),
                }
            })
            .collect()
    }

    #[test]
    fn test_walk_forward_basic() {
        let candles = mock_candles_sine_wave(2000);
        let config = WalkForwardConfig {
            train_window: 500,
            test_window: 250,
            step_size: 100,
            ..Default::default()
        };

        let result = run_walk_forward(&candles, "BTC", &config);
        assert!(result.is_ok());

        let result = result.unwrap();
        assert!(result.iterations.len() > 0);
        assert_eq!(result.aggregate_metrics.total_iterations, result.iterations.len());
    }

    #[test]
    fn test_parameter_optimization() {
        let candles = mock_candles_sine_wave(1200);
        let config = WalkForwardConfig::default();

        let (params, result) = optimize_parameters(&candles, "BTC", &config).unwrap();

        // Should have valid parameters
        assert!(params.rsi_period > 0);
        assert!(params.macd_fast < params.macd_slow);
        assert!(result.metrics.total_trades > 0);
    }

    #[test]
    fn test_insufficient_data() {
        let candles = mock_candles_sine_wave(500);
        let config = WalkForwardConfig::default();

        let result = run_walk_forward(&candles, "BTC", &config);
        assert!(result.is_err());
    }

    #[test]
    fn test_aggregate_metrics_consistency() {
        let candles = mock_candles_sine_wave(3000);
        let config = WalkForwardConfig {
            step_size: 200,
            ..Default::default()
        };

        let result = run_walk_forward(&candles, "BTC", &config).unwrap();
        let metrics = result.aggregate_metrics;

        assert!(metrics.avg_test_win_rate >= 0.0 && metrics.avg_test_win_rate <= 1.0);
        assert!(metrics.consistency_score >= 0.0);
        assert_eq!(metrics.total_iterations, result.iterations.len());
    }
}
