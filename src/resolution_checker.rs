//! Resolution checker: monitors open trades and resolves them when markets close.
//!
//! Two resolution paths:
//! 1. **In-memory positions** via `check_and_resolve` — checks `RiskManager::open_positions_detail`.
//! 2. **File-based** via `resolve_unresolved_trades` — scans `trades.jsonl` for rows with
//!    `outcome: null`, re-derives the true close time from the question string, and settles
//!    them against the CLOB API. This covers dry-run trades and bot restarts.
//! 3. **Shadow trades** via `resolve_unresolved_shadow_trades` — same as (2) for `shadow_trades.jsonl`.
//!
//! Market results come from the **CLOB API** (`GET /markets/{condition_id}`).

use anyhow::Result;
use chrono::Utc;
use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::{info, warn};

use crate::gamma::parse_close_time_from_question;
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

    /// Check in-memory open positions and resolve closed markets.
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

                    // Persist JSONL first so a failed write does not leave an in-memory resolution
                    // without a matching row (avoids double credit via credit_file_resolution later).
                    match logger.update_trade_resolution(
                        &pos.condition_id,
                        &pos.order_id,
                        yes_won,
                        pnl,
                    ) {
                        Err(e) => {
                            warn!(error = %e, "failed to update trade resolution in trades.jsonl — skipping in-memory settlement");
                            continue;
                        }
                        Ok(false) => {
                            warn!(
                                condition_id = %pos.condition_id,
                                order_id = %pos.order_id,
                                "no matching unresolved trades.jsonl row — skipping in-memory settlement (balance not updated)"
                            );
                            continue;
                        }
                        Ok(true) => {}
                    }

                    risk.record_resolution(pos, pnl);

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

    /// Scan `trades.jsonl` for unresolved rows whose market close time (parsed from question)
    /// has passed, then resolve them via the CLOB API and update the file.
    ///
    /// Also credits the `RiskManager` balance (stake + PnL) for each resolved trade so
    /// persisted balance stays accurate across restarts.
    pub async fn resolve_unresolved_trades(&self, risk: &mut RiskManager, logger: &MetricsLogger) -> Result<usize> {
        let unresolved = logger.read_unresolved_trades()?;
        if unresolved.is_empty() {
            return Ok(0);
        }

        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        let mut resolved_count = 0usize;

        for trade in &unresolved {
            let ref_date = trade.timestamp.date_naive();
            let close_ms = trade
                .question
                .as_deref()
                .and_then(|q| parse_close_time_from_question(q, ref_date))
                .map(|dt| dt.timestamp_millis());

            let market_closed = match close_ms {
                Some(ms) => ms + 30_000 < now_ms,
                None => {
                    if let Some(secs) = trade.secs_to_close {
                        let trade_ms = trade.timestamp.timestamp_millis();
                        trade_ms + secs * 1000 + 30_000 < now_ms
                    } else {
                        false
                    }
                }
            };

            if !market_closed {
                continue;
            }

            info!(
                condition_id = %trade.condition_id,
                question = trade.question.as_deref().unwrap_or("?"),
                "checking CLOB resolution for unresolved trade"
            );

            match self.fetch_market_result(&trade.condition_id).await {
                Ok(Some(yes_won)) => {
                    let direction = trade.direction;
                    let entry_price: Decimal =
                        trade.entry_price.parse().unwrap_or(Decimal::ZERO);
                    let size_usdc: Decimal =
                        trade.size_usdc.parse().unwrap_or(Decimal::ZERO);
                    let size_shares: Decimal =
                        trade.size_shares.parse().unwrap_or(Decimal::ZERO);

                    let pos = OpenPosition {
                        condition_id: trade.condition_id.clone(),
                        order_id: trade.order_id.clone(),
                        direction,
                        entry_price,
                        size_usdc,
                        size_shares,
                        end_date_ms: close_ms.unwrap_or(0),
                    };

                    let pnl = pos.pnl_on_resolution(yes_won);

                    match logger.update_trade_resolution(
                        &trade.condition_id,
                        &trade.order_id,
                        yes_won,
                        pnl,
                    ) {
                        Err(e) => {
                            warn!(error = %e, "failed to update trade resolution");
                        }
                        Ok(false) => {
                            warn!(
                                condition_id = %trade.condition_id,
                                order_id = %trade.order_id,
                                "trades.jsonl row not updated — not crediting balance"
                            );
                        }
                        Ok(true) => {
                            // Credit the balance even if the position isn't in-memory
                            // (restart scenario: balance was persisted with the deduction).
                            if risk.has_position(&trade.condition_id) {
                                risk.record_resolution(&pos, pnl);
                            } else {
                                risk.credit_file_resolution(pos.size_usdc, pnl);
                            }
                            info!(
                                condition_id = %trade.condition_id,
                                yes_won,
                                pnl = %pnl,
                                "trade resolved from file"
                            );
                            resolved_count += 1;
                        }
                    }
                }
                Ok(None) => {
                    info!(
                        condition_id = %trade.condition_id,
                        "market past close time but CLOB result not yet available"
                    );
                }
                Err(e) => {
                    warn!(
                        condition_id = %trade.condition_id,
                        error = %e,
                        "failed to fetch market result for file-based resolution"
                    );
                }
            }
        }

        Ok(resolved_count)
    }

    /// Same as [`Self::resolve_unresolved_trades`] but for `shadow_trades.jsonl` (counterfactual rows).
    pub async fn resolve_unresolved_shadow_trades(&self, logger: &MetricsLogger) -> Result<usize> {
        let unresolved = logger.read_unresolved_shadow_trades()?;
        if unresolved.is_empty() {
            return Ok(0);
        }

        let now_ms = Utc::now().timestamp_millis();
        let mut resolved_count = 0usize;

        for trade in &unresolved {
            let ref_date = trade.timestamp.date_naive();
            let close_ms = trade
                .question
                .as_deref()
                .and_then(|q| parse_close_time_from_question(q, ref_date))
                .map(|dt| dt.timestamp_millis());

            let market_closed = match close_ms {
                Some(ms) => ms + 30_000 < now_ms,
                None => {
                    if let Some(secs) = trade.secs_to_close {
                        let trade_ms = trade.timestamp.timestamp_millis();
                        trade_ms + secs * 1000 + 30_000 < now_ms
                    } else {
                        false
                    }
                }
            };

            if !market_closed {
                continue;
            }

            info!(
                condition_id = %trade.condition_id,
                question = trade.question.as_deref().unwrap_or("?"),
                "checking CLOB resolution for unresolved shadow trade"
            );

            match self.fetch_market_result(&trade.condition_id).await {
                Ok(Some(yes_won)) => {
                    let direction = trade.direction;
                    let entry_price: Decimal = trade.entry_price.parse().unwrap_or(Decimal::ZERO);
                    let size_usdc: Decimal = trade.size_usdc.parse().unwrap_or(Decimal::ZERO);
                    let size_shares: Decimal = trade.size_shares.parse().unwrap_or(Decimal::ZERO);

                    let pos = OpenPosition {
                        condition_id: trade.condition_id.clone(),
                        order_id: trade.order_id.clone(),
                        direction,
                        entry_price,
                        size_usdc,
                        size_shares,
                        end_date_ms: close_ms.unwrap_or(0),
                    };

                    let pnl = pos.pnl_on_resolution(yes_won);

                    match logger.update_shadow_trade_resolution(
                        &trade.condition_id,
                        &trade.order_id,
                        yes_won,
                        pnl,
                    ) {
                        Err(e) => {
                            warn!(error = %e, "failed to update shadow trade resolution");
                        }
                        Ok(false) => {
                            warn!(
                                condition_id = %trade.condition_id,
                                order_id = %trade.order_id,
                                "shadow_trades.jsonl row not updated — resolution not recorded"
                            );
                        }
                        Ok(true) => {
                            info!(
                                condition_id = %trade.condition_id,
                                yes_won,
                                pnl = %pnl,
                                "shadow trade resolved from file"
                            );
                            resolved_count += 1;
                        }
                    }
                }
                Ok(None) => {
                    info!(
                        condition_id = %trade.condition_id,
                        "shadow: market past close time but CLOB result not yet available"
                    );
                }
                Err(e) => {
                    warn!(
                        condition_id = %trade.condition_id,
                        error = %e,
                        "failed to fetch market result for shadow trade resolution"
                    );
                }
            }
        }

        Ok(resolved_count)
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
