use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::spot_price::Candle;
use crate::signals::{SignalConfig, SignalError, generate_signal};
use crate::volatility::{VolatilityFilterConfig, passes_volatility_filter};
use crate::edge;

/// İşlem yokken Sharpe tanımsız; 0 kullanılır (WF ortalamalarını bozmaz). Grid’de sıralama `best_result.is_none()` ile yapılır.
const SHARPE_ABS_CAP: f64 = 100.0;

/// Trade execution from backtest
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestTrade {
    pub timestamp: DateTime<Utc>,
    pub asset: String,
    pub signal: String,          // "UP" or "DOWN"
    pub entry_price: Decimal,
    pub exit_price: Decimal,
    pub size_usdc: Decimal,
    pub pnl: Decimal,
    pub edge: Decimal,
    pub confidence: Decimal,
    pub won: bool,
}

/// Backtest performance metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestMetrics {
    pub total_trades: usize,
    pub winning_trades: usize,
    pub losing_trades: usize,
    pub win_rate: f64,

    pub total_pnl: Decimal,
    pub total_return_pct: f64,
    pub average_pnl: Decimal,

    pub sharpe_ratio: f64,
    pub max_drawdown: f64,
    pub max_drawdown_pct: f64,

    pub avg_win: Decimal,
    pub avg_loss: Decimal,
    pub profit_factor: f64,

    pub start_date: DateTime<Utc>,
    pub end_date: DateTime<Utc>,
    pub duration_days: f64,
}

/// Configuration for backtesting
#[derive(Debug, Clone)]
pub struct BacktestConfig {
    pub initial_balance: Decimal,
    pub min_edge: Decimal,
    pub min_confidence: Decimal,
    pub max_position_pct: Decimal,
    pub signal_config: SignalConfig,
    pub volatility_filter: VolatilityFilterConfig,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            initial_balance: dec!(1000),
            min_edge: dec!(0.06),
            min_confidence: dec!(0.70),
            max_position_pct: dec!(0.05),
            signal_config: SignalConfig::default(),
            volatility_filter: VolatilityFilterConfig::default(),
        }
    }
}

/// Backtest result containing trades and metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestResult {
    pub trades: Vec<BacktestTrade>,
    pub metrics: BacktestMetrics,
    pub equity_curve: Vec<(DateTime<Utc>, Decimal)>,
    /// `generate_signal` başarılı olduğu bar sayısı (vol filtresinden önce).
    #[serde(default)]
    pub bars_with_signal: usize,
    /// Sinyal varken volatilite filtresinin işlemi kestiği bar sayısı.
    #[serde(default)]
    pub volatility_filter_skips: usize,
    /// Düşük spot hacim (`VOLUME_MIN_RATIO`) nedeniyle elenen bar sayısı.
    #[serde(default)]
    pub volume_low_skips: usize,
    /// RSI+küme / MACD net yön üretemediği bar sayısı.
    #[serde(default)]
    pub no_clear_signal_skips: usize,
}

/// Run a backtest on historical candle data
///
/// For each candle window:
/// 1. Generate technical signal
/// 2. If signal meets thresholds, calculate edge and position size
/// 3. Simulate trade outcome based on next candle movement
/// 4. Track P&L and performance metrics
pub fn run_backtest(
    candles: &[Candle],
    asset: &str,
    config: &BacktestConfig,
    lookback: usize,
    holding_period: usize,  // How many candles to hold position
) -> Result<BacktestResult> {
    let need = lookback + holding_period;
    if candles.len() < need {
        anyhow::bail!(
            "Not enough candles for backtest: need at least {} (lookback {} + holding {}), got {}",
            need,
            lookback,
            holding_period,
            candles.len()
        );
    }

    let mut trades = Vec::new();
    let mut balance = config.initial_balance;
    let mut equity_curve = vec![(candles[0].timestamp, balance)];
    let mut peak_balance = balance;
    let mut max_drawdown = dec!(0);
    let mut bars_with_signal = 0usize;
    let mut volatility_filter_skips = 0usize;
    let mut volume_low_skips = 0usize;
    let mut no_clear_signal_skips = 0usize;

    // Walk through candles with sliding window
    for i in lookback..(candles.len() - holding_period) {
        let window = &candles[(i - lookback)..i];

        // Generate signal
        let signal = match generate_signal(window, &config.signal_config) {
            Ok(s) => s,
            Err(SignalError::VolumeTooLow) => {
                volume_low_skips += 1;
                continue;
            }
            Err(SignalError::NoClearSignal) => {
                no_clear_signal_skips += 1;
                continue;
            }
        };
        bars_with_signal += 1;

        if !passes_volatility_filter(window, &config.volatility_filter) {
            volatility_filter_skips += 1;
            continue;
        }

        // Check confidence threshold
        if signal.confidence < config.min_confidence {
            continue;
        }

        // Simulate market price (use current close as proxy)
        let market_price = dec!(0.5); // Binary market default

        // Calculate edge
        let edge_result = match edge::calculate(
            signal.probability,
            market_price,
            config.min_edge,
        ) {
            Some(e) => e,
            None => continue,
        };

        // Position sizing
        let size_usdc = edge::kelly_size(
            edge_result.edge,
            signal.confidence,
            balance,
            config.max_position_pct,
        );

        if size_usdc <= Decimal::ZERO {
            continue;
        }

        // Entry
        let entry_price = window.last().unwrap().close;
        let entry_time = window.last().unwrap().timestamp;

        // Exit (after holding_period candles)
        let exit_candle = &candles[i + holding_period];
        let exit_price = exit_candle.close;

        // Calculate P&L based on signal direction
        let price_change = exit_price - entry_price;
        let _price_change_pct = if entry_price > Decimal::ZERO {
            price_change / entry_price
        } else {
            dec!(0)
        };

        let won = match signal.direction {
            crate::signals::SignalDirection::Up => price_change > Decimal::ZERO,
            crate::signals::SignalDirection::Down => price_change < Decimal::ZERO,
        };

        // P&L calculation (simplified binary market simulation)
        let pnl = if won {
            // Win: gain is (1 - entry_odds) * bet
            size_usdc * dec!(0.8) // Simplified ~80% return on win
        } else {
            // Loss: lose entire bet
            -size_usdc
        };

        balance += pnl;

        // Track drawdown
        if balance > peak_balance {
            peak_balance = balance;
        }
        let drawdown = peak_balance - balance;
        if drawdown > max_drawdown {
            max_drawdown = drawdown;
        }

        // Record trade
        trades.push(BacktestTrade {
            timestamp: entry_time,
            asset: asset.to_string(),
            signal: format!("{:?}", signal.direction),
            entry_price,
            exit_price,
            size_usdc,
            pnl,
            edge: edge_result.edge,
            confidence: signal.confidence,
            won,
        });

        equity_curve.push((exit_candle.timestamp, balance));
    }

    // Calculate metrics
    let metrics = calculate_metrics(&trades, config.initial_balance, &equity_curve)?;

    Ok(BacktestResult {
        trades,
        metrics,
        equity_curve,
        bars_with_signal,
        volatility_filter_skips,
        volume_low_skips,
        no_clear_signal_skips,
    })
}

/// Calculate comprehensive backtest metrics
fn calculate_metrics(
    trades: &[BacktestTrade],
    initial_balance: Decimal,
    equity_curve: &[(DateTime<Utc>, Decimal)],
) -> Result<BacktestMetrics> {
    if trades.is_empty() {
        let start_date = equity_curve
            .first()
            .map(|(t, _)| *t)
            .unwrap_or_else(Utc::now);
        let end_date = equity_curve
            .last()
            .map(|(t, _)| *t)
            .unwrap_or(start_date);
        let duration_days = ((end_date - start_date).num_seconds() as f64 / 86400.0).max(0.0);

        return Ok(BacktestMetrics {
            total_trades: 0,
            winning_trades: 0,
            losing_trades: 0,
            win_rate: 0.0,
            total_pnl: dec!(0),
            total_return_pct: 0.0,
            average_pnl: dec!(0),
            sharpe_ratio: 0.0,
            max_drawdown: 0.0,
            max_drawdown_pct: 0.0,
            avg_win: dec!(0),
            avg_loss: dec!(0),
            profit_factor: 0.0,
            start_date,
            end_date,
            duration_days,
        });
    }

    let total_trades = trades.len();
    let winning_trades = trades.iter().filter(|t| t.won).count();
    let losing_trades = total_trades - winning_trades;
    let win_rate = winning_trades as f64 / total_trades as f64;

    let total_pnl: Decimal = trades.iter().map(|t| t.pnl).sum();
    let final_balance = initial_balance + total_pnl;
    let total_return_pct = ((final_balance - initial_balance) / initial_balance)
        .to_f64()
        .unwrap_or(0.0) * 100.0;

    let average_pnl = total_pnl / Decimal::from(total_trades);

    // Win/loss averages
    let wins: Vec<_> = trades.iter().filter(|t| t.won).collect();
    let losses: Vec<_> = trades.iter().filter(|t| !t.won).collect();

    let avg_win = if !wins.is_empty() {
        wins.iter().map(|t| t.pnl).sum::<Decimal>() / Decimal::from(wins.len())
    } else {
        dec!(0)
    };

    let avg_loss = if !losses.is_empty() {
        losses.iter().map(|t| t.pnl.abs()).sum::<Decimal>() / Decimal::from(losses.len())
    } else {
        dec!(0)
    };

    let profit_factor = if avg_loss > Decimal::ZERO {
        (avg_win / avg_loss).to_f64().unwrap_or(0.0)
    } else {
        0.0
    };

    // Sharpe ratio (simplified)
    let returns: Vec<f64> = trades.iter()
        .map(|t| (t.pnl / t.size_usdc).to_f64().unwrap_or(0.0))
        .collect();

    let mean_return = returns.iter().sum::<f64>() / returns.len() as f64;
    let variance = returns.iter()
        .map(|r| (r - mean_return).powi(2))
        .sum::<f64>() / returns.len() as f64;
    let std_dev = variance.sqrt();

    // Çok küçük std_dev ile Sharpe patlamasını engelle (walk-forward ortalaması için).
    let sharpe_ratio = if std_dev > 1e-12 && returns.len() >= 2 {
        let raw = mean_return / std_dev * (252.0_f64).sqrt();
        raw.clamp(-SHARPE_ABS_CAP, SHARPE_ABS_CAP)
    } else {
        0.0
    };

    // Max drawdown
    let mut peak = initial_balance;
    let mut max_dd = dec!(0);
    let mut max_dd_pct = 0.0;

    for (_, balance) in equity_curve {
        if *balance > peak {
            peak = *balance;
        }
        let dd = peak - *balance;
        if dd > max_dd {
            max_dd = dd;
            max_dd_pct = (dd / peak).to_f64().unwrap_or(0.0) * 100.0;
        }
    }

    let start_date = trades.first().unwrap().timestamp;
    let end_date = trades.last().unwrap().timestamp;
    let duration_days = (end_date - start_date).num_seconds() as f64 / 86400.0;

    Ok(BacktestMetrics {
        total_trades,
        winning_trades,
        losing_trades,
        win_rate,
        total_pnl,
        total_return_pct,
        average_pnl,
        sharpe_ratio,
        max_drawdown: max_dd.to_f64().unwrap_or(0.0),
        max_drawdown_pct: max_dd_pct,
        avg_win,
        avg_loss,
        profit_factor,
        start_date,
        end_date,
        duration_days,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn mock_candles_trend(count: usize, trend_up: bool) -> Vec<Candle> {
        (0..count)
            .map(|i| {
                let base = if trend_up { 100 + i } else { 100 + count - i };
                Candle {
                    timestamp: Utc::now() + chrono::Duration::minutes(i as i64),
                    open: Decimal::from(base),
                    high: Decimal::from(base + 2),
                    low: Decimal::from(base - 2),
                    close: Decimal::from(base + 1),
                    volume: Decimal::from(1000),
                }
            })
            .collect()
    }

    #[test]
    fn test_backtest_uptrend() {
        let candles = mock_candles_trend(200, true);
        let config = BacktestConfig::default();

        let result = run_backtest(&candles, "BTC", &config, 100, 5);
        assert!(result.is_ok());

        let result = result.unwrap();
        assert!(result.trades.len() > 0, "Should generate trades in uptrend");
        assert!(result.metrics.total_trades > 0);
    }

    #[test]
    fn test_backtest_metrics_calculation() {
        let candles = mock_candles_trend(200, true);
        let config = BacktestConfig::default();

        let result = run_backtest(&candles, "BTC", &config, 100, 5).unwrap();
        let metrics = &result.metrics;

        assert!(metrics.win_rate >= 0.0 && metrics.win_rate <= 1.0);
        assert_eq!(metrics.winning_trades + metrics.losing_trades, metrics.total_trades);
        assert!(metrics.duration_days > 0.0);
    }

    #[test]
    fn test_insufficient_candles() {
        let candles = mock_candles_trend(50, true);
        let config = BacktestConfig::default();

        let result = run_backtest(&candles, "BTC", &config, 100, 5);
        assert!(result.is_err());
    }
}
