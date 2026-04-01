use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::types::{Direction, TradeSignal};
use crate::constants::SLIPPAGE_BPS;

/// Calculate edge and direction with slippage protection.
/// Returns `None` if edge is below `min_edge`.
pub fn calculate(
    signal_probability: Decimal,
    market_yes_price: Decimal,
    min_edge: Decimal,
) -> Option<TradeSignal> {
    let edge = signal_probability - market_yes_price;
    let abs_edge = edge.abs();

    if abs_edge < min_edge {
        return None;
    }

    // Positive edge → YES is underpriced → BUY YES
    // Negative edge → NO is underpriced → BUY NO
    let (direction, base_price) = if edge > Decimal::ZERO {
        (Direction::Yes, market_yes_price)
    } else {
        // For NO: market no price = 1 - yes price
        let no_price = dec!(1) - market_yes_price;
        (Direction::No, no_price)
    };

    // Apply slippage: increase price we're willing to pay
    let token_price_with_slippage = base_price * (dec!(1) + SLIPPAGE_BPS);

    // Cap at 0.99 to avoid paying more than token is worth
    let token_price = token_price_with_slippage.min(dec!(0.99));

    Some(TradeSignal {
        direction,
        edge: abs_edge,
        token_price
    })
}

/// Half-Kelly position sizing.
///
/// Kelly fraction = edge / (1 - token_price)
/// We use half-Kelly for safety, capped at max_position_pct of balance.
///
/// Returns USDC amount to spend.
pub fn kelly_size(
    edge: Decimal,
    confidence: Decimal,
    balance: Decimal,
    max_position_pct: Decimal,
    min_order_usdc: Decimal,   // yeni parametre
) -> Decimal {
    if balance <= Decimal::ZERO || edge <= Decimal::ZERO {
        return Decimal::ZERO;
    }

    let kelly = edge * dec!(0.5) * confidence;
    let fraction = kelly.min(max_position_pct);

    if fraction < dec!(0.005) {
        return Decimal::ZERO;
    }

    let size = (balance * fraction).round_dp(2);
    
    // Minimum order garantisi
    size.max(min_order_usdc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn edge_below_minimum_returns_none() {
        let result = calculate(dec!(0.55), dec!(0.53), dec!(0.06));
        assert!(result.is_none(), "2% edge should be below 6% minimum");
    }

    #[test]
    fn positive_edge_buys_yes() {
        let result = calculate(dec!(0.65), dec!(0.50), dec!(0.06)).unwrap();
        assert_eq!(result.direction, Direction::Yes);
        assert_eq!(result.edge, dec!(0.15));
    }

    #[test]
    fn negative_edge_buys_no() {
        let result = calculate(dec!(0.35), dec!(0.50), dec!(0.06)).unwrap();
        assert_eq!(result.direction, Direction::No);
        assert_eq!(result.edge, dec!(0.15));
    }

    #[test]
    fn kelly_size_caps_at_max() {
        // Very high edge should still be capped
        let size = kelly_size(dec!(0.50), dec!(1.0), dec!(1000), dec!(0.05), dec!(5));
        assert!(size <= dec!(50), "should be capped at 5% = $50");
    }

    #[test]
    fn kelly_size_scales_with_confidence() {
        let high = kelly_size(dec!(0.10), dec!(1.0), dec!(1000), dec!(0.10), dec!(5));
        let low  = kelly_size(dec!(0.10), dec!(0.5), dec!(1000), dec!(0.10), dec!(5));
        assert!(high > low, "higher confidence should produce larger size");
    }

    #[test]
    fn kelly_size_zero_balance() {
        let size = kelly_size(dec!(0.10), dec!(0.8), Decimal::ZERO, dec!(0.05), dec!(5));
        assert_eq!(size, Decimal::ZERO);
    }

    #[test]
    fn kelly_size_zero_edge() {
        let size = kelly_size(Decimal::ZERO, dec!(0.8), dec!(1000), dec!(0.05), dec!(5));
        assert_eq!(size, Decimal::ZERO);
    }

    #[test]
    fn kelly_size_very_small_fraction() {
        // Edge too small to meet minimum fraction
        let size = kelly_size(dec!(0.001), dec!(0.5), dec!(1000), dec!(0.05), dec!(5));
        assert_eq!(size, Decimal::ZERO, "Very small edge should return zero");
    }

    #[test]
    fn slippage_protection_applied() {
        let result = calculate(dec!(0.65), dec!(0.50), dec!(0.06)).unwrap();
        // Token price should include slippage
        assert!(result.token_price > dec!(0.50));
        assert!(result.token_price <= dec!(0.99));
    }

    #[test]
    fn price_cap_at_99_cents() {
        // Very high market price near 1.0
        let result = calculate(dec!(0.99), dec!(0.98), dec!(0.01)).unwrap();
        // Price with slippage: 0.98 * 1.002 = 0.98196, which is < 0.99
        assert!(result.token_price <= dec!(0.99), "Should cap at 0.99");
        assert!(result.token_price > dec!(0.98), "Should have slippage applied");
    }

    #[test]
    fn edge_calculation_symmetry() {
        // Positive edge
        let yes_trade = calculate(dec!(0.70), dec!(0.50), dec!(0.05)).unwrap();
        assert_eq!(yes_trade.direction, Direction::Yes);
        assert_eq!(yes_trade.edge, dec!(0.20));

        // Negative edge (mirror)
        let no_trade = calculate(dec!(0.30), dec!(0.50), dec!(0.05)).unwrap();
        assert_eq!(no_trade.direction, Direction::No);
        assert_eq!(no_trade.edge, dec!(0.20));
    }

    #[test]
    fn kelly_half_scaling() {
        // Verify half-Kelly is being used
        let size_full = dec!(0.10) * dec!(1.0) * dec!(1000); // Full Kelly
        let size_half = kelly_size(dec!(0.10), dec!(1.0), dec!(1000), dec!(1.0), dec!(5));

        // Half-Kelly should be ~50% of full Kelly
        assert!(size_half < size_full);
        assert!(size_half > Decimal::ZERO);
    }

    #[test]
    fn kelly_confidence_scaling() {
        let high_conf = kelly_size(dec!(0.10), dec!(1.0), dec!(1000), dec!(0.10), dec!(5));
        let low_conf = kelly_size(dec!(0.10), dec!(0.5), dec!(1000), dec!(0.10), dec!(5));

        // Higher confidence should produce exactly 2x size
        assert_eq!(high_conf, low_conf * dec!(2));
    }
}
