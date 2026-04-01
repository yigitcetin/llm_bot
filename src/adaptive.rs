//! Adjust `min_edge` / `min_confidence` from recent resolved trades in `trades.jsonl`.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::metrics::{read_trades_from_path, TradeRecord};

/// Win rate over last `window` resolved trades for `asset`, then nudge thresholds.
pub fn effective_thresholds(
    trades_path: &str,
    asset: &str,
    base_min_edge: Decimal,
    base_min_confidence: Decimal,
    window: usize,
    enabled: bool,
) -> (Decimal, Decimal) {
    if !enabled || window < 5 {
        return (base_min_edge, base_min_confidence);
    }

    let trades = match read_trades_from_path(trades_path) {
        Ok(t) => t,
        Err(_) => return (base_min_edge, base_min_confidence),
    };

    let mut resolved: Vec<&TradeRecord> = trades
        .iter()
        .filter(|t| t.asset == asset && t.outcome.is_some())
        .collect();
    if resolved.len() > window {
        resolved = resolved[resolved.len() - window..].to_vec();
    }
    let n = resolved.len();
    if n < 5 {
        return (base_min_edge, base_min_confidence);
    }

    let wins = resolved.iter().filter(|t| trade_won(t)).count();
    let wr = wins as f64 / n as f64;

    let mut edge_adj = Decimal::ZERO;
    let mut conf_adj = Decimal::ZERO;
    if wr < 0.45 {
        edge_adj = dec!(0.01);
        conf_adj = dec!(0.02);
    } else if wr > 0.55 {
        edge_adj = dec!(-0.005);
        conf_adj = dec!(-0.02);
    }

    let eff_edge = (base_min_edge + edge_adj).max(dec!(0.03)).min(dec!(0.25));
    let eff_conf = (base_min_confidence + conf_adj).max(dec!(0.5)).min(dec!(0.99));
    (eff_edge, eff_conf)
}

fn trade_won(t: &TradeRecord) -> bool {
    let Some(outcome) = t.outcome else {
        return false;
    };
    match (t.direction.as_str(), outcome) {
        ("YES", true) | ("NO", false) => true,
        _ => false,
    }
}
