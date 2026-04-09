use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::types::{Direction, TradeSignal};

/// Calculate edge and direction with slippage protection.
/// `slippage_bps` is a fraction added to the reference price (e.g. `0.002` for 20 bps).
/// Returns `None` if edge is below `min_edge`.
pub fn calculate(
    signal_probability: Decimal,
    market_yes_price: Decimal,
    min_edge: Decimal,
    slippage_bps: Decimal,
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
    let token_price_with_slippage = base_price * (dec!(1) + slippage_bps);

    // Cap at 0.99 to avoid paying more than token is worth
    let token_price = token_price_with_slippage.min(dec!(0.99));

    Some(TradeSignal {
        direction,
        edge: abs_edge,
        token_price,
    })
}

/// Raw half-Kelly USDC size before `min_order_usdc` floor (used by [`kelly_size_with_caps`]).
pub fn kelly_size_raw(
    edge: Decimal,
    confidence: Decimal,
    balance: Decimal,
    max_position_pct: Decimal,
) -> Decimal {
    if balance <= Decimal::ZERO || edge <= Decimal::ZERO {
        return Decimal::ZERO;
    }

    let kelly = edge * dec!(0.5) * confidence;
    let fraction = kelly.min(max_position_pct);

    if fraction < dec!(0.005) {
        return Decimal::ZERO;
    }

    // `round_dp(2)` can round slightly above `balance * max_position_pct`; RiskManager rejects that.
    let rounded = (balance * fraction).round_dp(2);
    let ceiling = balance * max_position_pct;
    rounded.min(ceiling)
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
    min_order_usdc: Decimal,
) -> Decimal {
    let size = kelly_size_raw(edge, confidence, balance, max_position_pct);
    if size <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let ceiling = balance * max_position_pct;
    // Minimum order floor, then hard ceiling (same as RiskManager).
    size.max(min_order_usdc).min(ceiling)
}

/// Result of [`kelly_size_with_caps_detail`] for JSONL / analytics.
#[derive(Debug, Clone)]
pub struct KellySizingResult {
    pub size_usdc: Decimal,
    /// Half-Kelly fraction actually used: `min(edge * 0.5 * confidence, max_position_pct)`.
    pub kelly_fraction: Decimal,
    /// Which constraint dominated before resolution: `none`, `cheap_token`, `hard_cap`, `min_order`, `max_position`.
    pub cap_hit: &'static str,
}

/// Half-Kelly fraction (before balance multiply), capped by `max_position_pct`.
pub fn half_kelly_fraction(
    edge: Decimal,
    confidence: Decimal,
    max_position_pct: Decimal,
) -> Decimal {
    let kelly = edge * dec!(0.5) * confidence;
    kelly.min(max_position_pct)
}

/// Half-Kelly with **cheap token** and **hard** USDC caps (plan P1/P4).
///
/// Caps apply to the raw Kelly size, then the minimum order floor is applied.
/// If the floor would exceed `cheap_max_usdc` in a cheap market, the caller should skip.
pub fn kelly_size_with_caps(
    edge: Decimal,
    confidence: Decimal,
    balance: Decimal,
    max_position_pct: Decimal,
    min_order_usdc: Decimal,
    token_price: Decimal,
    cheap_threshold: Decimal,
    cheap_max_usdc: Decimal,
    hard_cap: Option<Decimal>,
) -> Decimal {
    kelly_size_with_caps_detail(
        edge,
        confidence,
        balance,
        max_position_pct,
        min_order_usdc,
        token_price,
        cheap_threshold,
        cheap_max_usdc,
        hard_cap,
    )
    .size_usdc
}

/// Same as [`kelly_size_with_caps`] but returns fraction and which cap applied (for trade logs).
pub fn kelly_size_with_caps_detail(
    edge: Decimal,
    confidence: Decimal,
    balance: Decimal,
    max_position_pct: Decimal,
    min_order_usdc: Decimal,
    token_price: Decimal,
    cheap_threshold: Decimal,
    cheap_max_usdc: Decimal,
    hard_cap: Option<Decimal>,
) -> KellySizingResult {
    let kelly_fraction = half_kelly_fraction(edge, confidence, max_position_pct);
    let max_pct_hit = (edge * dec!(0.5) * confidence) > max_position_pct;

    let raw = kelly_size_raw(edge, confidence, balance, max_position_pct);
    if raw <= Decimal::ZERO {
        return KellySizingResult {
            size_usdc: Decimal::ZERO,
            kelly_fraction,
            cap_hit: if max_pct_hit { "max_position" } else { "none" },
        };
    }

    let mut size = raw;
    let mut cap_hit: &'static str = if max_pct_hit { "max_position" } else { "none" };

    if token_price > Decimal::ZERO && token_price < cheap_threshold {
        let capped = size.min(cheap_max_usdc);
        if capped < size {
            cap_hit = "cheap_token";
        }
        size = capped;
    }
    if let Some(cap) = hard_cap {
        if cap > Decimal::ZERO {
            let capped = size.min(cap);
            if capped < size {
                cap_hit = "hard_cap";
            }
            size = capped;
        }
    }

    let before_floor = size;
    let mut final_size = size.max(min_order_usdc);
    if final_size > before_floor {
        cap_hit = "min_order";
    }

    // Min-order floor can exceed `balance * max_position_pct` in edge cases; align with RiskManager.
    let hard_ceiling = balance * max_position_pct;
    if final_size > hard_ceiling {
        final_size = hard_ceiling;
        cap_hit = "max_position";
    }

    KellySizingResult {
        size_usdc: final_size,
        kelly_fraction,
        cap_hit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn edge_below_minimum_returns_none() {
        let result = calculate(dec!(0.55), dec!(0.53), dec!(0.06), dec!(0.002));
        assert!(result.is_none(), "2% edge should be below 6% minimum");
    }

    #[test]
    fn positive_edge_buys_yes() {
        let result = calculate(dec!(0.65), dec!(0.50), dec!(0.06), dec!(0.002)).unwrap();
        assert_eq!(result.direction, Direction::Yes);
        assert_eq!(result.edge, dec!(0.15));
    }

    #[test]
    fn negative_edge_buys_no() {
        let result = calculate(dec!(0.35), dec!(0.50), dec!(0.06), dec!(0.002)).unwrap();
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
    fn kelly_size_raw_rounding_never_exceeds_balance_times_max_pct() {
        // Fraction at max_position_pct: rounded USDC must stay <= balance * max_pct (RiskManager uses exact product).
        let balance = dec!(310.59);
        let max_pct = dec!(0.03);
        let ceiling = balance * max_pct;
        let size = kelly_size_raw(dec!(0.20), dec!(1.0), balance, max_pct);
        assert!(size > Decimal::ZERO);
        assert!(
            size <= ceiling,
            "rounded Kelly must not exceed RiskManager ceiling: size={} ceiling={}",
            size,
            ceiling
        );
    }

    #[test]
    fn kelly_size_scales_with_confidence() {
        let high = kelly_size(dec!(0.10), dec!(1.0), dec!(1000), dec!(0.10), dec!(5));
        let low = kelly_size(dec!(0.10), dec!(0.5), dec!(1000), dec!(0.10), dec!(5));
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
        let result = calculate(dec!(0.65), dec!(0.50), dec!(0.06), dec!(0.002)).unwrap();
        // Token price should include slippage
        assert!(result.token_price > dec!(0.50));
        assert!(result.token_price <= dec!(0.99));
    }

    #[test]
    fn price_cap_at_99_cents() {
        // Very high market price near 1.0
        let result = calculate(dec!(0.99), dec!(0.98), dec!(0.01), dec!(0.002)).unwrap();
        // Price with slippage: 0.98 * 1.002 = 0.98196, which is < 0.99
        assert!(result.token_price <= dec!(0.99), "Should cap at 0.99");
        assert!(
            result.token_price > dec!(0.98),
            "Should have slippage applied"
        );
    }

    #[test]
    fn edge_calculation_symmetry() {
        // Positive edge
        let yes_trade = calculate(dec!(0.70), dec!(0.50), dec!(0.05), dec!(0.002)).unwrap();
        assert_eq!(yes_trade.direction, Direction::Yes);
        assert_eq!(yes_trade.edge, dec!(0.20));

        // Negative edge (mirror)
        let no_trade = calculate(dec!(0.30), dec!(0.50), dec!(0.05), dec!(0.002)).unwrap();
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
    fn kelly_size_with_caps_limits_cheap_token() {
        // Large raw Kelly, but cheap token → cap at cheap_max
        let capped = kelly_size_with_caps(
            dec!(0.40),
            dec!(1.0),
            dec!(1000),
            dec!(0.50),
            dec!(5),
            dec!(0.05),
            dec!(0.15),
            dec!(5),
            None,
        );
        assert_eq!(capped, dec!(5));
    }

    #[test]
    fn kelly_size_with_caps_hard_limit() {
        let capped = kelly_size_with_caps(
            dec!(0.50),
            dec!(1.0),
            dec!(10000),
            dec!(0.50),
            dec!(5),
            dec!(0.50),
            dec!(0.15),
            dec!(5),
            Some(dec!(25)),
        );
        assert!(capped <= dec!(25));
        assert!(capped >= dec!(5));
    }

    #[test]
    fn kelly_confidence_scaling() {
        let high_conf = kelly_size(dec!(0.10), dec!(1.0), dec!(1000), dec!(0.10), dec!(5));
        let low_conf = kelly_size(dec!(0.10), dec!(0.5), dec!(1000), dec!(0.10), dec!(5));

        // Higher confidence should produce exactly 2x size
        assert_eq!(high_conf, low_conf * dec!(2));
    }
}
