//! Optional `config.toml` schema (strategy / TA / risk). Secrets stay in `.env` only.
//!
//! Precedence when building [`crate::config::AppConfig`]: **environment > TOML > code defaults**.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Root table for `config.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct TomlRoot {
    pub strategy: Option<StrategySection>,
    pub technical: Option<TechnicalSection>,
    pub cluster: Option<ClusterSection>,
    pub volatility: Option<VolatilitySection>,
    pub risk: Option<RiskSection>,
    pub htf: Option<HtfSection>,
    pub adaptive: Option<AdaptiveSection>,
    pub execution: Option<ExecutionSection>,
    /// Per-asset overrides: `[asset.btc]`, `[asset.eth]`, …
    pub asset: Option<HashMap<String, AssetOverride>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct StrategySection {
    pub assets: Option<Vec<String>>,
    pub durations: Option<Vec<String>>,
    pub gamma_tag_id: Option<u64>,
    pub min_edge: Option<String>,
    pub min_confidence: Option<String>,
    pub min_order_usdc: Option<String>,
    /// Fraction added to token price for slippage (e.g. `0.002` = 20 bps). Mirrors env `SLIPPAGE_BPS`.
    pub slippage_bps: Option<String>,
    /// Default block for all assets unless overridden per `[asset.*]` (`YES` or `NO`).
    pub blocked_direction: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct TechnicalSection {
    pub spot_exchange: Option<String>,
    pub candle_interval: Option<String>,
    pub candle_lookback: Option<usize>,
    pub rsi_period: Option<usize>,
    pub macd_fast: Option<usize>,
    pub macd_slow: Option<usize>,
    pub macd_signal: Option<usize>,
    pub volume_min_ratio: Option<f64>,
    pub volume_avg_bars: Option<usize>,
    pub volume_use_closed_candle_only: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ClusterSection {
    pub rsi_oversold: Option<f64>,
    pub rsi_overbought: Option<f64>,
    pub mom5_abs: Option<f64>,
    pub mom15_abs: Option<f64>,
    pub min_market_yes_price: Option<String>,
    pub max_market_yes_price: Option<String>,
    pub min_secs_to_close: Option<i64>,
    /// Skip when remaining time exceeds this (too far from expiry). `None` = off.
    pub max_secs_to_close: Option<i64>,
    pub expiry_dampen_last_secs: Option<i64>,
    pub cheap_token_price_threshold: Option<String>,
    pub cheap_token_max_usdc: Option<String>,
    pub large_order_usdc_hard_cap: Option<String>,
    /// When cluster vote is TIE, multiply effective `min_edge` by this (default 1.0 = no change).
    pub cluster_tie_min_edge_multiplier: Option<f64>,
    /// Skip when `|momentum_5m|` is below this (fractional return). `0` = off.
    pub min_momentum_5m_abs: Option<f64>,
    /// When `taker_buy_ratio` is in `[0.45, 0.55]`, multiply effective min edge by this.
    pub neutral_taker_edge_multiplier: Option<f64>,
    /// Skip BUY YES when RSI exceeds this (overbought chase). `0` = off.
    pub rsi_yes_max: Option<f64>,
    /// Skip BUY NO when RSI is below this (oversold fade). `0` = off.
    pub rsi_no_min: Option<f64>,
    /// Skip when `|MACD histogram|` is below this (weak signal). `0` = off.
    pub min_macd_histogram_abs: Option<f64>,
    /// Require taker flow aligned with trade direction (YES needs TBR>0.55, NO needs TBR<0.45).
    pub taker_direction_confirm: Option<bool>,
    /// Subtract from confidence before threshold when trading YES (soft veto).
    pub yes_confidence_penalty: Option<f64>,
    /// Subtract from confidence before threshold when trading NO (soft veto).
    pub no_confidence_penalty: Option<f64>,
    pub dynamic_momentum_threshold: Option<bool>,
    pub momentum_vol_reference: Option<f64>,
    pub adaptive_direction_penalty: Option<bool>,
    pub adaptive_penalty_window: Option<usize>,
    pub multi_tf_enabled: Option<bool>,
    pub multi_tf_interval: Option<String>,
    pub multi_tf_lookback: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct VolatilitySection {
    pub min_std_pct: Option<String>,
    pub max_std_pct: Option<String>,
    pub sample_bars: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct RiskSection {
    pub max_position_pct: Option<String>,
    pub daily_loss_limit_pct: Option<String>,
    pub initial_balance: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct HtfSection {
    pub enabled: Option<bool>,
    pub interval: Option<String>,
    pub lookback: Option<usize>,
    pub ema_period: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct AdaptiveSection {
    pub enabled: Option<bool>,
    pub trade_window: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ExecutionSection {
    pub dry_run: Option<bool>,
    pub cycle_secs: Option<u64>,
    pub clob_host: Option<String>,
    pub signature_type: Option<String>,
    pub data_dir: Option<String>,
    /// Cancel resting GTD orders after this many seconds if still unmatched.
    pub fill_timeout_secs: Option<u64>,
    /// Wait this long after placement before first REST `poll_order` (avoid hammering API).
    pub poll_min_order_age_secs: Option<u64>,
}

/// Fields mirror per-asset env keys (`MIN_EDGE_BTC`, …) without the `_ASSET` suffix.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct AssetOverride {
    pub min_edge: Option<String>,
    pub min_confidence: Option<String>,
    pub min_order_usdc: Option<String>,
    pub spot_exchange: Option<String>,
    pub candle_interval: Option<String>,
    pub candle_lookback: Option<usize>,
    pub rsi_period: Option<usize>,
    pub macd_fast: Option<usize>,
    pub macd_slow: Option<usize>,
    pub macd_signal: Option<usize>,
    pub volume_min_ratio: Option<f64>,
    pub volume_avg_bars: Option<usize>,
    pub max_position_pct: Option<String>,
    pub daily_loss_limit_pct: Option<String>,
    pub vol_min_std_pct: Option<String>,
    pub vol_max_std_pct: Option<String>,
    pub vol_sample_bars: Option<usize>,
    pub htf_enabled: Option<bool>,
    pub htf_interval: Option<String>,
    pub htf_lookback: Option<usize>,
    pub htf_ema_period: Option<usize>,
    pub adaptive_thresholds: Option<bool>,
    pub adaptive_trade_window: Option<usize>,
    pub min_secs_to_close: Option<i64>,
    pub max_secs_to_close: Option<i64>,
    pub expiry_dampen_last_secs: Option<i64>,
    pub min_market_yes_price: Option<String>,
    pub max_market_yes_price: Option<String>,
    pub cheap_token_price_threshold: Option<String>,
    pub cheap_token_max_usdc: Option<String>,
    pub large_order_usdc_hard_cap: Option<String>,
    pub volume_use_closed_candle_only: Option<bool>,
    pub cluster_rsi_oversold: Option<f64>,
    pub cluster_rsi_overbought: Option<f64>,
    pub cluster_mom5_abs: Option<f64>,
    pub cluster_mom15_abs: Option<f64>,
    pub cluster_tie_min_edge_multiplier: Option<f64>,
    pub slippage_bps: Option<String>,
    /// Block this Polymarket side for the asset (`YES` or `NO`).
    pub blocked_direction: Option<String>,
    pub min_momentum_5m_abs: Option<f64>,
    pub neutral_taker_edge_multiplier: Option<f64>,
    pub rsi_yes_max: Option<f64>,
    pub rsi_no_min: Option<f64>,
    pub min_macd_histogram_abs: Option<f64>,
    pub taker_direction_confirm: Option<bool>,
    pub yes_confidence_penalty: Option<f64>,
    pub no_confidence_penalty: Option<f64>,
    pub dynamic_momentum_threshold: Option<bool>,
    pub momentum_vol_reference: Option<f64>,
    pub adaptive_direction_penalty: Option<bool>,
    pub adaptive_penalty_window: Option<usize>,
    pub multi_tf_enabled: Option<bool>,
    pub multi_tf_interval: Option<String>,
    pub multi_tf_lookback: Option<usize>,
}

/// Read and parse `CONFIG_PATH` (default `config.toml`). Missing file → `None`.
pub fn load_optional(path_override: Option<&str>) -> Result<Option<TomlRoot>> {
    let path_s = path_override
        .map(|s| s.to_string())
        .or_else(|| std::env::var("CONFIG_PATH").ok())
        .unwrap_or_else(|| "config.toml".to_string());
    let path = Path::new(&path_s);
    if !path.exists() {
        return Ok(None);
    }
    let s = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let root: TomlRoot =
        toml::from_str(&s).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_asset_tables() {
        let s = r#"
[strategy]
assets = ["btc"]
durations = ["15m"]
min_edge = "0.10"

[asset.btc]
min_edge = "0.12"
vol_min_std_pct = "0.03"
"#;
        let r: TomlRoot = toml::from_str(s).expect("toml");
        assert_eq!(
            r.strategy.as_ref().unwrap().min_edge.as_deref(),
            Some("0.10")
        );
        let btc = r.asset.as_ref().unwrap().get("btc").expect("btc");
        assert_eq!(btc.min_edge.as_deref(), Some("0.12"));
        assert_eq!(btc.vol_min_std_pct.as_deref(), Some("0.03"));
    }

    #[test]
    fn parses_slippage_cluster_tie_and_asset_extras() {
        let s = r#"
[strategy]
slippage_bps = "0.002"
blocked_direction = "NO"

[cluster]
cluster_tie_min_edge_multiplier = 1.3
max_secs_to_close = 700

[asset.btc]
max_secs_to_close = 600
blocked_direction = "YES"
"#;
        let r: TomlRoot = toml::from_str(s).expect("toml");
        assert_eq!(
            r.strategy.as_ref().unwrap().slippage_bps.as_deref(),
            Some("0.002")
        );
        assert_eq!(
            r.strategy.as_ref().unwrap().blocked_direction.as_deref(),
            Some("NO")
        );
        let c = r.cluster.as_ref().unwrap();
        assert_eq!(c.cluster_tie_min_edge_multiplier, Some(1.3));
        assert_eq!(c.max_secs_to_close, Some(700));
        let btc = r.asset.as_ref().unwrap().get("btc").expect("btc");
        assert_eq!(btc.max_secs_to_close, Some(600));
        assert_eq!(btc.blocked_direction.as_deref(), Some("YES"));
    }

    #[test]
    fn parses_momentum_and_neutral_taker_cluster_fields() {
        let s = r#"
[cluster]
min_momentum_5m_abs = 0.0008
neutral_taker_edge_multiplier = 1.5
"#;
        let r: TomlRoot = toml::from_str(s).expect("toml");
        let c = r.cluster.as_ref().unwrap();
        assert_eq!(c.min_momentum_5m_abs, Some(0.0008));
        assert_eq!(c.neutral_taker_edge_multiplier, Some(1.5));
    }
}
