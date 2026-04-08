use anyhow::Result;
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::HashMap;
use tracing::{info, warn};

use crate::constants::{GAMMA_API_BASE, GAMMA_EVENTS_FETCH_LIMIT, MIN_MARKET_CLOSE_TIME_SECS};
use crate::types::Market;

pub struct GammaClient {
    http: reqwest::Client,
    tag_id: u64,
}

// ── Raw API response types ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GammaEvent {
    pub slug: Option<String>,
    pub markets: Option<Vec<GammaMarket>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaMarket {
    pub condition_id: Option<String>,
    pub question: Option<String>,
    pub end_date_iso: Option<String>,
    pub end_date: Option<String>,
    pub tokens: Option<Vec<GammaToken>>,
    pub outcomes: Option<String>,
    pub outcome_prices: Option<String>,
    pub liquidity: Option<String>,
    pub closed: Option<bool>,
    pub archived: Option<bool>,
}

/// Gamma outcome token (YES/NO or UP/DOWN) with last price string from API.
#[derive(Debug, Clone, Deserialize)]
pub struct GammaToken {
    pub outcome: String,
    pub price: String,
}

/// Parse YES and NO prices from Gamma `tokens` and/or stringified `outcomes` + `outcomePrices` JSON arrays.
pub fn parse_yes_no_prices(
    tokens: Option<&[GammaToken]>,
    outcomes: Option<&str>,
    outcome_prices: Option<&str>,
) -> (Decimal, Decimal) {
    let mut yes_price = Decimal::ZERO;
    let mut no_price = Decimal::ZERO;

    if let Some(ts) = tokens {
        for token in ts {
            let price: Decimal = token.price.parse().unwrap_or(Decimal::ZERO);
            match token.outcome.to_uppercase().as_str() {
                "YES" | "UP" => yes_price = price,
                "NO" | "DOWN" => no_price = price,
                _ => {}
            }
        }
    }

    if yes_price.is_zero() || no_price.is_zero() {
        if let (Some(outcomes_raw), Some(prices_raw)) = (outcomes, outcome_prices) {
            if let (Ok(outcomes), Ok(prices)) = (
                serde_json::from_str::<Vec<String>>(outcomes_raw),
                serde_json::from_str::<Vec<String>>(prices_raw),
            ) {
                for (outcome, price_str) in outcomes.iter().zip(prices.iter()) {
                    let price: Decimal = price_str.parse().unwrap_or(Decimal::ZERO);
                    match outcome.to_uppercase().as_str() {
                        "YES" | "UP" => yes_price = price,
                        "NO" | "DOWN" => no_price = price,
                        _ => {}
                    }
                }
            }
        }
    }

    (yes_price, no_price)
}

// ── Client ────────────────────────────────────────────────────────────────────

impl GammaClient {
    pub fn new(http: reqwest::Client, tag_id: u64) -> Self {
        Self { http, tag_id }
    }

    /// Fetch active markets for configured assets/durations using Gamma `tag_id` + slug prefix.
    ///
    /// Polymarket Gamma slugs: `{asset}-updown-{duration}-…`
    pub async fn active_markets(
        &self,
        assets: &[String],
        durations: &[String],
    ) -> Result<Vec<Market>> {
        let events = match self.fetch_events_by_tag().await {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()),
        };

        // Keep only the nearest-to-close market per (asset, duration) pair.
        // Gamma returns a rolling window of overlapping markets (e.g. 6 for 30m / 5m).
        let mut best_by_pair: HashMap<(String, String), Market> = HashMap::new();

        for event in events {
            let slug = event.slug.as_deref().unwrap_or("");

            let mut matched: Option<(&str, &str)> = None;
            'pair: for asset in assets {
                for duration in durations {
                    let prefix = format!("{}-updown-{}", asset, duration);
                    if slug.starts_with(&prefix) {
                        matched = Some((asset.as_str(), duration.as_str()));
                        break 'pair;
                    }
                }
            }

            let Some((asset, duration)) = matched else {
                continue;
            };

            let slug_epoch_secs = parse_slug_epoch_secs(slug);
            let duration_secs = duration_to_secs(duration);
            for raw in event.markets.unwrap_or_default() {
                if raw.closed.unwrap_or(false) || raw.archived.unwrap_or(false) {
                    continue;
                }

                if let Some(market) =
                    parse_market(raw, asset, duration, slug_epoch_secs, duration_secs)
                {
                    let secs = market.secs_to_close();
                    if secs < MIN_MARKET_CLOSE_TIME_SECS {
                        continue;
                    }
                    let key = (market.asset.clone(), market.duration.clone());
                    match best_by_pair.get(&key) {
                        Some(existing) if existing.secs_to_close() <= secs => {}
                        _ => {
                            best_by_pair.insert(key, market);
                        }
                    }
                }
            }
        }

        let mut selected: Vec<Market> = best_by_pair.into_values().collect();
        selected.sort_by_key(|m| m.secs_to_close());

        for m in &selected {
            info!("{}", m.question);
        }

        Ok(selected)
    }

    /// `GET /events?tag_id=…` — one request per cycle; filter by slug in-process.
    async fn fetch_events_by_tag(&self) -> Result<Vec<GammaEvent>> {
        // Gamma API behaves much better when we constrain by the active time window
        // (see `poly` project's implementation).
        let now = Utc::now();
        let window_end = now + Duration::minutes(30);

        let url = format!("{}/events", GAMMA_API_BASE);

        let r = match self
            .http
            .get(&url)
            .query(&[
                ("tag_id", self.tag_id.to_string()),
                ("active", "true".to_string()),
                ("closed", "false".to_string()),
                ("archived", "false".to_string()),
                ("end_date_min", now.to_rfc3339()),
                ("end_date_max", window_end.to_rfc3339()),
                ("limit", GAMMA_EVENTS_FETCH_LIMIT.to_string()),
            ])
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(tag_id = self.tag_id, error = %e, "gamma fetch error");
                return Err(e.into());
            }
        };

        let status = r.status();
        let bytes = match r.bytes().await {
            Ok(b) => b,
            Err(e) => {
                warn!(tag_id = self.tag_id, error = %e, "gamma read body error");
                return Err(e.into());
            }
        };

        if !status.is_success() {
            warn!(
                tag_id = self.tag_id,
                status = %status,
                body_snippet = %utf8_snippet(&bytes),
                "gamma API non-success response"
            );
            return Ok(Vec::new());
        }

        let resp: Vec<GammaEvent> = match serde_json::from_slice(&bytes) {
            Ok(j) => j,
            Err(e) => {
                warn!(
                    tag_id = self.tag_id,
                    error = %e,
                    body_snippet = %utf8_snippet(&bytes),
                    "gamma JSON parse error"
                );
                return Ok(Vec::new());
            }
        };

        Ok(resp)
    }
}

fn utf8_snippet(bytes: &[u8]) -> String {
    const MAX: usize = 240;
    let s = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX)]);
    s.chars().take(MAX).collect()
}

fn parse_market(
    raw: GammaMarket,
    asset: &str,
    duration: &str,
    slug_epoch_secs: Option<i64>,
    duration_secs: Option<i64>,
) -> Option<Market> {
    let condition_id = raw.condition_id?;
    let question = raw.question?;

    // Parse end date, handling both old/new Gamma fields.
    // If Gamma gives date-only values, fall back to slug epoch (+ duration).
    let end_date_ms = raw
        .end_date_iso
        .as_deref()
        .and_then(parse_gamma_datetime)
        .or_else(|| raw.end_date.as_deref().and_then(parse_gamma_datetime))
        .map(|dt| dt.timestamp_millis())
        .or_else(|| match (slug_epoch_secs, duration_secs) {
            (Some(start), Some(dur)) => Some((start + dur) * 1000),
            (Some(start), None) => Some(start * 1000),
            _ => None,
        })?;

    let (yes_price, no_price) = parse_yes_no_prices(
        raw.tokens.as_deref(),
        raw.outcomes.as_deref(),
        raw.outcome_prices.as_deref(),
    );

    if yes_price.is_zero() || no_price.is_zero() {
        return None;
    }

    let liquidity: Decimal = raw
        .liquidity
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(Decimal::ZERO);

    Some(Market {
        condition_id,
        question,
        asset: asset.to_string(),
        duration: duration.to_string(),
        yes_price,
        no_price,
        end_date_ms,
        liquidity,
    })
}

fn parse_gamma_datetime(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(s).ok()
}

fn parse_slug_epoch_secs(slug: &str) -> Option<i64> {
    slug.rsplit('-').next()?.parse::<i64>().ok()
}

fn duration_to_secs(duration: &str) -> Option<i64> {
    if let Some(mins) = duration
        .strip_suffix('m')
        .and_then(|s| s.parse::<i64>().ok())
    {
        return Some(mins * 60);
    }
    if let Some(hours) = duration
        .strip_suffix('h')
        .and_then(|s| s.parse::<i64>().ok())
    {
        return Some(hours * 3600);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_market_valid() {
        let raw = GammaMarket {
            condition_id: Some("0xabc".to_string()),
            question: Some("Will BTC be up in 5m?".to_string()),
            end_date_iso: Some("2099-01-01T00:00:00Z".to_string()),
            end_date: None,
            tokens: Some(vec![
                GammaToken {
                    outcome: "YES".to_string(),
                    price: "0.55".to_string(),
                },
                GammaToken {
                    outcome: "NO".to_string(),
                    price: "0.45".to_string(),
                },
            ]),
            outcomes: None,
            outcome_prices: None,
            liquidity: Some("5000".to_string()),
            closed: Some(false),
            archived: Some(false),
        };
        let m = parse_market(raw, "btc", "5m", None, Some(300)).unwrap();
        assert_eq!(m.asset, "btc");
        assert_eq!(m.yes_price.to_string(), "0.55");
    }

    #[test]
    fn parse_market_missing_prices_returns_none() {
        let raw = GammaMarket {
            condition_id: Some("0xabc".to_string()),
            question: Some("Will BTC be up?".to_string()),
            end_date_iso: Some("2099-01-01T00:00:00Z".to_string()),
            end_date: None,
            tokens: Some(vec![]),
            outcomes: None,
            outcome_prices: None,
            liquidity: None,
            closed: None,
            archived: None,
        };
        assert!(parse_market(raw, "btc", "5m", None, Some(300)).is_none());
    }
}
