//! Resolution checker: monitors open trades and resolves them when markets close.
//!
//! Each cycle checks open positions; when the market is closed, fetches the result from the
//! **CLOB API** (`GET /markets/{condition_id}`), updates [`crate::risk::RiskManager`], and writes
//! resolution into `trades.jsonl` via [`crate::metrics::MetricsLogger`].
//!
//! Note: Gamma's `/markets/{id}` expects an **integer** id, not a `conditionId` hash, so it
//! returns 422 for `0x…` values. The CLOB endpoint accepts `condition_id` in the path and
//! returns `tokens[].winner` which is the authoritative resolution source.

use anyhow::Result;
use chrono::Utc;
use serde::Deserialize;
use tracing::{info, warn};

use crate::metrics::MetricsLogger;
use crate::risk::RiskManager;
use crate::types::OpenPosition;

/// CLOB API market payload for `GET /markets/{condition_id}`.
#[derive(Debug, Deserialize)]
struct ClobMarketResult {
    pub closed: Option<bool>,
    pub tokens: Option<Vec<ClobToken>>,
}

/// Token inside the CLOB market response; `winner` is the resolution flag.
#[derive(Debug, Deserialize)]
struct ClobToken {
    pub outcome: String,
    pub winner: Option<bool>,
}

/// Resolution checker: tracks open positions and settles them.
pub struct ResolutionChecker {
    http: reqwest::Client,
    clob_host: String,
}

impl ResolutionChecker {
    pub fn new(http: reqwest::Client, clob_host: &str) -> Self {
        Self {
            http,
            clob_host: clob_host.to_string(),
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

    /// CLOB market outcome: `Some(true)` = YES/Up won, `Some(false)` = NO/Down won, `None` = not yet resolved.
    async fn fetch_market_result(&self, condition_id: &str) -> Result<Option<bool>> {
        let url = format!("{}/markets/{}", self.clob_host, condition_id);

        let resp = self.http.get(&url).send().await?;

        if !resp.status().is_success() {
            warn!(
                condition_id = %condition_id,
                status = %resp.status(),
                "CLOB API returned non-success for market result"
            );
            return Ok(None);
        }

        let market: ClobMarketResult = resp.json().await?;

        if !market.closed.unwrap_or(false) {
            return Ok(None);
        }

        if let Some(tokens) = &market.tokens {
            for token in tokens {
                if token.winner == Some(true) {
                    let yes_won = matches!(token.outcome.to_uppercase().as_str(), "YES" | "UP");
                    return Ok(Some(yes_won));
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
