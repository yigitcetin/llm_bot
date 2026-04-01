//! Resolution checker: monitors open trades and resolves them when markets close.
//!
//! Each cycle checks open positions; when the market is closed, fetches the result from Gamma,
//! updates [`crate::risk::RiskManager`], and writes resolution into `trades.jsonl` via [`crate::metrics::MetricsLogger`].

use anyhow::Result;
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use tracing::{info, warn};

use crate::constants::GAMMA_API_BASE;
use crate::gamma::GammaToken;
use crate::metrics::MetricsLogger;
use crate::risk::RiskManager;
use crate::types::OpenPosition;

/// Gamma API market payload for `/markets/{id}` (resolution).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct GammaMarketResult {
    #[serde(rename = "conditionId")]
    condition_id: Option<String>,
    pub closed: Option<bool>,
    pub archived: Option<bool>,
    #[serde(rename = "resolutionPrice")]
    resolution_price: Option<String>,
    pub outcomes: Option<String>,
    #[serde(rename = "outcomePrices")]
    outcome_prices: Option<String>,
    pub tokens: Option<Vec<GammaToken>>,
}

/// Resolution checker: tracks open positions and settles them.
pub struct ResolutionChecker {
    http: reqwest::Client,
    gamma_api_base: String,
}

impl ResolutionChecker {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            gamma_api_base: GAMMA_API_BASE.to_string(),
        }
    }

    /// Check open positions and resolve closed markets.
    pub async fn check_and_resolve(
        &self,
        open_positions: &[OpenPosition],
        risk: &mut RiskManager,
        logger: &MetricsLogger,
    ) -> Result<()> {
        let now_ms = Utc::now().timestamp_millis();

        for pos in open_positions {
            if pos.end_date_ms > now_ms + 30_000 {
                continue;
            }

            info!(
                condition_id = %pos.condition_id,
                "checking resolution for closed market"
            );

            match self.fetch_market_result(&pos.condition_id).await {
                Ok(Some(yes_won)) => {
                    let pnl = pos.pnl_on_resolution(yes_won);

                    risk.record_resolution(pos, pnl);

                    if let Err(e) = logger.update_trade_resolution(
                        &pos.condition_id,
                        &pos.order_id,
                        yes_won,
                        pnl,
                    ) {
                        warn!(error = %e, "failed to update trade resolution in trades.jsonl");
                    }

                    info!(
                        condition_id = %pos.condition_id,
                        yes_won = yes_won,
                        pnl = %pnl,
                        "position resolved"
                    );
                }
                Ok(None) => {
                    info!(
                        condition_id = %pos.condition_id,
                        "market closed but result not yet available, retrying next cycle"
                    );
                }
                Err(e) => {
                    warn!(
                        condition_id = %pos.condition_id,
                        error = %e,
                        "failed to fetch market result"
                    );
                }
            }
        }

        Ok(())
    }

    /// Gamma API market outcome: `Some(true)` = YES won, `Some(false)` = NO won, `None` = not yet known.
    async fn fetch_market_result(&self, condition_id: &str) -> Result<Option<bool>> {
        let url = format!("{}/markets/{}", self.gamma_api_base, condition_id);

        let resp = self.http.get(&url).send().await?;

        if !resp.status().is_success() {
            warn!(
                condition_id = %condition_id,
                status = %resp.status(),
                "gamma API returned non-success for market result"
            );
            return Ok(None);
        }

        let market: GammaMarketResult = resp.json().await?;

        if !market.closed.unwrap_or(false) {
            return Ok(None);
        }

        if let Some(price_str) = &market.resolution_price {
            if let Ok(price) = price_str.parse::<Decimal>() {
                return Ok(Some(price >= dec!(0.5)));
            }
        }

        if let Some(tokens) = &market.tokens {
            for token in tokens {
                if let Ok(price) = token.price.parse::<Decimal>() {
                    if price >= dec!(0.99) {
                        let yes_won = matches!(
                            token.outcome.to_uppercase().as_str(),
                            "YES" | "UP"
                        );
                        return Ok(Some(yes_won));
                    }
                }
            }
        }

        if let (Some(outcomes_raw), Some(prices_raw)) = (&market.outcomes, &market.outcome_prices) {
            if let (Ok(outcomes), Ok(prices)) = (
                serde_json::from_str::<Vec<String>>(outcomes_raw),
                serde_json::from_str::<Vec<String>>(prices_raw),
            ) {
                for (outcome, price_str) in outcomes.iter().zip(prices.iter()) {
                    if let Ok(price) = price_str.parse::<Decimal>() {
                        if price >= dec!(0.99) {
                            let yes_won = matches!(
                                outcome.to_uppercase().as_str(),
                                "YES" | "UP"
                            );
                            return Ok(Some(yes_won));
                        }
                    }
                }
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Direction;
    use rust_decimal_macros::dec;

    fn test_position(direction: Direction) -> OpenPosition {
        OpenPosition {
            condition_id: "0xtest".to_string(),
            order_id: "order-1".to_string(),
            direction,
            entry_price: dec!(0.40),
            size_usdc: dec!(5),
            size_shares: dec!(12.5),
            end_date_ms: 0,
        }
    }

    #[test]
    fn yes_wins_bought_yes() {
        let pos = test_position(Direction::Yes);
        let pnl = pos.pnl_on_resolution(true);
        assert_eq!(pnl, dec!(7.50));
    }

    #[test]
    fn yes_wins_bought_no() {
        let pos = test_position(Direction::No);
        let pnl = pos.pnl_on_resolution(true);
        assert_eq!(pnl, dec!(-5));
    }

    #[test]
    fn no_wins_bought_no() {
        let pos = test_position(Direction::No);
        let pnl = pos.pnl_on_resolution(false);
        assert_eq!(pnl, dec!(7.50));
    }

    #[test]
    fn no_wins_bought_yes() {
        let pos = test_position(Direction::Yes);
        let pnl = pos.pnl_on_resolution(false);
        assert_eq!(pnl, dec!(-5));
    }
}
