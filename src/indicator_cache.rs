use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use crate::signals::{generate_signal, SignalConfig, TechnicalSignal};
use crate::spot_price::Candle;
use anyhow::Result;

/// Cache key for technical indicators
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct CacheKey {
    asset: String,
    interval: String,
    last_candle_timestamp: i64,
}

/// Cached indicator data (`Arc` avoids cloning `Decimal` fields and `reasoning` on every cache hit).
#[derive(Debug, Clone)]
struct CachedIndicators {
    signal: Arc<TechnicalSignal>,
    cached_at: DateTime<Utc>,
}

/// Cache for technical indicators to avoid recalculation
pub struct IndicatorCache {
    cache: HashMap<CacheKey, CachedIndicators>,
    max_age: Duration,
}

impl IndicatorCache {
    pub fn new(max_age_secs: i64) -> Self {
        Self {
            cache: HashMap::new(),
            max_age: Duration::seconds(max_age_secs),
        }
    }

    /// Get or compute signal for given candles
    pub fn get_or_compute(
        &mut self,
        asset: &str,
        interval: &str,
        candles: &[Candle],
        config: &SignalConfig,
    ) -> Result<Arc<TechnicalSignal>> {
        // Create cache key from last candle timestamp
        let last_timestamp = candles
            .last()
            .map(|c| c.timestamp.timestamp())
            .unwrap_or(0);

        let key = CacheKey {
            asset: asset.to_string(),
            interval: interval.to_string(),
            last_candle_timestamp: last_timestamp,
        };

        // Check cache
        if let Some(cached) = self.cache.get(&key) {
            let age = Utc::now() - cached.cached_at;
            if age < self.max_age {
                tracing::debug!(
                    asset = %asset,
                    interval = %interval,
                    age_secs = age.num_seconds(),
                    "cache HIT for indicators"
                );
                return Ok(Arc::clone(&cached.signal));
            }
        }

        // Cache miss - compute signal
        tracing::debug!(
            asset = %asset,
            interval = %interval,
            "cache MISS for indicators - computing"
        );

        let signal = Arc::new(
            generate_signal(candles, config).map_err(|e| anyhow::anyhow!(e))?,
        );

        self.cache.insert(
            key,
            CachedIndicators {
                signal: Arc::clone(&signal),
                cached_at: Utc::now(),
            },
        );

        Ok(signal)
    }

    /// Clear old entries from cache
    pub fn cleanup(&mut self) {
        let now = Utc::now();
        self.cache.retain(|_, cached| {
            let age = now - cached.cached_at;
            age < self.max_age
        });
    }

    /// Get cache stats for monitoring
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            size: self.cache.len(),
            max_age_secs: self.max_age.num_seconds(),
        }
    }
}

#[derive(Debug)]
pub struct CacheStats {
    pub size: usize,
    pub max_age_secs: i64,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rust_decimal::Decimal;

    use super::*;

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
    fn test_cache_hit() {
        let mut cache = IndicatorCache::new(300);
        let candles = mock_candles(100);
        let config = SignalConfig::default();

        // First call - cache miss
        let signal1 = cache.get_or_compute("btc", "1m", &candles, &config).unwrap();

        // Second call - cache hit
        let signal2 = cache.get_or_compute("btc", "1m", &candles, &config).unwrap();

        assert_eq!(signal1.probability, signal2.probability);
        assert_eq!(signal1.confidence, signal2.confidence);
        assert!(
            Arc::ptr_eq(&signal1, &signal2),
            "cache hit should return the same Arc, not clone Decimals/reasoning"
        );
    }

    #[test]
    fn test_cache_different_assets() {
        let mut cache = IndicatorCache::new(300);
        let candles = mock_candles(100);
        let config = SignalConfig::default();

        let _btc = cache.get_or_compute("btc", "1m", &candles, &config).unwrap();
        let _eth = cache.get_or_compute("eth", "1m", &candles, &config).unwrap();

        // Different assets should be cached separately
        assert_eq!(cache.stats().size, 2);
    }

    #[test]
    fn test_cache_cleanup() {
        let mut cache = IndicatorCache::new(1); // 1 second max age
        let candles = mock_candles(100);
        let config = SignalConfig::default();

        cache.get_or_compute("btc", "1m", &candles, &config).unwrap();
        assert_eq!(cache.stats().size, 1);

        // Wait and cleanup
        std::thread::sleep(std::time::Duration::from_secs(2));
        cache.cleanup();

        // Should be empty after cleanup
        assert_eq!(cache.stats().size, 0);
    }
}
