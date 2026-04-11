//! Market timing around signal generation: time-to-expiry guards and expiry probability dampening.
//!
//! - Optional **minimum seconds to close** (skip illiquid end-game).
//! - Optional **probability dampening** in the last N seconds of the window (reduce conviction near expiry).

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::debug;

use crate::signals::TechnicalSignal;
use crate::types::Market;

/// Parse Polymarket duration strings (`5m`, `15m`, `1h`) to seconds.
pub fn parse_duration_to_secs(duration: &str) -> Option<i64> {
    let s = duration.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_suffix('h') {
        return rest.parse::<i64>().ok().map(|h| h * 3600);
    }
    if let Some(rest) = s.strip_suffix('m') {
        return rest.parse::<i64>().ok().map(|m| m * 60);
    }
    if let Some(rest) = s.strip_suffix('s') {
        return rest.parse().ok();
    }
    None
}

/// If `min_secs` is set and remaining time is below it, we should skip trading this market.
pub fn below_min_secs_to_close(market: &Market, min_secs: Option<i64>) -> bool {
    let Some(min) = min_secs else {
        return false;
    };
    if min <= 0 {
        return false;
    }
    market.secs_to_close() < min
}

/// If `max_secs` is set and remaining time is above it, skip (too far from expiry / early window).
pub fn above_max_secs_to_close(market: &Market, max_secs: Option<i64>) -> bool {
    let Some(max) = max_secs else {
        return false;
    };
    if max <= 0 {
        return false;
    }
    market.secs_to_close() > max
}

/// Blend `probability` toward `0.5` when inside the last `dampen_last_secs` of the window.
/// `window_secs` is the full market duration (e.g. 900 for 15m). If unknown, pass `None` to skip dampening.
pub fn apply_expiry_probability_dampening(
    probability: Decimal,
    secs_to_close: i64,
    window_secs: Option<i64>,
    dampen_last_secs: i64,
) -> Decimal {
    if dampen_last_secs <= 0 {
        return probability;
    }
    let Some(window) = window_secs.filter(|w| *w > 0) else {
        return probability;
    };
    let elapsed = window.saturating_sub(secs_to_close);
    let start_dampen_at = window.saturating_sub(dampen_last_secs);
    if elapsed < start_dampen_at {
        return probability;
    }
    // Linear blend: at start of dampen window factor=0 (keep prob), at expiry factor=1 (full pull to 0.5)
    let span = dampen_last_secs.max(1) as f64;
    let pos = (elapsed - start_dampen_at).max(0) as f64;
    let t = (pos / span).min(1.0);
    let p = probability.to_f64().unwrap_or(0.5);
    let blended = p + (0.5 - p) * t;
    Decimal::try_from(blended).unwrap_or(dec!(0.5))
}

/// Apply optional dampening and clamp to the same band as [`crate::signals::generate_signal`].
pub fn apply_market_timing_to_signal(
    mut signal: TechnicalSignal,
    market: &Market,
    window_secs: Option<i64>,
    dampen_last_secs: Option<i64>,
) -> TechnicalSignal {
    let Some(dampen) = dampen_last_secs.filter(|d| *d > 0) else {
        return signal;
    };
    let secs = market.secs_to_close();
    signal.probability =
        apply_expiry_probability_dampening(signal.probability, secs, window_secs, dampen);
    signal.probability = signal.probability.max(dec!(0.15)).min(dec!(0.85));
    debug!(
        asset = %market.asset,
        secs_to_close = secs,
        prob = %signal.probability,
        "expiry probability dampening applied"
    );
    signal
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    #[test]
    fn parse_duration() {
        assert_eq!(parse_duration_to_secs("15m"), Some(900));
        assert_eq!(parse_duration_to_secs("5m"), Some(300));
    }

    #[test]
    fn below_min_secs_triggers() {
        let m = Market {
            condition_id: "x".into(),
            question: "q".into(),
            asset: "btc".into(),
            duration: "15m".into(),
            yes_price: dec!(0.5),
            no_price: dec!(0.5),
            end_date_ms: Utc::now().timestamp_millis() + 30_000,
            liquidity: dec!(1000),
            yes_token_id: "1".into(),
            no_token_id: "2".into(),
        };
        assert!(below_min_secs_to_close(&m, Some(60)));
        assert!(!below_min_secs_to_close(&m, Some(10)));
    }

    #[test]
    fn above_max_secs_triggers() {
        let m = Market {
            condition_id: "x".into(),
            question: "q".into(),
            asset: "btc".into(),
            duration: "15m".into(),
            yes_price: dec!(0.5),
            no_price: dec!(0.5),
            end_date_ms: Utc::now().timestamp_millis() + 800_000,
            liquidity: dec!(1000),
            yes_token_id: "1".into(),
            no_token_id: "2".into(),
        };
        assert!(above_max_secs_to_close(&m, Some(600)));
        assert!(!above_max_secs_to_close(&m, Some(900)));
        assert!(!above_max_secs_to_close(&m, None));
    }
}
