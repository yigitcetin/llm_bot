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
    pub expiry_dampen_last_secs: Option<i64>,
    pub cheap_token_price_threshold: Option<String>,
    pub cheap_token_max_usdc: Option<String>,
    pub large_order_usdc_hard_cap: Option<String>,
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
}
