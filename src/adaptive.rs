//! Adjust `min_edge` / `min_confidence` from recent resolved trades in `trades.jsonl`.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::metrics::{read_trades_from_path, TradeRecord};
use crate::types::Direction;

/// Sum of realized PnL for this asset over the adaptive window below this (USDC) triggers extra tightening.
const PNL_CIRCUIT_THRESHOLD_USDC: f64 = -10.0;
const PNL_CIRCUIT_EDGE_BUMP: Decimal = dec!(0.03);
const PNL_CIRCUIT_CONF_BUMP: Decimal = dec!(0.02);

/// Penalty value meaning “do not trade this direction” (handled in [`crate::trading_loop`]).
pub const ADAPTIVE_DIRECTION_HARD_BLOCK_PENALTY: f64 = 1.0;

/// Win rate over last `window` resolved trades for `asset`, then nudge thresholds.
///
/// Also applies a **PnL circuit breaker**: if the sum of `pnl` over those trades is below
/// [`PNL_CIRCUIT_THRESHOLD_USDC`] (with at least 3 resolved rows), adds edge/confidence bumps so the
/// asset tightens even when headline WR has not yet crossed the WR bands.
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

    let total_pnl: f64 = resolved
        .iter()
        .filter_map(|t| t.pnl.as_ref()?.parse::<f64>().ok())
        .sum();

    let mut pnl_edge = Decimal::ZERO;
    let mut pnl_conf = Decimal::ZERO;
    if resolved.len() >= 3 && total_pnl < PNL_CIRCUIT_THRESHOLD_USDC {
        pnl_edge = PNL_CIRCUIT_EDGE_BUMP;
        pnl_conf = PNL_CIRCUIT_CONF_BUMP;
    }

    let n = resolved.len();
    if n < 5 {
        // Not enough trades for WR-based nudges — apply PnL circuit only when triggered.
        let eff_edge = (base_min_edge + pnl_edge).max(dec!(0.03)).min(dec!(0.25));
        let eff_conf = (base_min_confidence + pnl_conf)
            .max(dec!(0.5))
            .min(dec!(0.99));
        return (eff_edge, eff_conf);
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

    edge_adj += pnl_edge;
    conf_adj += pnl_conf;

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
///
/// Escalation (sample sizes are **per direction** in the rolling window):
/// - `n >= 2` and `WR == 0` → [`ADAPTIVE_DIRECTION_HARD_BLOCK_PENALTY`] (trade loop skips as hard block).
/// - `n >= 3` and `WR < 30%` → max soft penalty `0.50`.
/// - `n >= 5` → legacy graduated bands (unchanged).
pub fn adaptive_direction_penalty(
    trades_path: &str,
    asset: &str,
    trade_direction: Direction,
    base_penalty: f64,
    window: usize,
    enabled: bool,
) -> (f64, Option<f64>) {
    if !enabled {
        return (base_penalty, None);
    }

    let trades = match read_trades_from_path(trades_path) {
        Ok(t) => t,
        Err(_) => return (base_penalty, None),
    };

    let mut dir_trades: Vec<&TradeRecord> = trades
        .iter()
        .filter(|t| {
            t.asset == asset && t.direction == trade_direction && t.outcome.is_some()
        })
        .collect();
    if dir_trades.len() > window {
        dir_trades = dir_trades[dir_trades.len() - window..].to_vec();
    }
    let n = dir_trades.len();
    if n < 2 {
        return (base_penalty, None);
    }

    let wins = dir_trades.iter().filter(|t| trade_won(t)).count();
    let wr = wins as f64 / n as f64;

    if n >= 2 && wr == 0.0 {
        return (ADAPTIVE_DIRECTION_HARD_BLOCK_PENALTY, Some(wr));
    }
    if n >= 3 && wr < 0.30 {
        return (0.50_f64, Some(wr));
    }

    if n < 5 {
        return (base_penalty, Some(wr));
    }

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

/// Last `window` **resolved** trades (all assets): win rate, sum of `pnl` (USDC), count.
pub fn recent_closed_trade_metrics(trades_path: &str, window: usize) -> Option<(f64, f64, usize)> {
    if window == 0 {
        return None;
    }
    let trades = read_trades_from_path(trades_path).ok()?;
    let mut resolved: Vec<&TradeRecord> = trades.iter().filter(|t| t.outcome.is_some()).collect();
    if resolved.len() > window {
        resolved = resolved[resolved.len() - window..].to_vec();
    }
    let n = resolved.len();
    if n == 0 {
        return None;
    }
    let win_count = resolved.iter().filter(|t| trade_won(t)).count();
    let wr = win_count as f64 / n as f64;
    let pnl: f64 = resolved
        .iter()
        .filter_map(|t| t.pnl.as_ref()?.parse::<f64>().ok())
        .sum();
    Some((wr, pnl, n))
}

fn trade_won(t: &TradeRecord) -> bool {
    let Some(outcome) = t.outcome else {
        return false;
    };
    matches!(
        (t.direction, outcome),
        (Direction::Yes, true) | (Direction::No, false)
    )
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

    fn sample_row_with_pnl(
        asset: &str,
        direction: Direction,
        outcome: bool,
        pnl: &str,
    ) -> TradeRecord {
        let mut r = sample_row(asset, direction, outcome);
        r.pnl = Some(pnl.to_string());
        r
    }

    #[test]
    fn effective_thresholds_pnl_circuit_stacks_on_low_wr() {
        let dir = std::env::temp_dir().join(format!("adapt_{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let rows: Vec<_> = (0..5)
            .map(|_| sample_row_with_pnl("btc", Direction::Yes, false, "-3"))
            .collect();
        write_jsonl(&p, &rows);
        let (e, c) =
            effective_thresholds(p.to_str().unwrap(), "btc", dec!(0.06), dec!(0.70), 50, true);
        // WR < 0.45 -> +0.01 edge, +0.02 conf; sum PnL -15 -> +0.03 edge, +0.02 conf
        assert_eq!(e, dec!(0.10));
        assert_eq!(c, dec!(0.74));
    }

    #[test]
    fn adaptive_direction_hard_block_two_losses() {
        let dir = std::env::temp_dir().join(format!("adapt_{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let rows = vec![
            sample_row("btc", Direction::Yes, false),
            sample_row("btc", Direction::Yes, false),
        ];
        write_jsonl(&p, &rows);
        let (pen, wr) = adaptive_direction_penalty(
            p.to_str().unwrap(),
            "btc",
            Direction::Yes,
            0.08,
            50,
            true,
        );
        assert_eq!(pen, ADAPTIVE_DIRECTION_HARD_BLOCK_PENALTY);
        assert_eq!(wr, Some(0.0));
    }

    #[test]
    fn adaptive_direction_weak_side_max_penalty() {
        let dir = std::env::temp_dir().join(format!("adapt_{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let rows = vec![
            sample_row("sol", Direction::Yes, false),
            sample_row("sol", Direction::Yes, false),
            sample_row("sol", Direction::Yes, false),
            sample_row("sol", Direction::Yes, true),
        ];
        write_jsonl(&p, &rows);
        let (pen, wr) = adaptive_direction_penalty(
            p.to_str().unwrap(),
            "sol",
            Direction::Yes,
            0.08,
            50,
            true,
        );
        assert_eq!(pen, 0.50);
        assert!((wr.unwrap() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn recent_closed_trade_metrics_smoke() {
        let dir = std::env::temp_dir().join(format!("adapt_{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let rows: Vec<_> = (0..3)
            .map(|_| sample_row_with_pnl("eth", Direction::Yes, true, "1.5"))
            .collect();
        write_jsonl(&p, &rows);
        let m = recent_closed_trade_metrics(p.to_str().unwrap(), 50).unwrap();
        assert_eq!(m.2, 3);
        assert_eq!(m.1, 4.5);
        assert!((m.0 - 1.0).abs() < 1e-9);
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
