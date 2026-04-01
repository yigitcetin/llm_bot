//! Basit getiri volatilitesi ile işlem kapısı — RSI/MACD’dan bağımsız ikinci filtre.
//!
//! **Ölçek:** `compute_return_std_pct`, ardışık kapanış getirilerinin örneklem std sapmasını alır ve
//! `std * 100` ile sayıya çevirir (birim: getiri cinsinden std’nin yüz katı; “% fiyat hareketi” değil).
//! Örnek: getiri std’si `0.001` ise değer `0.1` olur. Bu yüzden `VOL_MAX_STD_PCT=0.05` çok sıkıdır —
//! çoğu 5m/1m BTC penceresinde üst sınırı aşılır. 5m için anlamlı deneme aralığı genelde **0.15–0.45**
//! civarıdır (veriye göre `analyze` ile kesinti oranına bakın).

use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;

use crate::spot_price::Candle;

/// Son `sample_bars` getirinin örneklem std’si × 100 (bkz. modül üstü ölçek notu); `min`/`max` yoksa filtre kapalıdır.
#[derive(Debug, Clone)]
pub struct VolatilityFilterConfig {
    pub min_std_pct: Option<Decimal>,
    pub max_std_pct: Option<Decimal>,
    pub sample_bars: usize,
}

impl Default for VolatilityFilterConfig {
    fn default() -> Self {
        Self {
            min_std_pct: None,
            max_std_pct: None,
            sample_bars: 20,
        }
    }
}

impl VolatilityFilterConfig {
    pub fn validate(&self) -> Result<()> {
        if self.sample_bars < 5 || self.sample_bars > 500 {
            anyhow::bail!(
                "VOL_SAMPLE_BARS must be between 5 and 500, got {}",
                self.sample_bars
            );
        }
        if let (Some(lo), Some(hi)) = (self.min_std_pct, self.max_std_pct) {
            if lo >= hi {
                anyhow::bail!(
                    "VOL_MIN_STD_PCT ({}) must be < VOL_MAX_STD_PCT ({})",
                    lo,
                    hi
                );
            }
        }
        for (label, v) in [("VOL_MIN_STD_PCT", self.min_std_pct), ("VOL_MAX_STD_PCT", self.max_std_pct)] {
            if let Some(m) = v {
                if m < Decimal::ZERO || m > dec!(100) {
                    anyhow::bail!("{} must be between 0 and 100, got {}", label, m);
                }
            }
        }
        Ok(())
    }

    pub fn is_disabled(&self) -> bool {
        self.min_std_pct.is_none() && self.max_std_pct.is_none()
    }
}

pub fn compute_return_std_pct(candles: &[Candle], sample_bars: usize) -> Option<Decimal> {
    if sample_bars < 2 || candles.len() < sample_bars + 1 {
        return None;
    }
    let start = candles.len() - (sample_bars + 1);
    let slice = &candles[start..];
    let mut returns = Vec::with_capacity(sample_bars);
    for w in slice.windows(2) {
        let prev = w[0].close.to_f64()?;
        let curr = w[1].close.to_f64()?;
        if prev == 0.0 {
            return None;
        }
        returns.push((curr - prev) / prev);
    }
    if returns.len() < 2 {
        return None;
    }
    let n = returns.len() as f64;
    let mean: f64 = returns.iter().sum::<f64>() / n;
    let variance: f64 = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
    let std = variance.sqrt();
    Decimal::try_from(std * 100.0).ok()
}

pub fn passes_volatility_filter(candles: &[Candle], cfg: &VolatilityFilterConfig) -> bool {
    if cfg.is_disabled() {
        return true;
    }
    let Some(vol_pct) = compute_return_std_pct(candles, cfg.sample_bars) else {
        return true;
    };
    if let Some(min) = cfg.min_std_pct {
        if vol_pct < min {
            return false;
        }
    }
    if let Some(max) = cfg.max_std_pct {
        if vol_pct > max {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn candle_close(close: f64) -> Candle {
        let d = Decimal::try_from(close).unwrap();
        Candle {
            timestamp: Utc::now(),
            open: d,
            high: d,
            low: d,
            close: d,
            volume: Decimal::from(1000),
            taker_buy_ratio: None,
        }
    }

    #[test]
    fn flat_prices_low_vol_passes_max_bound() {
        let candles: Vec<Candle> = (0..25).map(|_| candle_close(100.0)).collect();
        let cfg = VolatilityFilterConfig {
            min_std_pct: None,
            max_std_pct: Some(dec!(1.0)),
            sample_bars: 20,
        };
        assert!(passes_volatility_filter(&candles, &cfg));
    }

    #[test]
    fn oscillating_prices_high_vol_blocked_by_max() {
        let mut candles = Vec::new();
        for i in 0..25 {
            let p = if i % 2 == 0 { 100.0 } else { 110.0 };
            candles.push(candle_close(p));
        }
        let cfg = VolatilityFilterConfig {
            min_std_pct: None,
            max_std_pct: Some(dec!(0.01)),
            sample_bars: 20,
        };
        assert!(!passes_volatility_filter(&candles, &cfg));
    }

    #[test]
    fn disabled_filter_always_passes() {
        let candles: Vec<Candle> = (0..10).map(|_| candle_close(100.0)).collect();
        let cfg = VolatilityFilterConfig::default();
        assert!(passes_volatility_filter(&candles, &cfg));
    }
}
