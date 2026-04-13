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
    let eff_conf = (base_min_confidence + conf_adj)
        .max(dec!(0.5))
        .min(dec!(0.99));
    (eff_edge, eff_conf)
}

/// Compute an adaptive direction penalty from recent per-direction win rate.
///
/// Returns `(effective_penalty, direction_wr)`. When disabled or insufficient data,
/// returns `(base_penalty, None)`.
pub fn adaptive_direction_penalty(
    trades_path: &str,
    asset: &str,
    direction_str: &str,
    base_penalty: f64,
    window: usize,
    enabled: bool,
) -> (f64, Option<f64>) {
    if !enabled || window < 5 {
        return (base_penalty, None);
    }

    let trades = match read_trades_from_path(trades_path) {
        Ok(t) => t,
        Err(_) => return (base_penalty, None),
    };

    let mut dir_trades: Vec<&TradeRecord> = trades
        .iter()
        .filter(|t| t.asset == asset && t.direction == direction_str && t.outcome.is_some())
        .collect();
    if dir_trades.len() > window {
        dir_trades = dir_trades[dir_trades.len() - window..].to_vec();
    }
    let n = dir_trades.len();
    if n < 5 {
        return (base_penalty, None);
    }

    let wins = dir_trades.iter().filter(|t| trade_won(t)).count();
    let wr = wins as f64 / n as f64;

    let penalty = if wr < 0.40 {
        (base_penalty + 0.05).min(0.50)
    } else if wr < 0.50 {
        base_penalty
    } else if wr < 0.60 {
        base_penalty * 0.5
    } else {
        0.0
    };
    (penalty, Some(wr))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    use crate::metrics::TradeRecord;
    use crate::types::Direction;
    use rust_decimal_macros::dec;
    use uuid::Uuid;

    fn sample_row(asset: &str, direction: Direction, outcome: bool) -> TradeRecord {
        let mut r = TradeRecord::new(
            format!("c_{}", Uuid::new_v4()),
            asset.to_string(),
            "15m".to_string(),
            direction,
            dec!(0.5),
            dec!(5),
            dec!(10),
            dec!(0.6),
            dec!(0.8),
            dec!(0.1),
            "r".to_string(),
            format!("o_{}", Uuid::new_v4()),
        );
        r.outcome = Some(outcome);
        r
    }

    fn write_jsonl(path: &std::path::Path, rows: &[TradeRecord]) {
        let mut f = fs::File::create(path).expect("create temp trades");
        for tr in rows {
            writeln!(f, "{}", serde_json::to_string(tr).unwrap()).unwrap();
        }
    }

    #[test]
    fn effective_thresholds_disabled_returns_base() {
        let dir = std::env::temp_dir().join(format!("adapt_{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        write_jsonl(&p, &[]);
        let (e, c) = effective_thresholds(
            p.to_str().unwrap(),
            "btc",
            dec!(0.06),
            dec!(0.70),
            50,
            false,
        );
        assert_eq!(e, dec!(0.06));
        assert_eq!(c, dec!(0.70));
    }

    #[test]
    fn effective_thresholds_window_below_five_returns_base() {
        let dir = std::env::temp_dir().join(format!("adapt_{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let rows: Vec<_> = (0..5)
            .map(|_| sample_row("btc", Direction::Yes, true))
            .collect();
        write_jsonl(&p, &rows);
        let (e, c) =
            effective_thresholds(p.to_str().unwrap(), "btc", dec!(0.06), dec!(0.70), 4, true);
        assert_eq!(e, dec!(0.06));
        assert_eq!(c, dec!(0.70));
    }

    #[test]
    fn effective_thresholds_missing_file_returns_base() {
        let (e, c) = effective_thresholds(
            "/nonexistent/polymarket_adaptive_trades.jsonl",
            "btc",
            dec!(0.06),
            dec!(0.70),
            50,
            true,
        );
        assert_eq!(e, dec!(0.06));
        assert_eq!(c, dec!(0.70));
    }

    #[test]
    fn effective_thresholds_high_win_rate_nudges_edge_down() {
        let dir = std::env::temp_dir().join(format!("adapt_{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let rows: Vec<_> = (0..5)
            .map(|_| sample_row("btc", Direction::Yes, true))
            .collect();
        write_jsonl(&p, &rows);
        let (e, c) =
            effective_thresholds(p.to_str().unwrap(), "btc", dec!(0.06), dec!(0.70), 50, true);
        assert_eq!(e, dec!(0.055));
        assert_eq!(c, dec!(0.68));
    }

    #[test]
    fn effective_thresholds_low_win_rate_nudges_edge_up() {
        let dir = std::env::temp_dir().join(format!("adapt_{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let rows: Vec<_> = (0..5)
            .map(|_| sample_row("btc", Direction::Yes, false))
            .collect();
        write_jsonl(&p, &rows);
        let (e, c) =
            effective_thresholds(p.to_str().unwrap(), "btc", dec!(0.06), dec!(0.70), 50, true);
        assert_eq!(e, dec!(0.07));
        assert_eq!(c, dec!(0.72));
    }

    #[test]
    fn effective_thresholds_ignores_other_assets() {
        let dir = std::env::temp_dir().join(format!("adapt_{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let mut rows: Vec<_> = (0..5)
            .map(|_| sample_row("eth", Direction::Yes, true))
            .collect();
        rows.extend((0..5).map(|_| sample_row("btc", Direction::Yes, false)));
        write_jsonl(&p, &rows);
        let (e, c) =
            effective_thresholds(p.to_str().unwrap(), "btc", dec!(0.06), dec!(0.70), 50, true);
        assert_eq!(e, dec!(0.07));
        assert_eq!(c, dec!(0.72));
    }
}
