use anyhow::Result;
use chrono::{Datelike, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
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
    /// JSON string array of CLOB token ids, e.g. `"[\"5113…\", \"8147…\"]"`, aligned with `outcomes`.
    pub clob_token_ids: Option<String>,
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

/// Parse Gamma `clobTokenIds` (JSON string array) into YES and NO token ids using `outcomes` labels.
/// Falls back to positional mapping when labels are missing: index 0 → yes, index 1 → no.
fn parse_clob_token_ids(
    clob_raw: Option<&str>,
    outcomes_raw: Option<&str>,
) -> Option<(String, String)> {
    let ids: Vec<String> = serde_json::from_str(clob_raw?).ok()?;
    if ids.len() != 2 {
        return None;
    }

    if let Some(outcomes_raw) = outcomes_raw {
        if let Ok(outcomes) = serde_json::from_str::<Vec<String>>(outcomes_raw) {
            if outcomes.len() == 2 {
                let mut yes_token: Option<String> = None;
                let mut no_token: Option<String> = None;
                for (label, id) in outcomes.iter().zip(ids.iter()) {
                    match label.to_uppercase().as_str() {
                        "YES" | "UP" => yes_token = Some(id.clone()),
                        "NO" | "DOWN" => no_token = Some(id.clone()),
                        _ => {}
                    }
                }
                if let (Some(y), Some(n)) = (yes_token, no_token) {
                    return Some((y, n));
                }
            }
        }
    }

    // Positional fallback: Polymarket binary markets list Up/Yes then Down/No.
    Some((ids[0].clone(), ids[1].clone()))
}

// ── Client ────────────────────────────────────────────────────────────────────

impl GammaClient {
    pub fn new(http: reqwest::Client, tag_id: u64) -> Self {
        Self { http, tag_id }
    }

    /// Fetch active markets for configured assets/durations using Gamma `tag_id` + slug prefix.
    ///
    /// Slug formats:
    /// - `{asset}-updown-{duration}-…` (e.g. `btc-updown-15m-…`)
    /// - `{asset}-up-or-down-…` for hourly markets when `duration` is `1h` (e.g. `bnb-up-or-down-april-…`)
    /// - `dogecoin-up-or-down-…` when asset is `doge` and duration is `1h`
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
                    if slug_matches_configured_pair(slug, asset, duration) {
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

/// Match Gamma event `slug` to configured `(asset, duration)`.
fn slug_matches_configured_pair(slug: &str, asset: &str, duration: &str) -> bool {
    let standard = format!("{}-updown-{}", asset, duration);
    slug.starts_with(&standard)
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

    // Priority order for end_date_ms:
    // 1. Question string close time (most reliable for 15m/1h up-or-down markets)
    // 2. Slug epoch + duration (for standard updown-{duration}-{epoch} slugs)
    // 3. Gamma endDateIso / endDate fields (can be date-only → 23:59 UTC, unreliable)
    let end_date_ms = parse_close_time_from_question(&question)
        .map(|dt| dt.timestamp_millis())
        .or_else(|| match (slug_epoch_secs, duration_secs) {
            (Some(start), Some(dur)) => Some((start + dur) * 1000),
            (Some(start), None) => Some(start * 1000),
            _ => None,
        })
        .or_else(|| {
            raw.end_date_iso
                .as_deref()
                .and_then(parse_gamma_datetime)
                .or_else(|| raw.end_date.as_deref().and_then(parse_gamma_datetime))
                .map(|dt| dt.timestamp_millis())
        })?;

    let (yes_token_id, no_token_id) =
        parse_clob_token_ids(raw.clob_token_ids.as_deref(), raw.outcomes.as_deref())?;

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
        yes_token_id,
        no_token_id,
    })
}

/// Extract the market close time from a Polymarket question string.
///
/// Handles formats like:
/// - "Solana Up or Down - April 11, 8:00AM-8:15AM ET"
/// - "BNB Up or Down - April 11, 9:30AM-9:45AM ET"
///
/// Returns the close time (the second time in the range) as UTC DateTime.
/// ET (US Eastern) is UTC-4 during EDT and UTC-5 during EST.
pub fn parse_close_time_from_question(question: &str) -> Option<chrono::DateTime<Utc>> {
    let dash_idx = question.find(" - ")?;
    let after_dash = &question[dash_idx + 3..];

    // Expected: "April 11, 8:00AM-8:15AM ET" or "April 11, 8:00PM-8:15PM ET"
    let et_suffix = after_dash.strip_suffix(" ET").or_else(|| after_dash.strip_suffix(" ET "))?;

    // Split "April 11, 8:00AM-8:15AM" on the last '-' to get the close time
    let time_range_sep = et_suffix.rfind('-')?;
    let close_time_str = et_suffix[time_range_sep + 1..].trim();

    // Date part: everything before the time range.
    // "April 11, 8:00AM" -> need the "April 11" part
    let comma_idx = et_suffix.find(',')?;
    let date_part = et_suffix[..comma_idx].trim();

    let now = Utc::now();
    let year = now.year();

    // Parse "April 11" -> NaiveDate
    let date_with_year = format!("{} {}", date_part, year);
    let date = NaiveDate::parse_from_str(&date_with_year, "%B %d %Y").ok()?;

    // Parse "8:15AM" or "10:30PM" -> NaiveTime
    let close_time = parse_et_time(close_time_str)?;

    let naive_dt = date.and_time(close_time);

    // ET offset: determine EDT vs EST based on date.
    // EDT (UTC-4) runs ~ second Sunday of March to first Sunday of November.
    let offset_hours = if is_edt(date) { 4 } else { 5 };
    let utc_dt = naive_dt + chrono::Duration::hours(offset_hours);

    Some(Utc.from_utc_datetime(&utc_dt))
}

/// Parse a time string like "8:15AM", "10:30PM", "12:00AM" into NaiveTime.
fn parse_et_time(s: &str) -> Option<NaiveTime> {
    let upper = s.trim().to_uppercase();
    let is_pm = upper.ends_with("PM");
    let time_part = upper.trim_end_matches("AM").trim_end_matches("PM");
    let parts: Vec<&str> = time_part.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let mut hour: u32 = parts[0].parse().ok()?;
    let minute: u32 = parts[1].parse().ok()?;

    if is_pm && hour != 12 {
        hour += 12;
    } else if !is_pm && hour == 12 {
        hour = 0;
    }

    NaiveTime::from_hms_opt(hour, minute, 0)
}

/// Rough EDT check: EDT is active from second Sunday in March to first Sunday in November.
fn is_edt(date: NaiveDate) -> bool {
    use chrono::Weekday;
    let (y, m, _d) = (date.year(), date.month(), date.day());
    if !(3..=11).contains(&m) {
        return false;
    }
    if m > 3 && m < 11 {
        return true;
    }
    // March: EDT starts at the second Sunday
    if m == 3 {
        let first_day = NaiveDate::from_ymd_opt(y, 3, 1).unwrap();
        let first_sunday = match first_day.weekday() {
            Weekday::Sun => 1,
            _ => 7 - first_day.weekday().num_days_from_sunday() + 1,
        };
        let second_sunday = first_sunday + 7;
        return date.day() >= second_sunday;
    }
    // November: EDT ends at the first Sunday
    let first_day = NaiveDate::from_ymd_opt(y, 11, 1).unwrap();
    let first_sunday = match first_day.weekday() {
        Weekday::Sun => 1,
        _ => 7 - first_day.weekday().num_days_from_sunday() + 1,
    };
    date.day() < first_sunday
}

fn parse_gamma_datetime(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt);
    }
    // Date-only `YYYY-MM-DD` from Gamma (hourly / daily events).
    let t = s.trim();
    if t.len() >= 10 {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(&t[..10], "%Y-%m-%d") {
            let naive = d.and_hms_opt(23, 59, 59)?;
            return Some(Utc.from_utc_datetime(&naive).fixed_offset());
        }
    }
    None
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
    fn slug_matches_standard_updown() {
        assert!(slug_matches_configured_pair(
            "btc-updown-15m-1766162100",
            "btc",
            "15m"
        ));
        assert!(!slug_matches_configured_pair(
            "btc-updown-15m-1766162100",
            "eth",
            "15m"
        ));
    }

    #[test]
    fn slug_does_not_match_up_or_down_format() {
        assert!(!slug_matches_configured_pair(
            "bnb-up-or-down-april-11-2026-6am-et",
            "bnb",
            "15m"
        ));
        assert!(!slug_matches_configured_pair(
            "dogecoin-up-or-down-april-11-2026-6am-et",
            "doge",
            "15m"
        ));
    }

    #[test]
    fn parse_gamma_datetime_accepts_date_only() {
        use chrono::Timelike;
        let dt = parse_gamma_datetime("2026-04-11").expect("date");
        assert_eq!(dt.date_naive().to_string(), "2026-04-11");
        assert_eq!(dt.time().hour(), 23);
    }

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
            clob_token_ids: Some(r#"["111111111111111111","222222222222222222"]"#.to_string()),
            closed: Some(false),
            archived: Some(false),
        };
        let m = parse_market(raw, "btc", "5m", None, Some(300)).unwrap();
        assert_eq!(m.asset, "btc");
        assert_eq!(m.yes_price.to_string(), "0.55");
        assert_eq!(m.yes_token_id, "111111111111111111");
        assert_eq!(m.no_token_id, "222222222222222222");
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
            clob_token_ids: Some(r#"["1","2"]"#.to_string()),
            closed: None,
            archived: None,
        };
        assert!(parse_market(raw, "btc", "5m", None, Some(300)).is_none());
    }

    #[test]
    fn parse_clob_token_ids_maps_up_down_labels() {
        let raw = GammaMarket {
            condition_id: Some("0xabc".to_string()),
            question: Some("Up or down?".to_string()),
            end_date_iso: Some("2099-01-01T00:00:00Z".to_string()),
            end_date: None,
            tokens: Some(vec![
                GammaToken {
                    outcome: "Up".to_string(),
                    price: "0.6".to_string(),
                },
                GammaToken {
                    outcome: "Down".to_string(),
                    price: "0.4".to_string(),
                },
            ]),
            outcomes: Some(r#"["Up","Down"]"#.to_string()),
            outcome_prices: None,
            liquidity: Some("1".to_string()),
            clob_token_ids: Some(r#"["999","888"]"#.to_string()),
            closed: Some(false),
            archived: Some(false),
        };
        let m = parse_market(raw, "eth", "5m", None, Some(300)).unwrap();
        assert_eq!(m.yes_token_id, "999");
        assert_eq!(m.no_token_id, "888");
    }

    #[test]
    fn parse_close_time_from_question_15m_am() {
        let q = "Solana Up or Down - April 11, 8:00AM-8:15AM ET";
        let dt = parse_close_time_from_question(q).expect("should parse");
        // 8:15 AM ET (EDT, UTC-4) = 12:15 UTC
        assert_eq!(dt.format("%Y-%m-%d %H:%M").to_string(), "2026-04-11 12:15");
    }

    #[test]
    fn parse_close_time_from_question_15m_pm() {
        let q = "BNB Up or Down - April 11, 3:45PM-4:00PM ET";
        let dt = parse_close_time_from_question(q).expect("should parse");
        // 4:00 PM ET (EDT, UTC-4) = 20:00 UTC
        assert_eq!(dt.format("%Y-%m-%d %H:%M").to_string(), "2026-04-11 20:00");
    }

    #[test]
    fn parse_close_time_from_question_noon_boundary() {
        let q = "Ethereum Up or Down - April 11, 11:45AM-12:00PM ET";
        let dt = parse_close_time_from_question(q).expect("should parse");
        // 12:00 PM ET (EDT, UTC-4) = 16:00 UTC
        assert_eq!(dt.format("%Y-%m-%d %H:%M").to_string(), "2026-04-11 16:00");
    }

    #[test]
    fn parse_close_time_from_question_unrecognized_returns_none() {
        assert!(parse_close_time_from_question("Will BTC go up?").is_none());
        assert!(parse_close_time_from_question("Random question").is_none());
    }

    #[test]
    fn is_edt_april_is_true() {
        let d = NaiveDate::from_ymd_opt(2026, 4, 11).unwrap();
        assert!(is_edt(d));
    }

    #[test]
    fn is_edt_january_is_false() {
        let d = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        assert!(!is_edt(d));
    }

    #[test]
    fn parse_et_time_basic() {
        assert_eq!(
            parse_et_time("8:15AM"),
            NaiveTime::from_hms_opt(8, 15, 0)
        );
        assert_eq!(
            parse_et_time("4:00PM"),
            NaiveTime::from_hms_opt(16, 0, 0)
        );
        assert_eq!(
            parse_et_time("12:00PM"),
            NaiveTime::from_hms_opt(12, 0, 0)
        );
        assert_eq!(
            parse_et_time("12:00AM"),
            NaiveTime::from_hms_opt(0, 0, 0)
        );
    }
}
