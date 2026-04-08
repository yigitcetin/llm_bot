use anyhow::Result;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::debug;

use crate::constants::MIN_CANDLES_FOR_SIGNAL;
use crate::spot_price::Candle;

/// Teknik sinyal üretilemediğinde (düşük hacim, berabere oy vb.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalError {
    /// `VOLUME_MIN_RATIO` altında spot hacim — işlem yok
    VolumeTooLow,
    /// RSI+momentum kümesi ile MACD net yön üretemedi
    NoClearSignal,
}

impl std::fmt::Display for SignalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VolumeTooLow => write!(f, "volume below minimum ratio"),
            Self::NoClearSignal => write!(f, "no clear technical signal"),
        }
    }
}

impl std::error::Error for SignalError {}

pub type SignalResult = Result<TechnicalSignal, SignalError>;

/// Technical signal generated from spot price analysis
#[derive(Debug, Clone)]
pub struct TechnicalSignal {
    pub direction: SignalDirection,
    /// Implied probability that **YES** wins (direction-aware vs momentum; typically ~0.15–0.85).
    pub probability: Decimal,
    pub confidence: Decimal, // 0.5-1.0
    pub reasoning: String,
    /// Wilder RSI (0–100) at signal time.
    pub rsi: f64,
    /// MACD histogram (MACD line − signal line) at signal time.
    pub macd_histogram: f64,
    /// Last-bar volume / rolling average (see `compute_volume_ratio`).
    pub volume_ratio: f64,
    /// RSI+momentum cluster vote: `UP`, `DOWN`, or `TIE` when votes tie / no majority.
    pub cluster_direction: String,
}

/// Signal direction from technical analysis
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalDirection {
    Up,   // Spot price expected to go UP
    Down, // Spot price expected to go DOWN
}

/// Son mumun hacmi / son `period` mumun ortalama hacmi (spot kalite ölçümü).
pub fn compute_volume_ratio(candles: &[Candle], avg_period: usize) -> f64 {
    if candles.is_empty() || avg_period == 0 {
        return 1.0;
    }
    let current_volume = candles.last().unwrap().volume.to_f64().unwrap_or(0.0);
    let avg_volume = calculate_avg_volume(candles, avg_period);
    if avg_volume > 0.0 {
        current_volume / avg_volume
    } else {
        1.0
    }
}

/// Wilder (RMA) RSI — matches common exchange / TradingView defaults better than SMA-RSI.
/// Returns value between 0-100.
fn calculate_rsi(candles: &[Candle], period: usize) -> Result<f64> {
    if candles.len() < period + 1 {
        anyhow::bail!("Not enough candles for RSI calculation");
    }

    let mut gains = vec![];
    let mut losses = vec![];

    for i in 1..candles.len() {
        let change = candles[i].close - candles[i - 1].close;
        if change > Decimal::ZERO {
            gains.push(change.to_f64().unwrap_or(0.0));
            losses.push(0.0);
        } else {
            gains.push(0.0);
            losses.push(change.abs().to_f64().unwrap_or(0.0));
        }
    }

    if gains.len() < period {
        anyhow::bail!("Not enough price changes for RSI");
    }

    let mut avg_gain: f64 = gains[..period].iter().sum::<f64>() / period as f64;
    let mut avg_loss: f64 = losses[..period].iter().sum::<f64>() / period as f64;

    let p1 = (period - 1) as f64;
    for i in period..gains.len() {
        avg_gain = (avg_gain * p1 + gains[i]) / period as f64;
        avg_loss = (avg_loss * p1 + losses[i]) / period as f64;
    }

    if avg_loss == 0.0 {
        return Ok(100.0);
    }

    let rs = avg_gain / avg_loss;
    let rsi = 100.0 - (100.0 / (1.0 + rs));

    Ok(rsi)
}

/// Full-bar EMA series (same length as `values`).
fn ema_series(values: &[f64], period: usize) -> Vec<f64> {
    if values.is_empty() || period == 0 {
        return Vec::new();
    }
    let k = 2.0 / (period as f64 + 1.0);
    let mut out = Vec::with_capacity(values.len());
    let mut ema = values[0];
    out.push(ema);
    for &v in values.iter().skip(1) {
        ema = (v - ema) * k + ema;
        out.push(ema);
    }
    out
}

/// MACD line, signal line (EMA of MACD), histogram, and bullish crossover (histogram crosses above 0).
fn calculate_macd(
    candles: &[Candle],
    fast: usize,
    slow: usize,
    signal_period: usize,
) -> Result<(f64, f64, f64, bool)> {
    if signal_period == 0 || candles.len() < slow + signal_period {
        anyhow::bail!("Not enough candles for MACD calculation");
    }

    let closes: Vec<f64> = candles
        .iter()
        .map(|c| c.close.to_f64().unwrap_or(0.0))
        .collect();

    let ema_fast = ema_series(&closes, fast);
    let ema_slow = ema_series(&closes, slow);

    let macd_series: Vec<f64> = ema_fast
        .iter()
        .zip(ema_slow.iter())
        .map(|(a, b)| a - b)
        .collect();

    let signal_series = ema_series(&macd_series, signal_period);

    let n = macd_series.len();
    let macd_line = *macd_series.last().unwrap_or(&0.0);
    let signal_line = *signal_series.last().unwrap_or(&0.0);
    let histogram = macd_line - signal_line;

    let mut bullish_cross = false;
    if n >= 2 {
        let prev_macd = macd_series[n - 2];
        let prev_sig = signal_series[n - 2];
        let prev_hist = prev_macd - prev_sig;
        let curr_hist = histogram;
        bullish_cross = prev_hist <= 0.0 && curr_hist > 0.0;
    }

    Ok((macd_line, signal_line, histogram, bullish_cross))
}

/// Calculate EMA (Exponential Moving Average) — last value only (used by tests / helpers).
fn calculate_ema(values: &[f64], period: usize) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let s = ema_series(values, period);
    *s.last().unwrap_or(&0.0)
}

/// Calculate momentum (rate of change)
fn calculate_momentum(candles: &[Candle], lookback: usize) -> Result<f64> {
    if candles.len() <= lookback {
        anyhow::bail!("Not enough candles for momentum calculation");
    }

    let current = candles.last().unwrap().close.to_f64().unwrap_or(0.0);
    let past = candles[candles.len() - lookback - 1]
        .close
        .to_f64()
        .unwrap_or(0.0);

    if past == 0.0 {
        return Ok(0.0);
    }

    Ok((current - past) / past)
}

/// Calculate average volume
fn calculate_avg_volume(candles: &[Candle], period: usize) -> f64 {
    if candles.len() < period {
        return 0.0;
    }

    let sum: f64 = candles
        .iter()
        .rev()
        .take(period)
        .map(|c| c.volume.to_f64().unwrap_or(0.0))
        .sum();

    sum / period as f64
}

/// Tek oy: RSI + 5m momentum + 15m momentum (aynı fiyat türevinin tekrarlı oylaması değil).
/// Thresholds come from [`SignalConfig`] (plan P2: calibrated for short horizons).
fn cluster_vote_rsi_momentum(
    rsi: f64,
    momentum_5m: f64,
    momentum_15m: f64,
    cfg: &SignalConfig,
) -> Option<SignalDirection> {
    let mut up = 0u32;
    let mut down = 0u32;

    if rsi < cfg.cluster_rsi_oversold {
        up += 1;
    } else if rsi > cfg.cluster_rsi_overbought {
        down += 1;
    }

    let m5 = cfg.cluster_mom5_abs;
    if momentum_5m > m5 {
        up += 1;
    } else if momentum_5m < -m5 {
        down += 1;
    }

    let m15 = cfg.cluster_mom15_abs;
    if momentum_15m > m15 {
        up += 1;
    } else if momentum_15m < -m15 {
        down += 1;
    }

    if up > down {
        Some(SignalDirection::Up)
    } else if down > up {
        Some(SignalDirection::Down)
    } else {
        None
    }
}

/// Generate trading signal from technical indicators.
///
/// İki ana bileşen: (1) RSI+momentum kümesi — tek oy, (2) MACD çizgisi — trend oyu.
/// Çelişki durumunda MACD tie-breaker.
pub fn generate_signal(candles: &[Candle], config: &SignalConfig) -> SignalResult {
    if candles.len() < MIN_CANDLES_FOR_SIGNAL {
        return Err(SignalError::NoClearSignal);
    }

    let avg_bars = config.volume_avg_bars.max(5);
    // P3: optional use of **closed** candles only for volume ratio (excludes the open bar).
    let candles_for_volume: &[Candle] = if config.volume_use_closed_candle_only && candles.len() > 1
    {
        &candles[..candles.len() - 1]
    } else {
        candles
    };
    if candles_for_volume.len() < avg_bars + 1 {
        return Err(SignalError::NoClearSignal);
    }
    let volume_ratio = compute_volume_ratio(candles_for_volume, avg_bars);

    if let Some(min_r) = config.volume_min_ratio {
        if volume_ratio < min_r {
            return Err(SignalError::VolumeTooLow);
        }
    }

    let rsi = calculate_rsi(candles, config.rsi_period).map_err(|_| SignalError::NoClearSignal)?;
    let (macd_line, signal_line, histogram, bullish_cross) = calculate_macd(
        candles,
        config.macd_fast,
        config.macd_slow,
        config.macd_signal,
    )
    .map_err(|_| SignalError::NoClearSignal)?;

    let momentum_5m = calculate_momentum(candles, 5).map_err(|_| SignalError::NoClearSignal)?;
    let momentum_15m = calculate_momentum(candles, 15).map_err(|_| SignalError::NoClearSignal)?;

    // MACD direction: histogram (MACD − signal) is the momentum; tie-break with MACD line vs zero.
    let macd_dir = if histogram > 0.0 {
        SignalDirection::Up
    } else if histogram < 0.0 {
        SignalDirection::Down
    } else if macd_line > 0.0 {
        SignalDirection::Up
    } else if macd_line < 0.0 {
        SignalDirection::Down
    } else {
        return Err(SignalError::NoClearSignal);
    };

    let cluster_dir = cluster_vote_rsi_momentum(rsi, momentum_5m, momentum_15m, config);

    let direction = match cluster_dir {
        None => macd_dir,
        Some(c) if c == macd_dir => macd_dir,
        Some(_) => macd_dir, // çelişki: MACD tie-breaker
    };

    let base_confidence = match cluster_dir {
        None => 0.75,
        Some(c) if c == macd_dir => 0.88,
        Some(_) => 0.68,
    };

    let volume_boost = if volume_ratio > 2.0 { 0.1 } else { 0.0 };

    let mut reasons = vec![];
    match cluster_dir {
        None => reasons.push("cluster tie (MACD only)".to_string()),
        Some(c) if c == macd_dir => reasons.push("cluster+MACD agree".to_string()),
        Some(_) => reasons.push("cluster vs MACD conflict (MACD wins)".to_string()),
    }
    reasons.push(format!("MACD_line:{:.6}", macd_line));
    reasons.push(format!("MACD_sig:{:.6}", signal_line));
    reasons.push(format!("hist:{:.6}", histogram));
    if bullish_cross {
        reasons.push("MACD_hist_cross_up".to_string());
    }

    debug!(
        rsi = rsi,
        macd_line = macd_line,
        signal_line = signal_line,
        histogram = histogram,
        cluster = ?cluster_dir,
        volume_ratio = volume_ratio,
        "calculated technical indicators"
    );

    let scaled = momentum_5m.abs().min(0.03) / 0.03 * 0.3;
    // YES-implied probability: UP → higher YES odds; DOWN → lower YES odds (edge vs `yes_price` stays coherent).
    let base_probability = match direction {
        SignalDirection::Up => 0.5 + scaled,
        SignalDirection::Down => 0.5 - scaled,
    };
    let probability = base_probability.min(0.85).max(0.15);

    let taker_boost = candles
        .last()
        .and_then(|c| c.taker_buy_ratio)
        .and_then(|r| match direction {
            SignalDirection::Up if r > 0.55 => Some(0.05_f64),
            SignalDirection::Down if r < 0.45 => Some(0.05_f64),
            _ => None,
        })
        .unwrap_or(0.0);

    let confidence = f64::min(base_confidence + volume_boost + taker_boost, 0.95);

    let probability_dec = Decimal::try_from(probability).unwrap_or(dec!(0.5));
    let confidence_dec = Decimal::try_from(confidence).unwrap_or(dec!(0.5));

    let taker_note = candles
        .last()
        .and_then(|c| c.taker_buy_ratio)
        .map(|r| format!(" taker_buy_ratio:{:.2}", r))
        .unwrap_or_default();

    let reasoning = format!(
        "{} | RSI:{:.1} Mom5m:{:.2}% Mom15m:{:.2}% Vol:{:.1}x{}",
        reasons.join(" | "),
        rsi,
        momentum_5m * 100.0,
        momentum_15m * 100.0,
        volume_ratio,
        taker_note
    );

    let cluster_direction = match cluster_dir {
        Some(SignalDirection::Up) => "UP".to_string(),
        Some(SignalDirection::Down) => "DOWN".to_string(),
        None => "TIE".to_string(),
    };

    Ok(TechnicalSignal {
        direction,
        probability: probability_dec,
        confidence: confidence_dec,
        reasoning,
        rsi,
        macd_histogram: histogram,
        volume_ratio,
        cluster_direction,
    })
}

/// Configuration for signal generation
#[derive(Debug, Clone)]
pub struct SignalConfig {
    pub rsi_period: usize,
    pub macd_fast: usize,
    pub macd_slow: usize,
    pub macd_signal: usize,
    /// Son mum hacmi / ortalama; altında sinyal üretilmez (`None` = veto yok).
    pub volume_min_ratio: Option<f64>,
    /// `volume_ratio` ortalaması için mum sayısı (varsayılan 20).
    pub volume_avg_bars: usize,
    /// Use `candles[..-1]` for volume ratio so the in-progress bar does not veto signals (plan P3).
    pub volume_use_closed_candle_only: bool,
    /// Cluster vote: RSI below this → UP vote (oversold).
    pub cluster_rsi_oversold: f64,
    /// Cluster vote: RSI above this → DOWN vote (overbought).
    pub cluster_rsi_overbought: f64,
    /// Absolute 5m momentum threshold (rate of change) for cluster vote.
    pub cluster_mom5_abs: f64,
    /// Absolute 15m momentum threshold for cluster vote.
    pub cluster_mom15_abs: f64,
}

impl Default for SignalConfig {
    fn default() -> Self {
        Self {
            rsi_period: 14,
            macd_fast: 12,
            macd_slow: 26,
            macd_signal: 9,
            volume_min_ratio: None,
            volume_avg_bars: 20,
            volume_use_closed_candle_only: true,
            cluster_rsi_oversold: 40.0,
            cluster_rsi_overbought: 60.0,
            cluster_mom5_abs: 0.003,
            cluster_mom15_abs: 0.005,
        }
    }
}

/// Higher-timeframe trend filter: last close vs EMA on `htf_candles` must agree with `signal_dir`.
/// Returns `true` if filter passes (or data too thin / flat — do not block).
pub fn higher_timeframe_aligns(
    signal_dir: SignalDirection,
    htf_candles: &[Candle],
    ema_period: usize,
) -> bool {
    if htf_candles.len() < ema_period.max(5) {
        return true;
    }
    let closes: Vec<f64> = htf_candles
        .iter()
        .map(|c| c.close.to_f64().unwrap_or(0.0))
        .collect();
    let ema_val = calculate_ema(&closes, ema_period);
    let last = *closes.last().unwrap_or(&0.0);
    if last <= 0.0 || ema_val <= 0.0 {
        return true;
    }
    let rel = ((last - ema_val) / ema_val).abs();
    if rel < 1e-4 {
        return true;
    }
    let htf_up = last > ema_val;
    match signal_dir {
        SignalDirection::Up => htf_up,
        SignalDirection::Down => !htf_up,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn mock_candles(count: usize) -> Vec<Candle> {
        (0..count)
            .map(|i| Candle {
                timestamp: Utc::now(),
                open: Decimal::from(100 + i),
                high: Decimal::from(102 + i),
                low: Decimal::from(98 + i),
                close: Decimal::from(101 + i),
                volume: Decimal::from(1000),
                taker_buy_ratio: None,
            })
            .collect()
    }

    #[test]
    fn test_rsi_calculation() {
        let candles = mock_candles(50);
        let rsi = calculate_rsi(&candles, 14);
        assert!(rsi.is_ok());
        let rsi_value = rsi.unwrap();
        assert!(rsi_value >= 0.0 && rsi_value <= 100.0);
    }

    #[test]
    fn test_momentum_calculation() {
        let candles = mock_candles(20);
        let momentum = calculate_momentum(&candles, 5);
        assert!(momentum.is_ok());
    }

    #[test]
    fn test_signal_generation() {
        let candles = mock_candles(100);
        let config = SignalConfig::default();
        let signal = generate_signal(&candles, &config);

        assert!(signal.is_ok());
    }

    #[test]
    fn test_insufficient_candles() {
        let candles = mock_candles(50);
        let config = SignalConfig::default();
        let signal = generate_signal(&candles, &config);

        assert!(signal.is_err());
    }

    #[test]
    fn test_volume_veto() {
        let candles = mock_candles(100);
        let mut cfg = SignalConfig::default();
        cfg.volume_min_ratio = Some(5.0);
        let err = generate_signal(&candles, &cfg).unwrap_err();
        assert_eq!(err, SignalError::VolumeTooLow);
    }

    #[test]
    fn test_rsi_bounds() {
        // Test RSI stays within 0-100 range
        for trend_multiplier in [1, 5, 10] {
            let candles: Vec<Candle> = (0..50)
                .map(|i| Candle {
                    timestamp: Utc::now(),
                    open: Decimal::from(100 + i * trend_multiplier),
                    high: Decimal::from(102 + i * trend_multiplier),
                    low: Decimal::from(98 + i * trend_multiplier),
                    close: Decimal::from(101 + i * trend_multiplier),
                    volume: Decimal::from(1000),
                    taker_buy_ratio: None,
                })
                .collect();

            let rsi = calculate_rsi(&candles, 14).unwrap();
            assert!(rsi >= 0.0 && rsi <= 100.0, "RSI out of bounds: {}", rsi);
        }
    }

    #[test]
    fn test_macd_calculation() {
        let candles = mock_candles(50);
        let result = calculate_macd(&candles, 12, 26, 9);
        assert!(result.is_ok());

        let (macd_line, signal_line, histogram, _) = result.unwrap();
        assert!((histogram - (macd_line - signal_line)).abs() < 1e-9);
    }

    #[test]
    fn test_ema_calculation() {
        let values = vec![10.0, 11.0, 12.0, 11.5, 12.5, 13.0];
        let ema = calculate_ema(&values, 3);
        assert!(ema > 0.0);
        assert!(ema <= 13.0); // Should not exceed max value
    }

    #[test]
    fn test_momentum_positive() {
        let candles: Vec<Candle> = (0..20)
            .map(|i| Candle {
                timestamp: Utc::now(),
                open: Decimal::from(100 + i * 2),
                high: Decimal::from(102 + i * 2),
                low: Decimal::from(98 + i * 2),
                close: Decimal::from(101 + i * 2),
                volume: Decimal::from(1000),
                taker_buy_ratio: None,
            })
            .collect();

        let momentum = calculate_momentum(&candles, 5).unwrap();
        assert!(momentum > 0.0, "Uptrend should have positive momentum");
    }

    #[test]
    fn test_momentum_negative() {
        let candles: Vec<Candle> = (0..20)
            .map(|i| Candle {
                timestamp: Utc::now(),
                open: Decimal::from(150 - i * 2),
                high: Decimal::from(152 - i * 2),
                low: Decimal::from(148 - i * 2),
                close: Decimal::from(149 - i * 2),
                volume: Decimal::from(1000),
                taker_buy_ratio: None,
            })
            .collect();

        let momentum = calculate_momentum(&candles, 5).unwrap();
        assert!(momentum < 0.0, "Downtrend should have negative momentum");
    }

    #[test]
    fn test_volume_ratio_calculation() {
        let candles = mock_candles(50);
        let avg_vol = calculate_avg_volume(&candles, 20);
        assert!(avg_vol > 0.0);
        assert_eq!(avg_vol, 1000.0); // Mock candles have 1000 volume
    }

    #[test]
    fn test_signal_confidence_bounds() {
        let candles = mock_candles(100);
        let config = SignalConfig::default();
        let signal = generate_signal(&candles, &config).unwrap();

        assert!(signal.confidence >= dec!(0.5) && signal.confidence <= dec!(1.0));
        assert!(signal.probability >= dec!(0.15) && signal.probability <= dec!(0.85));
    }
}
