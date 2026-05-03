use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::types::{Direction, TradeSignal};

/// Ceil to $0.01 tick for CLOB limit compatibility (worst-case buy price).
fn ceil_to_cent_tick(price: Decimal) -> Decimal {
    if price <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let scaled = price * dec!(100);
    let n = scaled.ceil();
    (n / dec!(100)).min(dec!(0.99))
}

/// Worst-case outcome token price (slippage applied, capped at 0.99) for buying `direction`.
/// Matches the pricing leg of [`calculate`]; used when [`crate::market_matcher`] overrides direction
/// so `token_price` stays aligned with YES vs NO token.
pub(crate) fn token_price_for_direction(
    market_yes_price: Decimal,
    direction: Direction,
    slippage_bps: Decimal,
) -> Decimal {
    let base_price = match direction {
        Direction::Yes => market_yes_price,
        Direction::No => dec!(1) - market_yes_price,
    };
    let raw = (base_price * (dec!(1) + slippage_bps)).min(dec!(0.99));
    ceil_to_cent_tick(raw)
}

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
    let direction = if edge > Decimal::ZERO {
        Direction::Yes
    } else {
        Direction::No
    };

    let token_price = token_price_for_direction(market_yes_price, direction, slippage_bps);

    Some(TradeSignal {
        direction,
        edge: abs_edge,
        token_price,
    })
}

/// Recalculate edge and token price for a **forced** direction (from [`crate::market_matcher`]).
///
/// `signal.probability` is YES-implied (prob that the original market YES token wins).
/// When `market_matcher` overrides to the opposite direction, the effective probability
/// for the target token flips: YES edge = `prob - yes_price`, NO edge = `(1 - prob) - (1 - yes_price)`.
/// Returns `None` if the recalculated edge is non-positive or below `min_edge` (aligned with [`calculate`]).
pub fn recalculate_for_direction(
    signal_probability: Decimal,
    market_yes_price: Decimal,
    forced_direction: Direction,
    slippage_bps: Decimal,
    min_edge: Decimal,
) -> Option<TradeSignal> {
    let edge = match forced_direction {
        Direction::Yes => signal_probability - market_yes_price,
        Direction::No => (dec!(1) - signal_probability) - (dec!(1) - market_yes_price),
    };

    if edge < min_edge {
        return None;
    }

    let token_price = token_price_for_direction(market_yes_price, forced_direction, slippage_bps);

    Some(TradeSignal {
        direction: forced_direction,
        edge,
        token_price,
    })
}

/// Like [`calculate`] but ignores `min_edge` — always returns a [`TradeSignal`] for counterfactual / shadow logging.
pub fn calculate_unchecked(
    signal_probability: Decimal,
    market_yes_price: Decimal,
    slippage_bps: Decimal,
) -> TradeSignal {
    let edge_signed = signal_probability - market_yes_price;
    let abs_edge = edge_signed.abs();
    let direction = if edge_signed > Decimal::ZERO {
        Direction::Yes
    } else {
        // Tie (`edge_signed == 0`) matches [`calculate`]'s negative branch (NO).
        Direction::No
    };

    let token_price = token_price_for_direction(market_yes_price, direction, slippage_bps);

    TradeSignal {
        direction,
        edge: abs_edge,
        token_price,
    }
}

/// Like [`recalculate_for_direction`] but always returns a trade for the forced direction (shadow / what-if).
/// `edge` is `max(raw_edge, 0)` for the chosen side; pricing uses [`token_price_for_direction`].
pub fn recalculate_for_direction_unchecked(
    signal_probability: Decimal,
    market_yes_price: Decimal,
    forced_direction: Direction,
    slippage_bps: Decimal,
) -> TradeSignal {
    let raw = match forced_direction {
        Direction::Yes => signal_probability - market_yes_price,
        Direction::No => (dec!(1) - signal_probability) - (dec!(1) - market_yes_price),
    };
    let edge = raw.max(Decimal::ZERO);
    let token_price = token_price_for_direction(market_yes_price, forced_direction, slippage_bps);

    TradeSignal {
        direction: forced_direction,
        edge,
        token_price,
    }
}

/// Raw half-Kelly USDC size before `min_order_usdc` floor (used by [`kelly_size_with_caps_detail`]).
fn kelly_size_raw(
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
fn half_kelly_fraction(edge: Decimal, confidence: Decimal, max_position_pct: Decimal) -> Decimal {
    let kelly = edge * dec!(0.5) * confidence;
    kelly.min(max_position_pct)
}

/// Half-Kelly with **cheap token** and **hard** USDC caps.
///
/// Caps apply to the raw Kelly size, then the minimum order floor is applied.
/// Returns fraction and which cap applied (for trade logs).
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
    fn token_price_for_direction_matches_calculate() {
        let slip = dec!(0.002);
        let yes = dec!(0.805);
        let t = calculate(dec!(0.51), yes, dec!(0.29), slip).unwrap();
        assert_eq!(
            t.token_price,
            token_price_for_direction(yes, t.direction, slip)
        );
    }

    #[test]
    fn token_price_for_yes_not_below_yes_ask_reference() {
        // After market_matcher override to Yes, repriced token must track YES, not NO.
        let yes = dec!(0.805);
        let slip = dec!(0.002);
        let yes_px = token_price_for_direction(yes, Direction::Yes, slip);
        let no_px = token_price_for_direction(yes, Direction::No, slip);
        assert!(yes_px > dec!(0.50));
        assert!(no_px < dec!(0.50));
        assert!(yes_px > no_px);
    }

    /// Convenience: call the production sizing fn with neutral caps (no cheap/hard cap).
    fn sizing(
        edge: Decimal,
        confidence: Decimal,
        balance: Decimal,
        max_pct: Decimal,
        min_order: Decimal,
    ) -> KellySizingResult {
        kelly_size_with_caps_detail(
            edge,
            confidence,
            balance,
            max_pct,
            min_order,
            dec!(0.50),
            dec!(0.01),
            dec!(9999),
            None,
        )
    }

    #[test]
    fn kelly_size_caps_at_max() {
        let s = sizing(dec!(0.50), dec!(1.0), dec!(1000), dec!(0.05), dec!(5));
        assert!(s.size_usdc <= dec!(50), "should be capped at 5% = $50");
    }

    #[test]
    fn kelly_raw_rounding_never_exceeds_balance_times_max_pct() {
        let balance = dec!(310.59);
        let max_pct = dec!(0.03);
        let ceiling = balance * max_pct;
        let s = sizing(dec!(0.20), dec!(1.0), balance, max_pct, dec!(1));
        assert!(s.size_usdc > Decimal::ZERO);
        assert!(
            s.size_usdc <= ceiling,
            "rounded Kelly must not exceed RiskManager ceiling: size={} ceiling={}",
            s.size_usdc,
            ceiling
        );
    }

    #[test]
    fn kelly_size_scales_with_confidence() {
        let high = sizing(dec!(0.10), dec!(1.0), dec!(1000), dec!(0.10), dec!(5));
        let low = sizing(dec!(0.10), dec!(0.5), dec!(1000), dec!(0.10), dec!(5));
        assert!(
            high.size_usdc > low.size_usdc,
            "higher confidence should produce larger size"
        );
    }

    #[test]
    fn kelly_size_zero_balance() {
        let s = sizing(dec!(0.10), dec!(0.8), Decimal::ZERO, dec!(0.05), dec!(5));
        assert_eq!(s.size_usdc, Decimal::ZERO);
    }

    #[test]
    fn kelly_size_zero_edge() {
        let s = sizing(Decimal::ZERO, dec!(0.8), dec!(1000), dec!(0.05), dec!(5));
        assert_eq!(s.size_usdc, Decimal::ZERO);
    }

    #[test]
    fn kelly_size_very_small_fraction() {
        let s = sizing(dec!(0.001), dec!(0.5), dec!(1000), dec!(0.05), dec!(5));
        assert_eq!(
            s.size_usdc,
            Decimal::ZERO,
            "Very small edge should return zero"
        );
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
        let size_full = dec!(0.10) * dec!(1.0) * dec!(1000); // Full Kelly
        let s = sizing(dec!(0.10), dec!(1.0), dec!(1000), dec!(1.0), dec!(5));

        assert!(s.size_usdc < size_full);
        assert!(s.size_usdc > Decimal::ZERO);
    }

    #[test]
    fn kelly_caps_limits_cheap_token() {
        let s = kelly_size_with_caps_detail(
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
        assert_eq!(s.size_usdc, dec!(5));
        assert_eq!(s.cap_hit, "cheap_token");
    }

    #[test]
    fn kelly_caps_hard_limit() {
        let s = kelly_size_with_caps_detail(
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
        assert!(s.size_usdc <= dec!(25));
        assert!(s.size_usdc >= dec!(5));
        assert_eq!(s.cap_hit, "hard_cap");
    }

    #[test]
    fn kelly_confidence_scaling() {
        let high = sizing(dec!(0.10), dec!(1.0), dec!(1000), dec!(0.10), dec!(5));
        let low = sizing(dec!(0.10), dec!(0.5), dec!(1000), dec!(0.10), dec!(5));
        assert_eq!(high.size_usdc, low.size_usdc * dec!(2));
    }

    #[test]
    fn recalculate_same_direction_preserves_edge() {
        let orig = calculate(dec!(0.65), dec!(0.50), dec!(0.06), dec!(0.002)).unwrap();
        assert_eq!(orig.direction, Direction::Yes);

        let recalc = recalculate_for_direction(
            dec!(0.65),
            dec!(0.50),
            Direction::Yes,
            dec!(0.002),
            dec!(0.06),
        )
        .unwrap();
        assert_eq!(recalc.edge, orig.edge);
        assert_eq!(recalc.token_price, orig.token_price);
    }

    #[test]
    fn recalculate_flipped_direction_recomputes_edge() {
        // Signal prob 0.35 (YES-implied), yes_price 0.60 → calculate gives No, edge 0.25.
        let orig = calculate(dec!(0.35), dec!(0.60), dec!(0.06), dec!(0.002)).unwrap();
        assert_eq!(orig.direction, Direction::No);
        assert_eq!(orig.edge, dec!(0.25));

        // market_matcher forces Yes → edge for YES = 0.35 - 0.60 = -0.25 → no real edge.
        let recalc = recalculate_for_direction(
            dec!(0.35),
            dec!(0.60),
            Direction::Yes,
            dec!(0.002),
            dec!(0.06),
        );
        assert!(
            recalc.is_none(),
            "forcing YES when YES-implied prob < yes_price should yield no edge"
        );
    }

    #[test]
    fn recalculate_flipped_direction_with_real_edge() {
        // Signal prob 0.35 (YES-implied), yes_price 0.50 → calculate gives No, edge 0.15.
        let orig = calculate(dec!(0.35), dec!(0.50), dec!(0.06), dec!(0.002)).unwrap();
        assert_eq!(orig.direction, Direction::No);
        assert_eq!(orig.edge, dec!(0.15));

        // Force No direction (same as original): edge = (1-0.35) - (1-0.50) = 0.65 - 0.50 = 0.15.
        let recalc_no = recalculate_for_direction(
            dec!(0.35),
            dec!(0.50),
            Direction::No,
            dec!(0.002),
            dec!(0.06),
        )
        .unwrap();
        assert_eq!(recalc_no.edge, dec!(0.15));

        // Force Yes: edge = 0.35 - 0.50 = -0.15 → None.
        let recalc_yes = recalculate_for_direction(
            dec!(0.35),
            dec!(0.50),
            Direction::Yes,
            dec!(0.002),
            dec!(0.06),
        );
        assert!(recalc_yes.is_none());
    }

    #[test]
    fn recalculate_down_market_correct_edge() {
        // DOWN market scenario: signal DOWN, prob 0.30 (YES-implied low), yes_price 0.40.
        // calculate: edge = 0.30 - 0.40 = -0.10 → No, edge 0.10.
        // market_matcher(DOWN signal, DOWN question) → Yes.
        // recalculate for Yes: 0.30 - 0.40 = -0.10 → None (no edge for Yes).
        let recalc = recalculate_for_direction(
            dec!(0.30),
            dec!(0.40),
            Direction::Yes,
            dec!(0.002),
            dec!(0.06),
        );
        assert!(recalc.is_none());

        // But if yes_price were 0.20: Yes edge = 0.30 - 0.20 = 0.10 (real edge).
        let recalc2 = recalculate_for_direction(
            dec!(0.30),
            dec!(0.20),
            Direction::Yes,
            dec!(0.002),
            dec!(0.06),
        )
        .unwrap();
        assert_eq!(recalc2.edge, dec!(0.10));
        assert_eq!(recalc2.direction, Direction::Yes);
    }

    #[test]
    fn recalculate_rejected_when_below_min_edge() {
        assert!(
            recalculate_for_direction(
                dec!(0.65),
                dec!(0.50),
                Direction::Yes,
                dec!(0.002),
                dec!(0.20),
            )
            .is_none(),
            "0.15 edge should not pass min_edge 0.20"
        );
    }

    #[test]
    fn calculate_unchecked_matches_calculate_when_above_min_edge() {
        let min = dec!(0.01);
        let slip = dec!(0.002);
        let t = calculate(dec!(0.6), dec!(0.5), min, slip).unwrap();
        let u = calculate_unchecked(dec!(0.6), dec!(0.5), slip);
        assert_eq!(t.direction, u.direction);
        assert_eq!(t.edge, u.edge);
        assert_eq!(t.token_price, u.token_price);
    }

    #[test]
    fn recalculate_unchecked_returns_forced_side_even_when_below_min() {
        let u = recalculate_for_direction_unchecked(
            dec!(0.35),
            dec!(0.60),
            Direction::Yes,
            dec!(0.002),
        );
        assert_eq!(u.direction, Direction::Yes);
        assert_eq!(u.edge, Decimal::ZERO);
    }
}
