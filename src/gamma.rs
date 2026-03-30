use anyhow::Result;
use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::warn;

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
    pub tokens: Option<Vec<GammaToken>>,
    pub liquidity: Option<String>,
    pub closed: Option<bool>,
    pub archived: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct GammaToken {
    pub outcome: String,
    pub price: String,
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

        let mut results = Vec::new();

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

            for raw in event.markets.unwrap_or_default() {
                if raw.closed.unwrap_or(false) || raw.archived.unwrap_or(false) {
                    continue;
                }

                if let Some(market) = parse_market(raw, asset, duration) {
                    if market.secs_to_close() < MIN_MARKET_CLOSE_TIME_SECS {
                        continue;
                    }
                    results.push(market);
                }
            }
        }

        Ok(results)
    }

    /// `GET /events?tag_id=…` — one request per cycle; filter by slug in-process.
    async fn fetch_events_by_tag(&self) -> Result<Vec<GammaEvent>> {
        let url = format!(
            "{}/events?tag_id={}&closed=false&archived=false&limit={}",
            GAMMA_API_BASE, self.tag_id, GAMMA_EVENTS_FETCH_LIMIT
        );

        let r = match self.http.get(&url).send().await {
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

fn parse_market(raw: GammaMarket, asset: &str, duration: &str) -> Option<Market> {
    let condition_id = raw.condition_id?;
    let question = raw.question?;
    let end_date_iso = raw.end_date_iso?;
    let tokens = raw.tokens?;

    // Parse end date
    let end_dt = chrono::DateTime::parse_from_rfc3339(&end_date_iso).ok()?;
    let end_date_ms = end_dt.timestamp_millis();

    // Parse YES and NO prices from tokens
    let mut yes_price = Decimal::ZERO;
    let mut no_price = Decimal::ZERO;
    for token in &tokens {
        let price: Decimal = token.price.parse().unwrap_or(Decimal::ZERO);
        match token.outcome.to_uppercase().as_str() {
            "YES" => yes_price = price,
            "NO"  => no_price = price,
            _     => {}
        }
    }

    if yes_price.is_zero() || no_price.is_zero() {
        return None;
    }

    let liquidity: Decimal = raw.liquidity
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_market_valid() {
        let raw = GammaMarket {
            condition_id: Some("0xabc".to_string()),
            question: Some("Will BTC be up in 5m?".to_string()),
            end_date_iso: Some("2099-01-01T00:00:00Z".to_string()),
            tokens: Some(vec![
                GammaToken { outcome: "YES".to_string(), price: "0.55".to_string() },
                GammaToken { outcome: "NO".to_string(),  price: "0.45".to_string() },
            ]),
            liquidity: Some("5000".to_string()),
            closed: Some(false),
            archived: Some(false),
        };
        let m = parse_market(raw, "btc", "5m").unwrap();
        assert_eq!(m.asset, "btc");
        assert_eq!(m.yes_price.to_string(), "0.55");
    }

    #[test]
    fn parse_market_missing_prices_returns_none() {
        let raw = GammaMarket {
            condition_id: Some("0xabc".to_string()),
            question: Some("Will BTC be up?".to_string()),
            end_date_iso: Some("2099-01-01T00:00:00Z".to_string()),
            tokens: Some(vec![]),
            liquidity: None,
            closed: None,
            archived: None,
        };
        assert!(parse_market(raw, "btc", "5m").is_none());
    }
}
