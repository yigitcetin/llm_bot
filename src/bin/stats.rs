//! Trade analytics: `trades.jsonl` → win rate, PnL, edge/confidence buckets.
//!
//! Usage: `cargo run --bin stats -- [--data-dir DIR] [--recent N]`

use anyhow::{Context, Result};
use polymarket_llm_bot::metrics::{read_trades_from_path, TradeRecord};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

#[derive(Default)]
struct BucketAgg {
    total: usize,
    wins: usize,
    pnl_sum: Decimal,
    pnl_n: usize,
}

impl BucketAgg {
    fn record_win(&mut self, won: bool) {
        self.total += 1;
        if won {
            self.wins += 1;
        }
    }

    fn record_pnl(&mut self, pnl: Decimal) {
        self.pnl_sum += pnl;
        self.pnl_n += 1;
    }
}

fn main() -> Result<()> {
    let mut data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".to_string());
    let mut recent_n = 20usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--data-dir" | "-d" => {
                data_dir = args.next().context("--data-dir requires a path")?;
            }
            "--recent" | "-n" => {
                recent_n = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .context("--recent requires a number")?;
            }
            "--help" | "-h" => {
                eprintln!(
                    "Usage: stats [--data-dir DIR] [--recent N]\n\
                     Reads {}/trades.jsonl (override with DATA_DIR or --data-dir).",
                    data_dir
                );
                return Ok(());
            }
            _ => eprintln!("unknown arg: {} (try --help)", a),
        }
    }

    let path = format!("{}/trades.jsonl", data_dir);
    let trades = read_trades_from_path(&path).with_context(|| format!("failed to read {}", path))?;

    if trades.is_empty() {
        println!("No trades in {}.", path);
        return Ok(());
    }

    println!("=== Trade stats ({}) ===\n", path);
    print_summary(&trades);
    by_key(&trades, "asset", |t| t.asset.clone());
    by_key(&trades, "duration", |t| t.duration.clone());
    by_key(&trades, "direction", |t| t.direction.clone());
    edge_buckets(&trades);
    confidence_buckets(&trades);
    telemetry_buckets(&trades);
    pnl_stats(&trades);
    println!("\n--- Last {} trades ---", recent_n);
    for t in trades.iter().rev().take(recent_n) {
        println!(
            "{} | {} {} | edge={} conf={} | outcome={:?} pnl={:?}",
            t.timestamp,
            t.asset,
            t.direction,
            t.edge,
            t.confidence,
            t.outcome,
            t.pnl
        );
    }

    Ok(())
}

fn parse_dec(s: &str) -> Option<Decimal> {
    s.parse().ok()
}

fn trade_won(t: &TradeRecord) -> Option<bool> {
    let outcome = t.outcome?;
    Some(matches!((t.direction.as_str(), outcome), ("YES", true) | ("NO", false)))
}

fn print_summary(trades: &[TradeRecord]) {
    let resolved: Vec<&TradeRecord> = trades.iter().filter(|t| t.outcome.is_some()).collect();
    let n = resolved.len();
    if n == 0 {
        println!("Resolved trades: 0 (no outcomes yet)");
        return;
    }
    let wins = resolved.iter().filter(|t| trade_won(t).unwrap_or(false)).count();
    println!("Resolved trades: {} | win rate: {:.1}%", n, 100.0 * wins as f64 / n as f64);
}

fn by_key<F: Fn(&TradeRecord) -> String>(trades: &[TradeRecord], label: &str, key: F) {
    let resolved: Vec<&TradeRecord> = trades.iter().filter(|t| t.outcome.is_some()).collect();
    if resolved.is_empty() {
        return;
    }
    let mut map: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for t in resolved {
        let k = key(t);
        let e = map.entry(k).or_insert((0, 0));
        e.0 += 1;
        if trade_won(t).unwrap_or(false) {
            e.1 += 1;
        }
    }
    println!("\n--- Win rate by {} ---", label);
    for (k, (tot, w)) in map {
        println!("  {}: {} / {} ({:.1}%)", k, w, tot, 100.0 * w as f64 / tot as f64);
    }
}

fn edge_bucket(edge: Decimal) -> &'static str {
    let e = edge.to_string().parse::<f64>().unwrap_or(0.0);
    if e < 0.10 {
        "0.06–0.10 (or <0.10)"
    } else if e < 0.15 {
        "0.10–0.15"
    } else {
        "0.15+"
    }
}

fn edge_buckets(trades: &[TradeRecord]) {
    let resolved: Vec<&TradeRecord> = trades.iter().filter(|t| t.outcome.is_some()).collect();
    if resolved.is_empty() {
        return;
    }
    let mut map: BTreeMap<&'static str, BucketAgg> = BTreeMap::new();
    for t in resolved {
        let Some(e) = parse_dec(&t.edge) else {
            continue;
        };
        let b = edge_bucket(e);
        let entry = map.entry(b).or_default();
        entry.record_win(trade_won(t).unwrap_or(false));
        if let Some(p) = t.pnl.as_ref().and_then(|s| parse_dec(s)) {
            entry.record_pnl(p);
        }
    }
    println!("\n--- Win rate & mean PnL by edge bucket ---");
    for (k, agg) in map {
        let wr = 100.0 * agg.wins as f64 / agg.total as f64;
        if agg.pnl_n > 0 {
            let mean = agg.pnl_sum / Decimal::from(agg.pnl_n);
            println!(
                "  {}: {} / {} ({:.1}%)  mean_pnl={} (n={})",
                k, agg.wins, agg.total, wr, mean, agg.pnl_n
            );
        } else {
            println!("  {}: {} / {} ({:.1}%)  mean_pnl=n/a", k, agg.wins, agg.total, wr);
        }
    }
}

fn conf_bucket(c: Decimal) -> &'static str {
    let v = c.to_string().parse::<f64>().unwrap_or(0.0);
    if v < 0.75 {
        "<0.75"
    } else if v < 0.88 {
        "0.75–0.88"
    } else {
        "0.88+"
    }
}

fn confidence_buckets(trades: &[TradeRecord]) {
    let resolved: Vec<&TradeRecord> = trades.iter().filter(|t| t.outcome.is_some()).collect();
    if resolved.is_empty() {
        return;
    }
    let mut map: BTreeMap<&'static str, BucketAgg> = BTreeMap::new();
    for t in resolved {
        let Some(c) = parse_dec(&t.confidence) else {
            continue;
        };
        let b = conf_bucket(c);
        let entry = map.entry(b).or_default();
        entry.record_win(trade_won(t).unwrap_or(false));
        if let Some(p) = t.pnl.as_ref().and_then(|s| parse_dec(s)) {
            entry.record_pnl(p);
        }
    }
    println!("\n--- Win rate & mean PnL by confidence bucket ---");
    for (k, agg) in map {
        let wr = 100.0 * agg.wins as f64 / agg.total as f64;
        if agg.pnl_n > 0 {
            let mean = agg.pnl_sum / Decimal::from(agg.pnl_n);
            println!(
                "  {}: {} / {} ({:.1}%)  mean_pnl={} (n={})",
                k, agg.wins, agg.total, wr, mean, agg.pnl_n
            );
        } else {
            println!("  {}: {} / {} ({:.1}%)  mean_pnl=n/a", k, agg.wins, agg.total, wr);
        }
    }
}

fn rsi_bucket(rsi: f64) -> &'static str {
    if rsi < 30.0 {
        "<30"
    } else if rsi < 50.0 {
        "30–50"
    } else if rsi < 70.0 {
        "50–70"
    } else {
        "70+"
    }
}

fn volume_ratio_bucket(v: f64) -> &'static str {
    if v < 1.0 {
        "<1"
    } else if v < 1.5 {
        "1–1.5"
    } else if v < 2.0 {
        "1.5–2"
    } else {
        "2+"
    }
}

fn vol_std_bucket(v: f64) -> &'static str {
    if v < 0.12 {
        "<0.12"
    } else if v < 0.25 {
        "0.12–0.25"
    } else {
        "0.25+"
    }
}

fn secs_to_close_bucket(s: i64) -> &'static str {
    if s < 120 {
        "<120s"
    } else if s < 600 {
        "120s–10m"
    } else {
        "10m+"
    }
}

/// RSI, volume_ratio, volatility_std_pct, secs_to_close (when present on rows).
fn telemetry_buckets(trades: &[TradeRecord]) {
    let resolved: Vec<&TradeRecord> = trades.iter().filter(|t| t.outcome.is_some()).collect();
    if resolved.is_empty() {
        return;
    }

    let mut has_any = false;

    let mut rsi_m: BTreeMap<&'static str, BucketAgg> = BTreeMap::new();
    for t in &resolved {
        let Some(r) = t.rsi else { continue };
        has_any = true;
        let b = rsi_bucket(r);
        let e = rsi_m.entry(b).or_default();
        e.record_win(trade_won(t).unwrap_or(false));
        if let Some(p) = t.pnl.as_ref().and_then(|s| parse_dec(s)) {
            e.record_pnl(p);
        }
    }
    if has_any {
        println!("\n--- Win rate & mean PnL by RSI bucket ---");
        for (k, agg) in &rsi_m {
            let wr = 100.0 * agg.wins as f64 / agg.total as f64;
            if agg.pnl_n > 0 {
                let mean = agg.pnl_sum / Decimal::from(agg.pnl_n);
                println!(
                    "  {}: {} / {} ({:.1}%)  mean_pnl={} (n={})",
                    k, agg.wins, agg.total, wr, mean, agg.pnl_n
                );
            } else {
                println!("  {}: {} / {} ({:.1}%)  mean_pnl=n/a", k, agg.wins, agg.total, wr);
            }
        }
    }

    has_any = false;
    let mut vr_m: BTreeMap<&'static str, BucketAgg> = BTreeMap::new();
    for t in &resolved {
        let Some(v) = t.volume_ratio else { continue };
        has_any = true;
        let b = volume_ratio_bucket(v);
        let e = vr_m.entry(b).or_default();
        e.record_win(trade_won(t).unwrap_or(false));
        if let Some(p) = t.pnl.as_ref().and_then(|s| parse_dec(s)) {
            e.record_pnl(p);
        }
    }
    if has_any {
        println!("\n--- Win rate & mean PnL by volume_ratio bucket ---");
        for (k, agg) in &vr_m {
            let wr = 100.0 * agg.wins as f64 / agg.total as f64;
            if agg.pnl_n > 0 {
                let mean = agg.pnl_sum / Decimal::from(agg.pnl_n);
                println!(
                    "  {}: {} / {} ({:.1}%)  mean_pnl={} (n={})",
                    k, agg.wins, agg.total, wr, mean, agg.pnl_n
                );
            } else {
                println!("  {}: {} / {} ({:.1}%)  mean_pnl=n/a", k, agg.wins, agg.total, wr);
            }
        }
    }

    has_any = false;
    let mut vs_m: BTreeMap<&'static str, BucketAgg> = BTreeMap::new();
    for t in &resolved {
        let Some(v) = t.volatility_std_pct else { continue };
        has_any = true;
        let b = vol_std_bucket(v);
        let e = vs_m.entry(b).or_default();
        e.record_win(trade_won(t).unwrap_or(false));
        if let Some(p) = t.pnl.as_ref().and_then(|s| parse_dec(s)) {
            e.record_pnl(p);
        }
    }
    if has_any {
        println!("\n--- Win rate & mean PnL by volatility_std_pct bucket ---");
        for (k, agg) in &vs_m {
            let wr = 100.0 * agg.wins as f64 / agg.total as f64;
            if agg.pnl_n > 0 {
                let mean = agg.pnl_sum / Decimal::from(agg.pnl_n);
                println!(
                    "  {}: {} / {} ({:.1}%)  mean_pnl={} (n={})",
                    k, agg.wins, agg.total, wr, mean, agg.pnl_n
                );
            } else {
                println!("  {}: {} / {} ({:.1}%)  mean_pnl=n/a", k, agg.wins, agg.total, wr);
            }
        }
    }

    has_any = false;
    let mut sc_m: BTreeMap<&'static str, BucketAgg> = BTreeMap::new();
    for t in &resolved {
        let Some(s) = t.secs_to_close else { continue };
        has_any = true;
        let b = secs_to_close_bucket(s);
        let e = sc_m.entry(b).or_default();
        e.record_win(trade_won(t).unwrap_or(false));
        if let Some(p) = t.pnl.as_ref().and_then(|s| parse_dec(s)) {
            e.record_pnl(p);
        }
    }
    if has_any {
        println!("\n--- Win rate & mean PnL by secs_to_close bucket ---");
        for (k, agg) in &sc_m {
            let wr = 100.0 * agg.wins as f64 / agg.total as f64;
            if agg.pnl_n > 0 {
                let mean = agg.pnl_sum / Decimal::from(agg.pnl_n);
                println!(
                    "  {}: {} / {} ({:.1}%)  mean_pnl={} (n={})",
                    k, agg.wins, agg.total, wr, mean, agg.pnl_n
                );
            } else {
                println!("  {}: {} / {} ({:.1}%)  mean_pnl=n/a", k, agg.wins, agg.total, wr);
            }
        }
    }
}

fn pnl_stats(trades: &[TradeRecord]) {
    let pnls: Vec<Decimal> = trades
        .iter()
        .filter_map(|t| t.pnl.as_ref().and_then(|p| parse_dec(p)))
        .collect();
    if pnls.is_empty() {
        println!("\nPnL: no resolved PnL rows");
        return;
    }
    let sum: Decimal = pnls.iter().copied().sum();
    let n = pnls.len() as f64;
    let mean = sum / Decimal::from(pnls.len());
    let mean_f = mean.to_string().parse::<f64>().unwrap_or(0.0);
    let var: f64 = pnls
        .iter()
        .map(|p| {
            let x = p.to_string().parse::<f64>().unwrap_or(0.0);
            (x - mean_f).powi(2)
        })
        .sum::<f64>()
        / n;
    let std = var.sqrt();
    let sharpe = if std > 1e-9 {
        mean_f / std * n.sqrt()
    } else {
        0.0
    };
    let mut sorted = pnls.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let min = sorted.first().copied().unwrap_or(Decimal::ZERO);
    let max = sorted.last().copied().unwrap_or(Decimal::ZERO);
    let mut cum = Decimal::ZERO;
    let mut max_dd = Decimal::ZERO;
    let mut peak = Decimal::ZERO;
    for t in trades.iter().filter(|t| t.pnl.is_some()) {
        let Some(p) = t.pnl.as_ref().and_then(|s| parse_dec(s)) else {
            continue;
        };
        cum += p;
        if cum > peak {
            peak = cum;
        }
        let dd = peak - cum;
        if dd > max_dd {
            max_dd = dd;
        }
    }
    println!("\n--- PnL ---");
    println!("  count: {}", pnls.len());
    println!("  sum: {}", sum);
    println!("  mean: {}", mean);
    println!("  std (sample): {:.6}", std);
    println!("  Sharpe-like (mean/std*sqrt n): {:.4}", sharpe);
    println!("  min / max: {} / {}", min, max);
    println!("  max drawdown (cumulative PnL path): {}", max_dd);
}
