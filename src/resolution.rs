//! Market resolution watcher and PnL helpers (uses [`crate::positions`]).
//!
//! Not invoked from the main trading loop yet; kept for upcoming integration.

use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::{info, warn};

use crate::gamma::GammaClient;
use crate::positions::{Position, PositionTracker};
use crate::risk::RiskManager;
use crate::types::Direction;
use crate::resolution_checker::OpenPosition;

/// Watch for market resolutions and calculate P&L.
pub async fn watch_resolutions(
    gamma: &GammaClient,
    positions: &mut PositionTracker,
    risk: &mut RiskManager,
) -> Result<()> {
    let open_positions: Vec<Position> = positions.all().iter().map(|&p| p.clone()).collect();

    if open_positions.is_empty() {
        return Ok(());
    }

    info!(count = open_positions.len(), "checking resolutions for open positions");

    for position in open_positions {
        // Check if market has resolved
        match check_market_resolution(gamma, &position).await {
            Ok(Some(outcome)) => {
                // Calculate P&L
                let pnl = calculate_pnl(&position, outcome);

                info!(
                    condition_id = %position.condition_id,
                    asset = %position.asset,
                    side = ?position.side,
                    outcome = outcome,
                    entry_price = %position.entry_price,
                    size_shares = %position.size_shares,
                    pnl = %pnl,
                    "position resolved"
                );

                let open_pos = OpenPosition {
                    condition_id: position.condition_id.clone(),
                    order_id: position.order_id.clone(),
                    direction: match position.side {
                        Direction::Yes => "YES".to_string(),
                        Direction::No => "NO".to_string(),
                    },
                    entry_price: position.entry_price,
                    size_usdc: position.size_usdc,
                    size_shares: position.size_shares,
                    end_date_ms: position.opened_at.timestamp_millis(), // geçici
                };

                // Record resolution in risk manager
                risk.record_resolution(&open_pos, pnl);

                // Remove from position tracker
                positions.remove(&position.condition_id);
            }
            Ok(None) => {
                // Market not yet resolved
            }
            Err(e) => {
                warn!(
                    condition_id = %position.condition_id,
                    error = %e,
                    "failed to check resolution"
                );
            }
        }
    }

    Ok(())
}

/// Check if a market has resolved and return the outcome.
/// Returns None if market is still open.
/// Returns Some(true) if YES won, Some(false) if NO won.
async fn check_market_resolution(
    _gamma: &GammaClient,
    _position: &Position,
) -> Result<Option<bool>> {
    // Fetch market details from Gamma API
    // Note: This requires implementing a market_by_id method in GammaClient
    // For now, we'll use a simplified approach checking via active_markets

    // If market is no longer in active markets AND end_date has passed, it's likely resolved
    // We need to fetch the actual market data to get the outcome

    // TODO: Implement proper Gamma API call to get market resolution
    // For now, return None (market not resolved)

    Ok(None)
}

/// Calculate P&L for a resolved position.
///
/// For a YES position:
/// - If YES wins: profit = (1 - entry_price) * shares
/// - If NO wins:  loss = -entry_price * shares
///
/// For a NO position:
/// - If NO wins:  profit = (1 - entry_price) * shares
/// - If YES wins: loss = -entry_price * shares
fn calculate_pnl(position: &Position, yes_won: bool) -> Decimal {
    let won = match position.side {
        Direction::Yes => yes_won,
        Direction::No => !yes_won,
    };

    if won {
        // Profit: (1 - entry_price) * shares
        // This is the payout per share (1 USDC) minus what we paid
        (dec!(1) - position.entry_price) * position.size_shares
    } else {
        // Loss: we lose what we paid (entry_price * shares = size_usdc)
        -position.size_usdc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn mock_position(side: Direction, entry_price: Decimal, shares: Decimal) -> Position {
        Position {
            condition_id: "test".to_string(),
            asset: "btc".to_string(),
            duration: "5m".to_string(),
            side,
            entry_price,
            size_shares: shares,
            size_usdc: entry_price * shares,
            opened_at: chrono::Utc::now(),
            order_id: "test-order".to_string(),
        }
    }

    #[test]
    fn test_pnl_yes_position_wins() {
        // Bought YES at 0.6, 100 shares
        let pos = mock_position(Direction::Yes, dec!(0.6), dec!(100));
        // YES wins → each share pays 1 USDC → profit = (1 - 0.6) * 100 = 40
        let pnl = calculate_pnl(&pos, true);
        assert_eq!(pnl, dec!(40));
    }

    #[test]
    fn test_pnl_yes_position_loses() {
        // Bought YES at 0.6, 100 shares → paid 60 USDC
        let pos = mock_position(Direction::Yes, dec!(0.6), dec!(100));
        // NO wins → lose investment
        let pnl = calculate_pnl(&pos, false);
        assert_eq!(pnl, dec!(-60));
    }

    #[test]
    fn test_pnl_no_position_wins() {
        // Bought NO at 0.4, 100 shares
        let pos = mock_position(Direction::No, dec!(0.4), dec!(100));
        // NO wins → profit = (1 - 0.4) * 100 = 60
        let pnl = calculate_pnl(&pos, false);
        assert_eq!(pnl, dec!(60));
    }

    #[test]
    fn test_pnl_no_position_loses() {
        // Bought NO at 0.4, 100 shares → paid 40 USDC
        let pos = mock_position(Direction::No, dec!(0.4), dec!(100));
        // YES wins → lose investment
        let pnl = calculate_pnl(&pos, true);
        assert_eq!(pnl, dec!(-40));
    }
}
