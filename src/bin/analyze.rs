//! Çoklu ufuk (1 / 7 / 14 / 30 gün) backtest + walk-forward raporu ve kural tabanlı metrik önerileri.

use anyhow::{Context, Result};
use clap::Parser;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;

use polymarket_llm_bot::backtest::{BacktestConfig, BacktestResult, run_backtest};
use polymarket_llm_bot::signals::SignalConfig;
use polymarket_llm_bot::volatility::VolatilityFilterConfig;
use polymarket_llm_bot::spot_price::{Candle, SpotPriceClient};
use polymarket_llm_bot::walk_forward::{WalkForwardConfig, WalkForwardResult, run_walk_forward};

const HORIZONS_DAYS: &[u32] = &[1, 7, 14, 30];

#[derive(Parser)]
#[command(
    name = "analyze",
    about = "Multi-horizon backtest + walk-forward. Loads .env; CANDLE_INTERVAL / CANDLE_LOOKBACK etc. override CLI defaults when set.",
    version
)]
struct Cli {
    #[arg(long, default_value = "btc")]
    asset: String,
    #[arg(long, default_value = "binance")]
    exchange: String,
    #[arg(long, default_value = "1m")]
    interval: String,
    #[arg(long, default_value_t = 100)]
    lookback: usize,
    #[arg(long, default_value_t = 5)]
    holding_period: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct BtSummary {
    trades: usize,
    total_return_pct: f64,
    sharpe_ratio: f64,
    max_drawdown_pct: f64,
    win_rate: f64,
    profit_factor: f64,
    /// Teknik sinyalin üretildiği bar sayısı (vol filtresinden önce).
    bars_with_signal: usize,
    /// Volatilite filtresinin kestiği bar sayısı (sinyal varken).
    vol_filter_skips: usize,
    /// Sinyal üretilen barların yüzde kaçında vol filtresi devreye girdi (0–100).
    vol_filter_skip_pct_of_signals: f64,
    volume_low_skips: usize,
    no_clear_signal_skips: usize,
    /// Sinyal üretim denemelerinde (hacim + net sinyal + başarılı) düşük hacim payı %.
    volume_low_pct_of_signal_attempts: f64,
}

#[derive(Debug, Serialize)]
struct WfSummary {
    iterations: usize,
    /// Tüm WF test pencerelerindeki toplam işlem sayısı (OOS).
    total_test_trades: usize,
    avg_test_sharpe: f64,
    avg_test_return_pct: f64,
    cumulative_pnl: String,
    consistency_score: f64,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct HorizonReport {
    horizon_days: u32,
    candles_used: usize,
    backtest: BtSummary,
    walk_forward: WfSummary,
    wf_windows: Option<String>,
}

#[derive(Debug, Serialize)]
struct ParamSuggestion {
    /// `.env` anahtarı (`--asset btc` → `MIN_EDGE_BTC` vb.; AppConfig ile uyumlu)
    env_var: String,
    /// Analizde kullanılan mevcut / taban değer
    current: String,
    /// Kural motorunun önerdiği değer
    suggested: String,
    #[serde(rename = "note")]
    not_tr: String,
}

#[derive(Debug, Serialize)]
struct StrategySnapshot {
    /// Analiz edilen varlık (`MIN_EDGE_{ASSET}` çözümlemesi için)
    asset: String,
    /// `.env` yüklendi (proje kökünde)
    dotenv_loaded: bool,
    candle_interval: String,
    candle_lookback: usize,
    min_edge: String,
    min_confidence: String,
    max_position_pct: String,
    min_order_usdc: String,
    daily_loss_limit_pct: String,
    initial_balance: String,
    rsi_period: usize,
    macd_fast: usize,
    macd_slow: usize,
    macd_signal: usize,
    /// Volatilite filtresi (— = kapalı)
    vol_max_std_pct: String,
    vol_sample_bars: usize,
    /// Spot hacim kalitesi (— = veto yok)
    volume_min_ratio: String,
    volume_avg_bars: usize,
}

#[derive(Debug, Serialize)]
struct AnalyzeOutput {
    strategy: StrategySnapshot,
    horizons: Vec<HorizonReport>,
    suggestions: Vec<ParamSuggestion>,
}

/// Bot ile aynı env anahtarları (`AppConfig` ile uyumlu); private key gerekmez.
#[derive(Debug, Clone)]
struct LoadedStrategy {
    /// Küçük harf (`btc`, `eth`) — `KEY_{ASSET}` env çözümlemesi
    asset: String,
    backtest: BacktestConfig,
    min_order_usdc: Decimal,
    daily_loss_limit_pct: Decimal,
    interval: String,
    lookback: usize,
    /// `dotenvy::dotenv()` proje kökünde `.env` buldu mu?
    dotenv_loaded: bool,
}

#[derive(Debug, Clone, Copy)]
struct RuleFlags {
    short_vs_long_mismatch: bool,
    drawdown_escalates: bool,
    wf_sharpe_all_neg: bool,
    wf_low_consistency: bool,
    heavy_trades_bad_pf: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let loaded = load_env_strategy(&cli)?;
    let snapshot = strategy_snapshot(&loaded);

    let exchange = parse_string_env_asset("SPOT_EXCHANGE", &loaded.asset, cli.exchange.clone());
    let max_candles = candles_for_days(30, &loaded.interval);
    let full = fetch_candles(&exchange, &cli.asset, &loaded.interval, max_candles)
        .await
        .with_context(|| format!("fetch up to {} candles for 30d window", max_candles))?;

    if full.is_empty() {
        anyhow::bail!("no candles returned");
    }

    let mut reports: Vec<HorizonReport> = Vec::new();

    for &days in HORIZONS_DAYS {
        let want = candles_for_days(days, &loaded.interval);
        let slice = tail_slice(&full, want);
        let n = slice.len();

        let bt = run_backtest(
            slice,
            &cli.asset,
            &loaded.backtest,
            loaded.lookback,
            cli.holding_period,
        )
        .with_context(|| format!("backtest failed for {}d horizon", days))?;

        let (wf, wf_meta) =
            run_wf_or_err(slice, &cli.asset, n, days, &loaded, cli.holding_period);

        reports.push(HorizonReport {
            horizon_days: days,
            candles_used: n,
            backtest: summarize_bt(&bt),
            walk_forward: wf,
            wf_windows: wf_meta,
        });
    }

    let flags = analyze_rule_flags(&reports);
    let suggestions = build_param_suggestions(&flags, &loaded);

    if cli.json {
        let out = AnalyzeOutput {
            strategy: snapshot,
            horizons: reports,
            suggestions,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print_human_table(&reports, &cli, &loaded);
        println!();
        for line in suggest_changes_text(&flags) {
            println!("{}", line);
        }
        println!();
        print_param_suggestions_table(&suggestions);
    }

    Ok(())
}

fn load_env_strategy(cli: &Cli) -> Result<LoadedStrategy> {
    let dotenv_loaded = dotenvy::dotenv().is_ok();
    let asset = cli.asset.trim().to_lowercase();

    let interval = parse_string_env_asset("CANDLE_INTERVAL", &asset, cli.interval.clone());

    let lookback = parse_usize_env_asset("CANDLE_LOOKBACK", &asset, cli.lookback);

    let volume_min_ratio = parse_opt_f64_env_asset("VOLUME_MIN_RATIO", &asset);
    let volume_avg_bars = parse_usize_env_asset("VOLUME_AVG_BARS", &asset, 20);
    if let Some(r) = volume_min_ratio {
        if r <= 0.0 || r > 5.0 {
            anyhow::bail!("VOLUME_MIN_RATIO must be in (0, 5], got {}", r);
        }
    }
    if volume_avg_bars < 5 || volume_avg_bars > 200 {
        anyhow::bail!(
            "VOLUME_AVG_BARS must be between 5 and 200, got {}",
            volume_avg_bars
        );
    }

    let signal_config = SignalConfig {
        rsi_period: parse_usize_env_asset("RSI_PERIOD", &asset, 14),
        macd_fast: parse_usize_env_asset("MACD_FAST", &asset, 12),
        macd_slow: parse_usize_env_asset("MACD_SLOW", &asset, 26),
        macd_signal: parse_usize_env_asset("MACD_SIGNAL", &asset, 9),
        volume_min_ratio,
        volume_avg_bars: volume_avg_bars.max(5),
    };

    let volatility_filter = VolatilityFilterConfig {
        min_std_pct: parse_opt_dec_env_asset("VOL_MIN_STD_PCT", &asset),
        max_std_pct: parse_opt_dec_env_asset("VOL_MAX_STD_PCT", &asset),
        sample_bars: parse_usize_env_asset("VOL_SAMPLE_BARS", &asset, 20),
    };
    volatility_filter.validate().map_err(|e| anyhow::anyhow!("{}", e))?;

    let backtest = BacktestConfig {
        initial_balance: parse_dec_env("INITIAL_BALANCE", dec!(200)),
        min_edge: parse_dec_env_asset("MIN_EDGE", &asset, dec!(0.06)),
        min_confidence: parse_dec_env_asset("MIN_CONFIDENCE", &asset, dec!(0.70)),
        max_position_pct: parse_dec_env_asset("MAX_POSITION_PCT", &asset, dec!(0.05)),
        signal_config,
        volatility_filter,
    };

    let min_order_usdc = parse_dec_env_asset("MIN_ORDER_USDC", &asset, dec!(5));
    let daily_loss_limit_pct = parse_dec_env_asset("DAILY_LOSS_LIMIT_PCT", &asset, dec!(0.10));

    Ok(LoadedStrategy {
        asset,
        backtest,
        min_order_usdc,
        daily_loss_limit_pct,
        interval,
        lookback,
        dotenv_loaded,
    })
}

fn strategy_snapshot(loaded: &LoadedStrategy) -> StrategySnapshot {
    let b = &loaded.backtest;
    let s = &b.signal_config;
    StrategySnapshot {
        asset: loaded.asset.clone(),
        dotenv_loaded: loaded.dotenv_loaded,
        candle_interval: loaded.interval.clone(),
        candle_lookback: loaded.lookback,
        min_edge: fmt_dec(b.min_edge),
        min_confidence: fmt_dec(b.min_confidence),
        max_position_pct: fmt_dec(b.max_position_pct),
        min_order_usdc: fmt_dec(loaded.min_order_usdc),
        daily_loss_limit_pct: fmt_dec(loaded.daily_loss_limit_pct),
        initial_balance: fmt_dec(b.initial_balance),
        rsi_period: s.rsi_period,
        macd_fast: s.macd_fast,
        macd_slow: s.macd_slow,
        macd_signal: s.macd_signal,
        vol_max_std_pct: b
            .volatility_filter
            .max_std_pct
            .map(fmt_dec)
            .unwrap_or_else(|| "—".to_string()),
        vol_sample_bars: b.volatility_filter.sample_bars,
        volume_min_ratio: s
            .volume_min_ratio
            .map(|v| format!("{v}"))
            .unwrap_or_else(|| "—".to_string()),
        volume_avg_bars: s.volume_avg_bars,
    }
}

fn fmt_opt_dec(d: Option<Decimal>) -> String {
    d.map(fmt_dec).unwrap_or_else(|| "—".to_string())
}

fn fmt_opt_f64(o: Option<f64>) -> String {
    o.map(|v| v.to_string()).unwrap_or_else(|| "—".to_string())
}

fn parse_dec_env(key: &str, default: Decimal) -> Decimal {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_key_asset(base: &str, asset: &str) -> String {
    format!("{}_{}", base, asset.to_uppercase())
}

fn parse_opt_dec_env_asset(key: &str, asset: &str) -> Option<Decimal> {
    let ks = env_key_asset(key, asset);
    std::env::var(&ks)
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| std::env::var(key).ok().and_then(|v| v.parse().ok()))
}

fn parse_opt_f64_env_asset(key: &str, asset: &str) -> Option<f64> {
    let ks = env_key_asset(key, asset);
    std::env::var(&ks)
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| std::env::var(key).ok().and_then(|v| v.parse().ok()))
}

fn parse_dec_env_asset(key: &str, asset: &str, default: Decimal) -> Decimal {
    let ks = env_key_asset(key, asset);
    std::env::var(&ks)
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| std::env::var(key).ok().and_then(|v| v.parse().ok()))
        .unwrap_or(default)
}

fn parse_usize_env_asset(key: &str, asset: &str, default: usize) -> usize {
    let ks = env_key_asset(key, asset);
    std::env::var(&ks)
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| std::env::var(key).ok().and_then(|v| v.parse().ok()))
        .unwrap_or(default)
}

fn parse_string_env_asset(key: &str, asset: &str, fallback: String) -> String {
    let ks = env_key_asset(key, asset);
    std::env::var(&ks)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var(key)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or(fallback)
}

fn analyze_rule_flags(reports: &[HorizonReport]) -> RuleFlags {
    let bt_short = reports.iter().find(|r| r.horizon_days == 1);
    let bt_long = reports.iter().find(|r| r.horizon_days == 30);
    let bt_7 = reports.iter().find(|r| r.horizon_days == 7);

    let short_vs_long_mismatch = if let (Some(s), Some(l)) = (bt_short, bt_long) {
        s.backtest.total_return_pct > 3.0 && l.backtest.total_return_pct < -3.0
    } else {
        false
    };

    let drawdown_escalates = if let (Some(a), Some(b)) = (bt_short, bt_7) {
        a.backtest.max_drawdown_pct + 5.0 < b.backtest.max_drawdown_pct
    } else {
        false
    };

    let wf_sharpe_all_neg = {
        let with_oos_trades: Vec<&HorizonReport> = reports
            .iter()
            .filter(|r| {
                r.walk_forward.error.is_none()
                    && r.walk_forward.iterations > 0
                    && r.walk_forward.total_test_trades > 0
            })
            .collect();
        !with_oos_trades.is_empty()
            && with_oos_trades.iter().all(|r| {
                let s = r.walk_forward.avg_test_sharpe;
                s.is_finite() && s < 0.0
            })
    };

    let wf_low_consistency = reports.iter().filter(|r| r.horizon_days >= 14).any(|r| {
        r.walk_forward.error.is_none()
            && r.walk_forward.iterations > 0
            && r.walk_forward.consistency_score < 0.5
    });

    let heavy_trades_bad_pf = if let Some(l) = bt_long {
        l.backtest.trades > 50 && l.backtest.profit_factor < 1.0
    } else {
        false
    };

    RuleFlags {
        short_vs_long_mismatch,
        drawdown_escalates,
        wf_sharpe_all_neg,
        wf_low_consistency,
        heavy_trades_bad_pf,
    }
}

fn build_param_suggestions(flags: &RuleFlags, loaded: &LoadedStrategy) -> Vec<ParamSuggestion> {
    let base = &loaded.backtest;
    let sig = &base.signal_config;

    let mut min_edge = base.min_edge;
    let mut min_conf = base.min_confidence;
    let mut max_pos = base.max_position_pct;

    let base_daily_loss = loaded.daily_loss_limit_pct;
    let mut daily_sugg = base_daily_loss;

    let base_min_order = loaded.min_order_usdc;
    let mut order_sugg = base_min_order;

    let rsi_base = sig.rsi_period;
    let mut rsi_sugg = rsi_base;

    let macd_fast_base = sig.macd_fast;
    let mut macd_fast_sugg = macd_fast_base;

    let macd_slow_base = sig.macd_slow;
    let mut macd_slow_sugg = macd_slow_base;

    let macd_signal_base = sig.macd_signal;

    if flags.drawdown_escalates {
        max_pos = (max_pos - dec!(0.02)).max(dec!(0.02));
        daily_sugg = (daily_sugg - dec!(0.03)).max(dec!(0.05));
    }

    if flags.wf_sharpe_all_neg {
        min_edge = (min_edge + dec!(0.02)).min(dec!(0.15));
        min_conf = (min_conf + dec!(0.03)).min(dec!(0.90));
    }

    if flags.wf_low_consistency {
        min_edge = (min_edge + dec!(0.01)).min(dec!(0.15));
        min_conf = (min_conf + dec!(0.02)).min(dec!(0.90));
        rsi_sugg = (rsi_sugg + 2).min(28);
        macd_slow_sugg = (macd_slow_sugg + 2).min(34);
    }

    if flags.heavy_trades_bad_pf {
        min_edge = (min_edge + dec!(0.02)).min(dec!(0.15));
        order_sugg = (order_sugg + dec!(2)).min(dec!(25));
    }

    if flags.short_vs_long_mismatch {
        min_edge = (min_edge + dec!(0.01)).min(dec!(0.15));
        min_conf = (min_conf + dec!(0.02)).min(dec!(0.90));
    }

    if macd_fast_sugg >= macd_slow_sugg {
        macd_fast_sugg = (macd_slow_sugg.saturating_sub(4)).max(8);
    }

    let candle_lookback = loaded.lookback;
    let candle_interval = loaded.interval.clone();
    let suggested_lookback = if flags.wf_low_consistency || flags.wf_sharpe_all_neg {
        (candle_lookback + 50).min(200)
    } else {
        candle_lookback
    };

    let vf = &base.volatility_filter;
    let vol_stress = flags.wf_low_consistency
        || flags.wf_sharpe_all_neg
        || flags.drawdown_escalates
        || flags.heavy_trades_bad_pf
        || flags.short_vs_long_mismatch;
    let vol_max_sugg = if vol_stress {
        vf.max_std_pct.map(|m| m.min(dec!(0.5))).unwrap_or(dec!(0.5))
    } else {
        vf.max_std_pct.unwrap_or(dec!(0.5))
    };
    let vol_sample_sugg = vf.sample_bars.max(20);

    let a = &loaded.asset;
    vec![
        ParamSuggestion {
            env_var: env_key_asset("MIN_EDGE", a),
            current: fmt_dec(base.min_edge),
            suggested: fmt_dec(min_edge),
            not_tr: "Daha seçici edge eşiği (kural tetiklerine göre artırıldı)".to_string(),
        },
        ParamSuggestion {
            env_var: env_key_asset("MIN_CONFIDENCE", a),
            current: fmt_dec(base.min_confidence),
            suggested: fmt_dec(min_conf),
            not_tr: "Düşük güvenli sinyalleri kesmek için".to_string(),
        },
        ParamSuggestion {
            env_var: env_key_asset("MAX_POSITION_PCT", a),
            current: fmt_dec(base.max_position_pct),
            suggested: fmt_dec(max_pos),
            not_tr: if flags.drawdown_escalates {
                "Drawdown artışı — pozisyon oranını düşür".to_string()
            } else {
                "Risk tavanı".to_string()
            },
        },
        ParamSuggestion {
            env_var: env_key_asset("DAILY_LOSS_LIMIT_PCT", a),
            current: fmt_dec(base_daily_loss),
            suggested: fmt_dec(daily_sugg),
            not_tr: if flags.drawdown_escalates {
                "Günlük zarar limitini sıkılaştır".to_string()
            } else {
                "Mevcut env’den".to_string()
            },
        },
        ParamSuggestion {
            env_var: env_key_asset("MIN_ORDER_USDC", a),
            current: fmt_dec(base_min_order),
            suggested: fmt_dec(order_sugg),
            not_tr: if flags.heavy_trades_bad_pf {
                "Küçük/gürültülü işlemleri azaltmak için taban artırıldı".to_string()
            } else {
                "Minimum emir boyutu".to_string()
            },
        },
        ParamSuggestion {
            env_var: env_key_asset("RSI_PERIOD", a),
            current: rsi_base.to_string(),
            suggested: rsi_sugg.to_string(),
            not_tr: if flags.wf_low_consistency {
                "Daha az gürültü için +2 (tavan 28)".to_string()
            } else {
                "Backtest SignalConfig ile aynı".to_string()
            },
        },
        ParamSuggestion {
            env_var: env_key_asset("MACD_FAST", a),
            current: macd_fast_base.to_string(),
            suggested: macd_fast_sugg.to_string(),
            not_tr: if macd_fast_sugg != macd_fast_base {
                "fast < slow kısıtı için ayarlandı".to_string()
            } else {
                "MACD hızlı".to_string()
            },
        },
        ParamSuggestion {
            env_var: env_key_asset("MACD_SLOW", a),
            current: macd_slow_base.to_string(),
            suggested: macd_slow_sugg.to_string(),
            not_tr: if flags.wf_low_consistency {
                "Rejim stabilitesi için yavaş çizgi +2 (tavan 34)".to_string()
            } else {
                "MACD yavaş".to_string()
            },
        },
        ParamSuggestion {
            env_var: env_key_asset("MACD_SIGNAL", a),
            current: macd_signal_base.to_string(),
            suggested: macd_signal_base.to_string(),
            not_tr: "Sinyal hattı".to_string(),
        },
        ParamSuggestion {
            env_var: env_key_asset("CANDLE_INTERVAL", a),
            current: candle_interval.clone(),
            suggested: candle_interval,
            not_tr: "Analiz komutuyla aynı interval".to_string(),
        },
        ParamSuggestion {
            env_var: env_key_asset("CANDLE_LOOKBACK", a),
            current: candle_lookback.to_string(),
            suggested: suggested_lookback.to_string(),
            not_tr: if suggested_lookback > candle_lookback {
                "WF zayıfsa daha uzun geçmiş".to_string()
            } else {
                "Analiz lookback ile aynı".to_string()
            },
        },
        ParamSuggestion {
            env_var: env_key_asset("VOLUME_MIN_RATIO", a),
            current: fmt_opt_f64(sig.volume_min_ratio),
            suggested: fmt_opt_f64(sig.volume_min_ratio),
            not_tr: "Spot hacim kalitesi (son/ort.); — = veto yok — düşük hacimde sinyal üretilmez".to_string(),
        },
        ParamSuggestion {
            env_var: env_key_asset("VOLUME_AVG_BARS", a),
            current: sig.volume_avg_bars.to_string(),
            suggested: sig.volume_avg_bars.max(20).to_string(),
            not_tr: "Hacim ortalaması penceresi (RSI+küme/MACD sinyali)".to_string(),
        },
        ParamSuggestion {
            env_var: env_key_asset("VOL_MAX_STD_PCT", a),
            current: fmt_opt_dec(vf.max_std_pct),
            suggested: fmt_dec(vol_max_sugg),
            not_tr: if vol_stress {
                "Getiri std×100 üst sınırı — stres bayraklarında tavan 0.5’e sıkılaştırılabilir".to_string()
            } else {
                "getiri std×100; 0.05 gibi değerler 5m’de neredeyse her şeyi keser — 0.15–0.45 deneyin".to_string()
            },
        },
        ParamSuggestion {
            env_var: env_key_asset("VOL_SAMPLE_BARS", a),
            current: vf.sample_bars.to_string(),
            suggested: vol_sample_sugg.to_string(),
            not_tr: "Volatilite ölçümü için mum sayısı (min 20 önerilir)".to_string(),
        },
    ]
}

fn fmt_dec(d: Decimal) -> String {
    d.normalize().to_string()
}

fn suggest_changes_text(flags: &RuleFlags) -> Vec<String> {
    let mut out = vec!["=== Öneriler (kural tabanlı; canlı öncesi doğrulayın) ===".to_string()];
    out.push(
        "• Sinyal: RSI + 5m/15m momentum tek küme oyu; MACD çizgi yönü ayrı; çelişkide MACD tie-break. Düşük hacimde VOLUME_MIN_RATIO ile işlem yok."
            .to_string(),
    );

    if flags.short_vs_long_mismatch {
        out.push(
            "• Kısa (1g) ile uzun (30g) getiri uyumsuz — şans/rejim riski. MIN_EDGE / MIN_CONFIDENCE artırın veya WF OOS’a güvenin."
                .to_string(),
        );
    }

    if flags.drawdown_escalates {
        out.push(
            "• Ufuk uzadıkça MDD artıyor — MAX_POSITION_PCT ve DAILY_LOSS_LIMIT_PCT ile riski kısın."
                .to_string(),
        );
    }

    if flags.wf_sharpe_all_neg {
        out.push(
            "• Tüm ufukta WF test Sharpe negatif — sinyal eşikleri ve/veya RSI/MACD periyotlarını gözden geçirin."
                .to_string(),
        );
    }

    if flags.wf_low_consistency {
        out.push(
            "• 14g+ WF consistency düşük — daha muhafazakâr eşikler veya farklı CANDLE_* deneyin."
                .to_string(),
        );
    }

    if flags.heavy_trades_bad_pf {
        out.push(
            "• 30g’de çok işlem + PF < 1 — MIN_EDGE / MIN_ORDER_USDC ile zayıf edge’i kesin."
                .to_string(),
        );
    }

    if out.len() == 2 {
        // Başlık + sabit sinyal satırı dışında bayrak yoksa
        out.push(
            "• Net kırmızı bayrak yok; yine de uzun ufuk getiri + WF metriklerini birlikte okuyun."
                .to_string(),
        );
    }

    out
}

fn print_param_suggestions_table(rows: &[ParamSuggestion]) {
    println!("=== Önerilen .env değerleri (mevcut analiz tabanına göre) ===");
    println!(
        "{:<22} {:>12} {:>12}  {}",
        "değişken", "şimdiki", "önerilen", "not"
    );
    for r in rows {
        println!(
            "{:<22} {:>12} {:>12}  {}",
            r.env_var, r.current, r.suggested, r.not_tr
        );
    }
}

fn tail_slice<'a>(candles: &'a [Candle], want: usize) -> &'a [Candle] {
    if candles.len() <= want {
        candles
    } else {
        &candles[candles.len() - want..]
    }
}

/// Approximate candles per calendar day for common Binance intervals.
fn candles_per_day(interval: &str) -> usize {
    match interval {
        "1m" => 60 * 24,
        "3m" => 60 * 24 / 3,
        "5m" => 60 * 24 / 5,
        "15m" => 60 * 24 / 15,
        "30m" => 60 * 24 / 30,
        "1h" | "60m" => 24,
        "4h" => 6,
        "1d" | "1D" => 1,
        _ => 60 * 24,
    }
}

fn candles_for_days(days: u32, interval: &str) -> usize {
    days as usize * candles_per_day(interval)
}

fn summarize_bt(r: &BacktestResult) -> BtSummary {
    let m = &r.metrics;
    let bars_with_signal = r.bars_with_signal;
    let vol_filter_skips = r.volatility_filter_skips;
    let vol_filter_skip_pct_of_signals = if bars_with_signal > 0 {
        100.0 * vol_filter_skips as f64 / bars_with_signal as f64
    } else {
        0.0
    };
    let vol_low = r.volume_low_skips;
    let no_clear = r.no_clear_signal_skips;
    let attempts = vol_low + no_clear + bars_with_signal;
    let volume_low_pct_of_signal_attempts = if attempts > 0 {
        100.0 * vol_low as f64 / attempts as f64
    } else {
        0.0
    };
    BtSummary {
        trades: m.total_trades,
        total_return_pct: m.total_return_pct,
        sharpe_ratio: m.sharpe_ratio,
        max_drawdown_pct: m.max_drawdown_pct,
        win_rate: m.win_rate,
        profit_factor: m.profit_factor,
        bars_with_signal,
        vol_filter_skips,
        vol_filter_skip_pct_of_signals,
        volume_low_skips: vol_low,
        no_clear_signal_skips: no_clear,
        volume_low_pct_of_signal_attempts,
    }
}

fn run_wf_or_err(
    candles: &[Candle],
    asset: &str,
    total: usize,
    days: u32,
    loaded: &LoadedStrategy,
    holding_period: usize,
) -> (WfSummary, Option<String>) {
    let cfg = match walk_forward_config_for(total, loaded, holding_period) {
        Ok(c) => c,
        Err(e) => {
            return (
                WfSummary {
                    iterations: 0,
                    total_test_trades: 0,
                    avg_test_sharpe: f64::NAN,
                    avg_test_return_pct: f64::NAN,
                    cumulative_pnl: "—".to_string(),
                    consistency_score: f64::NAN,
                    error: Some(e.to_string()),
                },
                None,
            );
        }
    };

    let meta = Some(format!(
        "train={} test={} step={} | min_edge={} min_conf={} hold={}",
        cfg.train_window,
        cfg.test_window,
        cfg.step_size,
        cfg.min_edge,
        cfg.min_confidence,
        cfg.holding_period
    ));

    match run_walk_forward(candles, asset, &cfg) {
        Ok(r) => (summarize_wf(&r), meta),
        Err(e) => (
            WfSummary {
                iterations: 0,
                total_test_trades: 0,
                avg_test_sharpe: f64::NAN,
                avg_test_return_pct: f64::NAN,
                cumulative_pnl: "—".to_string(),
                consistency_score: f64::NAN,
                error: Some(format!("{}d: {}", days, e)),
            },
            meta,
        ),
    }
}

fn summarize_wf(r: &WalkForwardResult) -> WfSummary {
    let a = &r.aggregate_metrics;
    WfSummary {
        iterations: a.total_iterations,
        total_test_trades: a.total_test_trades,
        avg_test_sharpe: a.avg_test_sharpe,
        avg_test_return_pct: a.avg_test_return,
        cumulative_pnl: a.cumulative_pnl.to_string(),
        consistency_score: a.consistency_score,
        error: None,
    }
}

/// Train/test/step sized from total length so short horizons still get at least one WF iteration.
/// Risk/eşik alanları `loaded` (.env) ile backtest ile aynı hizalanır.
fn walk_forward_config_for(
    total: usize,
    loaded: &LoadedStrategy,
    holding_period: usize,
) -> Result<WalkForwardConfig> {
    if total < 800 {
        anyhow::bail!("need at least ~800 candles for walk-forward (got {})", total);
    }

    let train = ((total as f64 * 0.42).round() as usize).clamp(250, 3500);
    let test = ((total as f64 * 0.28).round() as usize).clamp(150, 2500);
    let step = ((total as f64 * 0.08).round() as usize).clamp(50, 500);

    let (train, test) = if train + test > total.saturating_sub(10) {
        let t = total * 48 / 100;
        let e = total * 32 / 100;
        (t.max(200), e.max(120))
    } else {
        (train, test)
    };

    if train + test > total {
        anyhow::bail!("train+test exceeds series length");
    }

    let b = &loaded.backtest;
    let s = &b.signal_config;
    Ok(WalkForwardConfig {
        train_window: train,
        test_window: test,
        step_size: step,
        min_edge: b.min_edge,
        min_confidence: b.min_confidence,
        max_position_pct: b.max_position_pct,
        initial_balance: b.initial_balance,
        holding_period,
        volatility_filter: b.volatility_filter.clone(),
        volume_min_ratio: s.volume_min_ratio,
        volume_avg_bars: s.volume_avg_bars.max(5),
    })
}

fn print_human_table(reports: &[HorizonReport], cli: &Cli, loaded: &LoadedStrategy) {
    println!(
        "=== Multi-horizon analysis: {} {} (lookback={} holding={}) ===",
        cli.asset.to_uppercase(),
        loaded.interval,
        loaded.lookback,
        cli.holding_period
    );
    println!(
        "Env çözümlemesi: --asset {} → örn. MIN_EDGE_{}, CANDLE_LOOKBACK_{} (yoksa global MIN_EDGE, CANDLE_LOOKBACK)",
        loaded.asset,
        loaded.asset.to_uppercase(),
        loaded.asset.to_uppercase()
    );
    println!(
        "Strateji (önce .env yüklenir; env > CLI varsayılanı): dotenv_loaded={} | min_edge={} min_confidence={} max_position_pct={} initial_balance={}",
        loaded.dotenv_loaded,
        loaded.backtest.min_edge,
        loaded.backtest.min_confidence,
        loaded.backtest.max_position_pct,
        loaded.backtest.initial_balance
    );
    println!(
        "MIN_ORDER_USDC={} DAILY_LOSS_LIMIT_PCT={}",
        loaded.min_order_usdc,
        loaded.daily_loss_limit_pct
    );
    if !loaded.dotenv_loaded {
        println!("Uyarı: `.env` bulunamadı veya okunamadı — yalnızca shell ortamı ve CLI varsayılanları kullanıldı. Projeyi kök dizinden çalıştırın.");
    }
    if loaded.backtest.volatility_filter.max_std_pct.is_some()
        && reports.iter().any(|r| r.backtest.vol_filter_skip_pct_of_signals > 85.0)
    {
        println!(
            "Uyarı: VOL_MAX_STD_PCT çok sıkı — sinyallerin >%85’i vol filtresinde kesiliyor. \
             Değer getiri std×100 ölçeğindedir; 0.05 tipik 5m BTC’de neredeyse tüm barları eler. \
             VOL_MAX_STD_PCT’yi yükseltin (ör. 0.2–0.4) veya geçici olarak kaldırıp analyze ile tekrar ölçün."
        );
    }
    println!();

    for r in reports {
        println!(
            "--- {} gün (~{} mum) ---",
            r.horizon_days, r.candles_used
        );
        if let Some(ref w) = r.wf_windows {
            println!("  WF pencereleri: {}", w);
        }
        let b = &r.backtest;
        println!(
            "  Backtest: return {:+.2}% | Sharpe {:.3} | MDD {:.2}% | trades {} | WR {:.1}% | PF {:.2}",
            b.total_return_pct,
            b.sharpe_ratio,
            b.max_drawdown_pct,
            b.trades,
            b.win_rate * 100.0,
            b.profit_factor
        );
        println!(
            "            vol filtresi: sinyal bar {} | vol ile kesilen {} ({:.2}% sinyaller)",
            b.bars_with_signal,
            b.vol_filter_skips,
            b.vol_filter_skip_pct_of_signals
        );
        println!(
            "            hacim/sinyal: düşük hacim skip {} | net sinyal yok {} | hacim skip % deneme {:.1}%",
            b.volume_low_skips,
            b.no_clear_signal_skips,
            b.volume_low_pct_of_signal_attempts
        );
        let w = &r.walk_forward;
        if let Some(ref e) = w.error {
            println!("  Walk-forward: (atlandı) {}", e);
        } else {
            println!(
                "  Walk-fwd:  iter {} | avg test Sharpe {:.3} | avg test ret {:+.2}% | cum PnL {} | consistency {:.2}",
                w.iterations,
                w.avg_test_sharpe,
                w.avg_test_return_pct,
                w.cumulative_pnl,
                w.consistency_score
            );
        }
        println!();
    }
}

async fn fetch_candles(
    exchange: &str,
    asset: &str,
    interval: &str,
    limit: usize,
) -> Result<Vec<Candle>> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("HTTP client build failed")?;

    let client = SpotPriceClient::new(http, exchange.to_string());
    client
        .fetch_candles(asset, interval, limit)
        .await
        .with_context(|| format!("fetch_candles {exchange} {asset} {interval}"))
}
