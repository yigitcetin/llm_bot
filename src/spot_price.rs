use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde::de::IgnoredAny;
use tracing::{debug, warn};

use crate::constants::{
    BINANCE_API_BASE, BINANCE_KLINES_MAX, DEFAULT_MAX_RETRIES, RETRY_BACKOFF_BASE_MS,
};

/// A single OHLCV candle from spot exchange
#[derive(Debug, Clone)]
pub struct Candle {
    pub timestamp: DateTime<Utc>,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
}

/// Client for fetching spot price data from exchanges
pub struct SpotPriceClient {
    http: reqwest::Client,
    exchange: String,
}

// Binance API response format
#[derive(Deserialize)]
struct BinanceKline(
    i64,    // Open time
    String, // Open
    String, // High
    String, // Low
    String, // Close
    String, // Volume
    IgnoredAny, // Close time + remaining Binance kline fields (unused)
    IgnoredAny,
    IgnoredAny,
    IgnoredAny,
    IgnoredAny,
    IgnoredAny,
);

#[derive(Deserialize)]
struct BinancePrice {
    #[allow(dead_code)]
    symbol: String,
    price: String,
}

impl SpotPriceClient {
    pub fn new(http: reqwest::Client, exchange: String) -> Self {
        Self { http, exchange }
    }

    /// Fetch recent candles from exchange
    ///
    /// For BTC, symbol will be "BTCUSDT"
    /// For ETH, symbol will be "ETHUSDT"
    pub async fn fetch_candles(
        &self,
        asset: &str,
        interval: &str,
        limit: usize,
    ) -> Result<Vec<Candle>> {
        self.fetch_candles_at_exchange(asset, interval, limit, self.exchange.as_str())
            .await
    }

    /// Same as [`fetch_candles`], but uses `exchange` instead of the client default (per-asset `SPOT_EXCHANGE_*`).
    pub async fn fetch_candles_at_exchange(
        &self,
        asset: &str,
        interval: &str,
        limit: usize,
        exchange: &str,
    ) -> Result<Vec<Candle>> {
        match exchange {
            "binance" => self.fetch_binance_candles(asset, interval, limit).await,
            _ => anyhow::bail!("Unsupported exchange: {}", exchange),
        }
    }

    /// Fetch current spot price for an asset
    pub async fn fetch_current_price(&self, asset: &str) -> Result<Decimal> {
        match self.exchange.as_str() {
            "binance" => self.fetch_binance_price(asset).await,
            _ => anyhow::bail!("Unsupported exchange: {}", self.exchange),
        }
    }

    async fn fetch_binance_candles(
        &self,
        asset: &str,
        interval: &str,
        limit: usize,
    ) -> Result<Vec<Candle>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        if limit <= BINANCE_KLINES_MAX {
            let symbol = format!("{}USDT", asset.to_uppercase());
            let url = format!(
                "{}/klines?symbol={}&interval={}&limit={}",
                BINANCE_API_BASE, symbol, interval, limit
            );
            debug!(symbol = %symbol, interval = %interval, limit = limit, "fetching Binance candles (single page)");
            return self.fetch_binance_klines_with_retries(&url, &symbol).await;
        }

        self.fetch_binance_candles_paged(asset, interval, limit).await
    }

    /// Pages backward using `endTime` until `target` candles are collected (oldest → newest).
    async fn fetch_binance_candles_paged(
        &self,
        asset: &str,
        interval: &str,
        target: usize,
    ) -> Result<Vec<Candle>> {
        let symbol = format!("{}USDT", asset.to_uppercase());
        let mut chunks: Vec<Vec<Candle>> = Vec::new();
        let mut remaining = target;
        let mut end_time_ms: Option<i64> = None;

        while remaining > 0 {
            let batch_limit = remaining.min(BINANCE_KLINES_MAX);
            let mut url = format!(
                "{}/klines?symbol={}&interval={}&limit={}",
                BINANCE_API_BASE, symbol, interval, batch_limit
            );
            if let Some(end) = end_time_ms {
                url.push_str(&format!("&endTime={}", end));
            }

            debug!(
                symbol = %symbol,
                interval = %interval,
                batch_limit = batch_limit,
                end_time_ms = ?end_time_ms,
                "fetching Binance candles (paged)"
            );

            let batch = self.fetch_binance_klines_with_retries(&url, &symbol).await?;
            if batch.is_empty() {
                break;
            }

            let next_end = batch
                .first()
                .expect("non-empty batch")
                .timestamp
                .timestamp_millis()
                - 1;
            let took = batch.len();
            chunks.push(batch);
            remaining = remaining.saturating_sub(took);

            if took < batch_limit || remaining == 0 {
                break;
            }
            end_time_ms = Some(next_end);
        }

        if chunks.is_empty() {
            anyhow::bail!("no Binance klines returned (paged fetch)");
        }

        // chunks[0] = newest segment, chunks[1] = older, … — reverse for chronological order
        let mut out: Vec<Candle> = chunks.into_iter().rev().flatten().collect();
        if out.len() > target {
            out = out[out.len() - target..].to_vec();
        }

        debug!(count = out.len(), "Binance paged fetch complete");
        Ok(out)
    }

    async fn fetch_binance_klines_with_retries(&self, url: &str, symbol: &str) -> Result<Vec<Candle>> {
        for attempt in 0..DEFAULT_MAX_RETRIES {
            if attempt > 0 {
                let backoff_ms = RETRY_BACKOFF_BASE_MS * 2_u64.pow(attempt);
                debug!(attempt = attempt + 1, backoff_ms, "retrying after backoff");
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            }

            match self.try_fetch_candles(url, symbol).await {
                Ok(candles) => {
                    debug!(count = candles.len(), "fetched candles from Binance");
                    return Ok(candles);
                }
                Err(e) if Self::is_retryable(&e) && attempt < DEFAULT_MAX_RETRIES - 1 => {
                    warn!(
                        error = %e,
                        attempt = attempt + 1,
                        max_retries = DEFAULT_MAX_RETRIES,
                        "retryable error occurred"
                    );
                    continue;
                }
                Err(e) => {
                    return Err(e).context(format!(
                        "failed to fetch Binance candles after {} attempts",
                        attempt + 1
                    ));
                }
            }
        }

        unreachable!("retry loop should always return or error")
    }

    /// Single attempt to fetch candles from Binance
    async fn try_fetch_candles(&self, url: &str, _symbol: &str) -> Result<Vec<Candle>> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .context("Binance HTTP request failed")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_else(|_| "<no body>".to_string());
            anyhow::bail!("Binance API error {}: {}", status, body);
        }

        let klines: Vec<BinanceKline> = response
            .json()
            .await
            .context("failed to parse Binance JSON response")?;

        let candles: Vec<Candle> = klines
            .into_iter()
            .filter_map(|k| {
                let timestamp = DateTime::from_timestamp_millis(k.0)?;
                Some(Candle {
                    timestamp,
                    open: k.1.parse().ok()?,
                    high: k.2.parse().ok()?,
                    low: k.3.parse().ok()?,
                    close: k.4.parse().ok()?,
                    volume: k.5.parse().ok()?,
                })
            })
            .collect();

        if candles.is_empty() {
            anyhow::bail!("no valid candles parsed from Binance response");
        }

        Ok(candles)
    }

    /// Check if an error is retryable
    fn is_retryable(error: &anyhow::Error) -> bool {
        let error_str = error.to_string();
        // Rate limit, server errors, network issues
        error_str.contains("429")
            || error_str.contains("500")
            || error_str.contains("502")
            || error_str.contains("503")
            || error_str.contains("504")
            || error_str.contains("timeout")
            || error_str.contains("connection")
    }

    async fn fetch_binance_price(&self, asset: &str) -> Result<Decimal> {
        let symbol = format!("{}USDT", asset.to_uppercase());

        let url = format!(
            "{}/ticker/price?symbol={}",
            BINANCE_API_BASE, symbol
        );

        let response = self.http
            .get(&url)
            .send()
            .await
            .context("Binance price request failed")?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Binance price API error: {}", body);
        }

        let price_data: BinancePrice = response
            .json()
            .await
            .context("Failed to parse Binance price response")?;

        price_data.price.parse()
            .context("Failed to parse price as Decimal")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_fetch_binance_candles() {
        let client = SpotPriceClient::new(
            reqwest::Client::new(),
            "binance".to_string()
        );

        let candles = client.fetch_candles("BTC", "1m", 10).await;
        assert!(candles.is_ok());

        let candles = candles.unwrap();
        assert_eq!(candles.len(), 10);
        assert!(candles[0].close > Decimal::ZERO);
    }

    #[tokio::test]
    async fn test_fetch_current_price() {
        let client = SpotPriceClient::new(
            reqwest::Client::new(),
            "binance".to_string()
        );

        let price = client.fetch_current_price("BTC").await;
        assert!(price.is_ok());
        assert!(price.unwrap() > Decimal::ZERO);
    }
}
