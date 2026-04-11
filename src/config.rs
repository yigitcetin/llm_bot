use std::sync::Arc;

use anyhow::{Context, Result};
use polymarket_client_sdk::{POLYGON, PRIVATE_KEY_VAR};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::config_toml::{AssetOverride, TomlRoot};
use crate::constants::{GAMMA_TAG_ID_DEFAULT, SLIPPAGE_BPS};
use crate::signals::SignalConfig;
use crate::types::Direction;
use crate::volatility::VolatilityFilterConfig;

#[derive(Debug, Clone)]
pub struct AppConfig {
    // Auth
    pub polymarket_private_key: String,

    // Strategy
    pub assets: Vec<String>,
    pub durations: Vec<String>,

    // Edge / signal thresholds
    pub min_edge: Decimal,       // minimum technical prob vs market price gap
    pub min_confidence: Decimal, // minimum technical confidence score
    pub min_order_usdc: Decimal, // minimum order size in USDC
    /// Slippage fraction added to reference token price (e.g. `0.002` = 0.2%). Also used as CLOB worst-price limit.
    pub slippage_bps: Decimal,

    // Technical Analysis
    pub spot_exchange: String,   // "binance", "coinbase", etc.
    pub candle_interval: String, // "1m", "5m"
    pub candle_lookback: usize,  // how many candles to analyze
    pub rsi_period: usize,       // RSI calculation period
    pub macd_fast: usize,        // MACD fast EMA
    pub macd_slow: usize,        // MACD slow EMA
    pub macd_signal: usize,      // MACD signal line
    /// Spot hacim kalitesi: son mum / ortalama altındaysa sinyal yok (`None` = veto kapalı).
    pub volume_min_ratio: Option<f64>,
    pub volume_avg_bars: usize, // hacim ortalaması için mum sayısı

    // Risk
    pub max_position_pct: Decimal, // max % of balance per trade
    pub daily_loss_limit_pct: Decimal,
    pub initial_balance: Decimal,

    // Execution
    pub dry_run: bool,
    pub cycle_secs: u64,
    /// Cancel unfilled GTD orders after this many seconds.
    pub fill_timeout_secs: u64,
    /// Minimum order age before first REST poll (seconds).
    pub poll_min_order_age_secs: u64,

    // Polymarket
    /// Gamma API `tag_id` filter for listing events (see Polymarket tags).
    pub gamma_tag_id: u64,
    pub clob_host: String,
    pub chain_id: u64,
    pub signature_type: SignatureType,
    pub funder_address: Option<String>, // Required for Proxy/GnosisSafe, must be None for EOA

    // Builder API (optional - for market makers)
    pub builder_api_key: Option<String>,
    pub builder_api_secret: Option<String>,
    pub builder_api_passphrase: Option<String>,

    /// Data directory for `trades.jsonl`, skips, order failures (`data/` by default).
    pub data_dir: String,

    /// Higher-timeframe trend filter (e.g. 15m EMA vs close).
    pub htf_enabled: bool,
    pub htf_interval: String,
    pub htf_lookback: usize,
    pub htf_ema_period: usize,

    /// Nudge `min_edge` / `min_confidence` from recent resolved trades.
    pub adaptive_thresholds: bool,
    pub adaptive_trade_window: usize,

    /// Skip markets that resolve sooner than this (seconds). `None` = off.
    pub min_secs_to_close: Option<i64>,
    /// Skip when remaining time exceeds this (seconds; e.g. avoid early-window noise). `None` = off.
    pub max_secs_to_close: Option<i64>,
    /// Blend probability toward 0.5 in the last N seconds of the window. `None` = off.
    pub expiry_dampen_last_secs: Option<i64>,
    /// Reject if Gamma YES mid &lt; this (illiquid / mispriced tail). `None` = off.
    pub min_market_yes_price: Option<Decimal>,
    /// Reject if Gamma YES mid &gt; this. `None` = off.
    pub max_market_yes_price: Option<Decimal>,
    /// Token price below this → position USDC capped (see `cheap_token_max_usdc`).
    pub cheap_token_price_threshold: Decimal,
    pub cheap_token_max_usdc: Decimal,
    /// Hard cap on any single order (USDC). `None` = off.
    pub large_order_usdc_hard_cap: Option<Decimal>,
    /// Use prior closed candle for volume ratio (avoids partial-bar low volume).
    pub volume_use_closed_candle_only: bool,
    /// RSI cluster thresholds (plan P2).
    pub cluster_rsi_oversold: f64,
    pub cluster_rsi_overbought: f64,
    pub cluster_mom5_abs: f64,
    pub cluster_mom15_abs: f64,
    /// When cluster vote is TIE, multiply effective min edge by this before trading.
    pub cluster_tie_min_edge_multiplier: f64,
    /// Skip when `|momentum_5m|` &lt; this (fractional). `0` = filter off.
    pub min_momentum_5m_abs: f64,
    /// When `taker_buy_ratio` in `[0.45, 0.55]`, multiply effective min edge by this.
    pub neutral_taker_edge_multiplier: f64,
    /// Skip BUY YES when RSI exceeds this (`0` = off). Default 70.
    pub rsi_yes_max: f64,
    /// Skip BUY NO when RSI is below this (`0` = off). Default 30.
    pub rsi_no_min: f64,

    /// Parsed `config.toml` for per-asset TOML fallbacks (environment still wins).
    pub(crate) toml: Option<Arc<TomlRoot>>,
}

/// Polymarket signature types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureType {
    /// EOA (Externally Owned Account) - Standard wallet signature
    Eoa,
    /// Proxy contract signature
    Proxy,
    /// Gnosis Safe multisig signature
    GnosisSafe,
}

/// Strategy parameters resolved for one asset (`ASSETS` entry).
///
/// Env: global `MIN_EDGE`, … plus overrides `MIN_EDGE_BTC`, `RSI_PERIOD_ETH`, …
/// (suffix `_` + asset uppercased, e.g. `btc` → `MIN_EDGE_BTC`).
#[derive(Debug, Clone)]
pub struct AssetStrategy {
    pub min_edge: Decimal,
    pub min_confidence: Decimal,
    pub min_order_usdc: Decimal,
    pub spot_exchange: String,
    pub candle_interval: String,
    pub candle_lookback: usize,
    pub rsi_period: usize,
    pub macd_fast: usize,
    pub macd_slow: usize,
    pub macd_signal: usize,
    pub volume_min_ratio: Option<f64>,
    pub volume_avg_bars: usize,
    pub max_position_pct: Decimal,
    pub daily_loss_limit_pct: Decimal,
    pub volatility_filter: VolatilityFilterConfig,

    pub htf_enabled: bool,
    pub htf_interval: String,
    pub htf_lookback: usize,
    pub htf_ema_period: usize,

    pub adaptive_thresholds: bool,
    pub adaptive_trade_window: usize,

    pub min_secs_to_close: Option<i64>,
    pub expiry_dampen_last_secs: Option<i64>,
    pub min_market_yes_price: Option<Decimal>,
    pub max_market_yes_price: Option<Decimal>,
    pub cheap_token_price_threshold: Decimal,
    pub cheap_token_max_usdc: Decimal,
    pub large_order_usdc_hard_cap: Option<Decimal>,
    pub volume_use_closed_candle_only: bool,
    pub cluster_rsi_oversold: f64,
    pub cluster_rsi_overbought: f64,
    pub cluster_mom5_abs: f64,
    pub cluster_mom15_abs: f64,
    pub cluster_tie_min_edge_multiplier: f64,
    /// Skip when `|momentum_5m|` &lt; this (fractional). `0` = filter off.
    pub min_momentum_5m_abs: f64,
    /// When `taker_buy_ratio` in `[0.45, 0.55]`, multiply effective min edge by this.
    pub neutral_taker_edge_multiplier: f64,
    /// Skip BUY YES when RSI exceeds this (`0` = off).
    pub rsi_yes_max: f64,
    /// Skip BUY NO when RSI is below this (`0` = off).
    pub rsi_no_min: f64,
    /// Slippage fraction for edge sizing and order worst-price limit.
    pub slippage_bps: Decimal,
    pub max_secs_to_close: Option<i64>,
    /// Never take this side for this asset (`None` = allow both).
    pub blocked_direction: Option<Direction>,
}

impl AssetStrategy {
    pub fn signal_config(&self) -> SignalConfig {
        SignalConfig {
            rsi_period: self.rsi_period,
            macd_fast: self.macd_fast,
            macd_slow: self.macd_slow,
            macd_signal: self.macd_signal,
            volume_min_ratio: self.volume_min_ratio,
            volume_avg_bars: self.volume_avg_bars.max(5),
            volume_use_closed_candle_only: self.volume_use_closed_candle_only,
            cluster_rsi_oversold: self.cluster_rsi_oversold,
            cluster_rsi_overbought: self.cluster_rsi_overbought,
            cluster_mom5_abs: self.cluster_mom5_abs,
            cluster_mom15_abs: self.cluster_mom15_abs,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.min_edge <= Decimal::ZERO {
            anyhow::bail!("MIN_EDGE_* must be positive, got: {}", self.min_edge);
        }
        if self.min_edge > dec!(0.50) {
            anyhow::bail!("MIN_EDGE_* too high (>50%): {}", self.min_edge);
        }
        if self.min_confidence < dec!(0.5) || self.min_confidence > dec!(1.0) {
            anyhow::bail!(
                "MIN_CONFIDENCE_* must be between 0.5 and 1.0, got: {}",
                self.min_confidence
            );
        }
        if self.max_position_pct <= Decimal::ZERO || self.max_position_pct > dec!(0.5) {
            anyhow::bail!("MAX_POSITION_PCT_* invalid: {}", self.max_position_pct);
        }
        if self.daily_loss_limit_pct <= Decimal::ZERO || self.daily_loss_limit_pct > dec!(1.0) {
            anyhow::bail!(
                "DAILY_LOSS_LIMIT_PCT_* must be in (0, 1], got: {}",
                self.daily_loss_limit_pct
            );
        }
        if self.min_order_usdc < dec!(1) {
            anyhow::bail!("MIN_ORDER_USDC_* too low: {}", self.min_order_usdc);
        }
        if self.rsi_period < 5 || self.rsi_period > 50 {
            anyhow::bail!(
                "RSI_PERIOD_* must be between 5 and 50, got: {}",
                self.rsi_period
            );
        }
        if self.macd_fast >= self.macd_slow {
            anyhow::bail!(
                "MACD_FAST_* ({}) must be less than MACD_SLOW_* ({})",
                self.macd_fast,
                self.macd_slow
            );
        }
        if self.candle_lookback < 50 {
            anyhow::bail!(
                "CANDLE_LOOKBACK_* too low (min 50), got: {}",
                self.candle_lookback
            );
        }
        if let Some(r) = self.volume_min_ratio {
            if r <= 0.0 || r > 5.0 {
                anyhow::bail!("VOLUME_MIN_RATIO_* must be in (0, 5], got: {}", r);
            }
        }
        if self.volume_avg_bars < 5 || self.volume_avg_bars > 200 {
            anyhow::bail!(
                "VOLUME_AVG_BARS_* must be between 5 and 200, got: {}",
                self.volume_avg_bars
            );
        }
        if self.cluster_rsi_oversold <= 0.0 || self.cluster_rsi_oversold >= 50.0 {
            anyhow::bail!(
                "CLUSTER_RSI_OVERSOLD_* should be in (0, 50), got: {}",
                self.cluster_rsi_oversold
            );
        }
        if self.cluster_rsi_overbought <= 50.0 || self.cluster_rsi_overbought >= 100.0 {
            anyhow::bail!(
                "CLUSTER_RSI_OVERBOUGHT_* should be in (50, 100), got: {}",
                self.cluster_rsi_overbought
            );
        }
        if self.cluster_rsi_oversold >= self.cluster_rsi_overbought {
            anyhow::bail!(
                "CLUSTER_RSI_OVERSOLD_* ({}) must be < CLUSTER_RSI_OVERBOUGHT_* ({})",
                self.cluster_rsi_oversold,
                self.cluster_rsi_overbought
            );
        }
        if self.cluster_mom5_abs <= 0.0 || self.cluster_mom5_abs > 0.2 {
            anyhow::bail!(
                "CLUSTER_MOM5_ABS_* out of range (0, 0.2], got: {}",
                self.cluster_mom5_abs
            );
        }
        if self.cluster_mom15_abs <= 0.0 || self.cluster_mom15_abs > 0.2 {
            anyhow::bail!(
                "CLUSTER_MOM15_ABS_* out of range (0, 0.2], got: {}",
                self.cluster_mom15_abs
            );
        }
        if self.cluster_tie_min_edge_multiplier < 1.0 || self.cluster_tie_min_edge_multiplier > 5.0
        {
            anyhow::bail!(
                "CLUSTER_TIE_MIN_EDGE_MULTIPLIER_* must be in [1.0, 5.0], got: {}",
                self.cluster_tie_min_edge_multiplier
            );
        }
        if self.min_momentum_5m_abs < 0.0 || self.min_momentum_5m_abs > 0.05 {
            anyhow::bail!(
                "MIN_MOMENTUM_5M_ABS_* must be in [0.0, 0.05] (0 = off), got: {}",
                self.min_momentum_5m_abs
            );
        }
        if self.neutral_taker_edge_multiplier < 1.0 || self.neutral_taker_edge_multiplier > 5.0 {
            anyhow::bail!(
                "NEUTRAL_TAKER_EDGE_MULTIPLIER_* must be in [1.0, 5.0], got: {}",
                self.neutral_taker_edge_multiplier
            );
        }
        if self.rsi_yes_max < 0.0 || self.rsi_yes_max > 100.0 {
            anyhow::bail!(
                "RSI_YES_MAX_* must be in [0, 100] (0 = off), got: {}",
                self.rsi_yes_max
            );
        }
        if self.rsi_no_min < 0.0 || self.rsi_no_min > 100.0 {
            anyhow::bail!(
                "RSI_NO_MIN_* must be in [0, 100] (0 = off), got: {}",
                self.rsi_no_min
            );
        }
        if self.rsi_yes_max > 0.0 && self.rsi_no_min > 0.0 && self.rsi_no_min >= self.rsi_yes_max {
            anyhow::bail!(
                "RSI_NO_MIN_* ({}) must be < RSI_YES_MAX_* ({}) when both are enabled",
                self.rsi_no_min,
                self.rsi_yes_max
            );
        }
        if self.slippage_bps <= Decimal::ZERO || self.slippage_bps > dec!(0.05) {
            anyhow::bail!(
                "SLIPPAGE_BPS_* must be in (0, 0.05], got: {}",
                self.slippage_bps
            );
        }
        if let (Some(lo), Some(hi)) = (self.min_secs_to_close, self.max_secs_to_close) {
            if hi <= lo {
                anyhow::bail!(
                    "MAX_SECS_TO_CLOSE_* ({}) must be > MIN_SECS_TO_CLOSE_* ({}) when both are set",
                    hi,
                    lo
                );
            }
        }
        if self.cheap_token_price_threshold <= Decimal::ZERO
            || self.cheap_token_price_threshold >= dec!(1)
        {
            anyhow::bail!(
                "CHEAP_TOKEN_PRICE_THRESHOLD_* invalid: {}",
                self.cheap_token_price_threshold
            );
        }
        if self.cheap_token_max_usdc < dec!(1) {
            anyhow::bail!(
                "CHEAP_TOKEN_MAX_USDC_* too low: {}",
                self.cheap_token_max_usdc
            );
        }
        self.volatility_filter.validate()?;
        Ok(())
    }
}

fn asset_upper_suffix(asset: &str) -> String {
    asset.trim().to_lowercase().to_uppercase()
}

fn env_vol_ratio_opt(prefix: &str, asset_upper: &str) -> Option<f64> {
    let k = format!("{prefix}_{asset_upper}");
    std::env::var(&k)
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| std::env::var(prefix).ok().and_then(|v| v.parse().ok()))
}

fn parse_dec_str(os: &Option<String>) -> Option<Decimal> {
    os.as_ref().and_then(|s| s.parse().ok())
}

/// Environment first, then optional TOML string field, then `default`.
fn env_toml_decimal(key: &str, tom: Option<Decimal>, default: Decimal) -> Decimal {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .or(tom)
        .unwrap_or(default)
}

fn env_toml_opt_decimal(key: &str, tom: Option<Decimal>) -> Option<Decimal> {
    std::env::var(key)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .and_then(|v| v.parse().ok())
        .or(tom)
}

fn env_toml_i64(key: &str, tom: Option<i64>) -> Option<i64> {
    std::env::var(key)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .and_then(|v| v.parse().ok())
        .or(tom)
}

fn env_toml_usize(key: &str, tom: Option<usize>, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .or(tom)
        .unwrap_or(default)
}

fn env_toml_f64(key: &str, tom: Option<f64>, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .or(tom)
        .unwrap_or(default)
}

fn env_toml_string(key: &str, tom: Option<&String>, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| tom.cloned())
        .unwrap_or_else(|| default.to_string())
}

fn env_toml_bool(key: &str, tom: Option<bool>, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .or(tom)
        .unwrap_or(default)
}

fn parse_signature_str(s: &str) -> SignatureType {
    match s.to_uppercase().as_str() {
        "GNOSIS_SAFE" | "GNOSISSAFE" | "GNOSIS" | "2" => SignatureType::GnosisSafe,
        "PROXY" | "1" => SignatureType::Proxy,
        "EOA" | "0" | _ => SignatureType::Eoa,
    }
}

fn asset_section<'a>(toml: Option<&'a TomlRoot>, asset: &str) -> Option<&'a AssetOverride> {
    toml.and_then(|t| t.asset.as_ref())
        .and_then(|m| m.get(&asset.to_lowercase()))
}

fn env_toml_asset_decimal(
    key: &str,
    su: &str,
    asset: Option<&AssetOverride>,
    global: Decimal,
    pick: impl FnOnce(&AssetOverride) -> Option<&String>,
) -> Decimal {
    let k = format!("{key}_{su}");
    if let Ok(v) = std::env::var(&k) {
        if let Ok(d) = v.parse::<Decimal>() {
            return d;
        }
    }
    if let Some(sec) = asset {
        if let Some(s) = pick(sec) {
            if let Ok(d) = s.parse::<Decimal>() {
                return d;
            }
        }
    }
    global
}

fn env_toml_asset_string(
    key: &str,
    su: &str,
    asset: Option<&AssetOverride>,
    global: &str,
    pick: impl FnOnce(&AssetOverride) -> Option<&String>,
) -> String {
    let k = format!("{key}_{su}");
    std::env::var(&k)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| asset.and_then(pick).cloned())
        .unwrap_or_else(|| global.to_string())
}

fn env_toml_asset_usize(
    key: &str,
    su: &str,
    asset: Option<&AssetOverride>,
    global: usize,
    pick: impl FnOnce(&AssetOverride) -> Option<usize>,
) -> usize {
    let k = format!("{key}_{su}");
    if let Ok(v) = std::env::var(&k) {
        if let Ok(u) = v.parse() {
            return u;
        }
    }
    if let Some(sec) = asset {
        if let Some(u) = pick(sec) {
            return u;
        }
    }
    global
}

fn env_toml_asset_bool(
    key: &str,
    su: &str,
    asset: Option<&AssetOverride>,
    global: bool,
    pick: impl FnOnce(&AssetOverride) -> Option<bool>,
) -> bool {
    let k = format!("{key}_{su}");
    if let Ok(v) = std::env::var(&k) {
        return matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    if let Some(sec) = asset {
        if let Some(b) = pick(sec) {
            return b;
        }
    }
    if let Ok(v) = std::env::var(key) {
        return matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    global
}

fn env_toml_asset_f64(
    key: &str,
    su: &str,
    asset: Option<&AssetOverride>,
    global: f64,
    pick: impl FnOnce(&AssetOverride) -> Option<f64>,
) -> f64 {
    let k = format!("{key}_{su}");
    if let Ok(v) = std::env::var(&k) {
        if let Ok(f) = v.parse() {
            return f;
        }
    }
    if let Some(sec) = asset {
        if let Some(f) = pick(sec) {
            return f;
        }
    }
    if let Ok(v) = std::env::var(key) {
        if let Ok(f) = v.parse() {
            return f;
        }
    }
    global
}

fn env_toml_asset_opt_decimal(
    key: &str,
    su: &str,
    asset: Option<&AssetOverride>,
    global: Option<Decimal>,
    pick: impl FnOnce(&AssetOverride) -> Option<&String>,
) -> Option<Decimal> {
    let k = format!("{key}_{su}");
    if let Ok(v) = std::env::var(&k) {
        if !v.trim().is_empty() {
            if let Ok(d) = v.parse() {
                return Some(d);
            }
        }
    }
    if let Some(sec) = asset {
        if let Some(s) = pick(sec) {
            if let Ok(d) = s.parse() {
                return Some(d);
            }
        }
    }
    if let Ok(v) = std::env::var(key) {
        if !v.trim().is_empty() {
            if let Ok(d) = v.parse() {
                return Some(d);
            }
        }
    }
    global
}

fn env_toml_asset_opt_i64(
    key: &str,
    su: &str,
    asset: Option<&AssetOverride>,
    global: Option<i64>,
    pick: impl FnOnce(&AssetOverride) -> Option<i64>,
) -> Option<i64> {
    let k = format!("{key}_{su}");
    if let Ok(v) = std::env::var(&k) {
        if !v.trim().is_empty() {
            if let Ok(i) = v.parse() {
                return Some(i);
            }
        }
    }
    if let Some(sec) = asset {
        if let Some(i) = pick(sec) {
            return Some(i);
        }
    }
    if let Ok(v) = std::env::var(key) {
        if !v.trim().is_empty() {
            if let Ok(i) = v.parse() {
                return Some(i);
            }
        }
    }
    global
}

fn parse_direction_str(s: &str) -> Option<Direction> {
    match s.trim().to_uppercase().as_str() {
        "YES" => Some(Direction::Yes),
        "NO" => Some(Direction::No),
        _ => None,
    }
}

/// `BLOCKED_DIRECTION_BTC` > `[asset.btc] blocked_direction` > `BLOCKED_DIRECTION` > `[strategy] blocked_direction`.
fn blocked_direction_for_asset(
    su: &str,
    asset: Option<&AssetOverride>,
    strategy_blocked: Option<&str>,
) -> Option<Direction> {
    let k = format!("BLOCKED_DIRECTION_{su}");
    if let Ok(v) = std::env::var(&k) {
        if !v.trim().is_empty() {
            if let Some(d) = parse_direction_str(&v) {
                return Some(d);
            }
        }
    }
    if let Some(sec) = asset {
        if let Some(ref s) = sec.blocked_direction {
            if let Some(d) = parse_direction_str(s) {
                return Some(d);
            }
        }
    }
    if let Ok(v) = std::env::var("BLOCKED_DIRECTION") {
        if !v.trim().is_empty() {
            if let Some(d) = parse_direction_str(&v) {
                return Some(d);
            }
        }
    }
    strategy_blocked.and_then(parse_direction_str)
}

fn vol_std_with_toml(
    prefix: &str,
    su: &str,
    asset: Option<&AssetOverride>,
    toml_vol: Option<&crate::config_toml::VolatilitySection>,
) -> Option<Decimal> {
    let k = format!("{prefix}_{su}");
    if let Ok(v) = std::env::var(&k) {
        if let Ok(d) = v.parse() {
            return Some(d);
        }
    }
    if let Some(sec) = asset {
        let s = match prefix {
            "VOL_MIN_STD_PCT" => sec.vol_min_std_pct.as_ref(),
            "VOL_MAX_STD_PCT" => sec.vol_max_std_pct.as_ref(),
            _ => None,
        };
        if let Some(s) = s {
            if let Ok(d) = s.parse() {
                return Some(d);
            }
        }
    }
    if let Ok(v) = std::env::var(prefix) {
        if let Ok(d) = v.parse() {
            return Some(d);
        }
    }
    toml_vol.and_then(|v| match prefix {
        "VOL_MIN_STD_PCT" => parse_dec_str(&v.min_std_pct),
        "VOL_MAX_STD_PCT" => parse_dec_str(&v.max_std_pct),
        _ => None,
    })
}

impl AppConfig {
    /// `env` > `config.toml` > defaults. Secrets (`POLYMARKET_PRIVATE_KEY`, Builder, `FUNDER_ADDRESS`) are **env-only**.
    pub fn load() -> Result<Self> {
        let toml_arc = crate::config_toml::load_optional(None)?.map(Arc::new);
        let r = toml_arc.as_deref();
        let ts = r.and_then(|x| x.strategy.as_ref());
        let tt = r.and_then(|x| x.technical.as_ref());
        let tc = r.and_then(|x| x.cluster.as_ref());
        let _tv = r.and_then(|x| x.volatility.as_ref());
        let tr = r.and_then(|x| x.risk.as_ref());
        let th = r.and_then(|x| x.htf.as_ref());
        let ta = r.and_then(|x| x.adaptive.as_ref());
        let te = r.and_then(|x| x.execution.as_ref());

        let assets = std::env::var("ASSETS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_lowercase())
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty())
            .or_else(|| ts.and_then(|s| s.assets.clone()).filter(|v| !v.is_empty()))
            .unwrap_or_else(|| vec!["btc".to_string(), "eth".to_string()]);

        let durations = std::env::var("DURATIONS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty())
            .or_else(|| {
                ts.and_then(|s| s.durations.clone())
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_else(|| vec!["5m".to_string(), "15m".to_string()]);

        let signature_s = std::env::var("SIGNATURE_TYPE")
            .ok()
            .or_else(|| te.and_then(|e| e.signature_type.clone()))
            .unwrap_or_else(|| "EOA".to_string());

        let config = Self {
            polymarket_private_key: std::env::var(PRIVATE_KEY_VAR)
                .context("POLYMARKET_PRIVATE_KEY not set")?,

            assets,

            durations,

            min_edge: env_toml_decimal(
                "MIN_EDGE",
                parse_dec_str(&ts.and_then(|s| s.min_edge.clone())),
                dec!(0.06),
            ),

            min_confidence: env_toml_decimal(
                "MIN_CONFIDENCE",
                parse_dec_str(&ts.and_then(|s| s.min_confidence.clone())),
                dec!(0.70),
            ),

            min_order_usdc: env_toml_decimal(
                "MIN_ORDER_USDC",
                parse_dec_str(&ts.and_then(|s| s.min_order_usdc.clone())),
                dec!(5),
            ),

            slippage_bps: env_toml_decimal(
                "SLIPPAGE_BPS",
                parse_dec_str(&ts.and_then(|s| s.slippage_bps.clone())),
                SLIPPAGE_BPS,
            ),

            spot_exchange: env_toml_string(
                "SPOT_EXCHANGE",
                tt.and_then(|t| t.spot_exchange.as_ref()),
                "binance",
            ),

            candle_interval: env_toml_string(
                "CANDLE_INTERVAL",
                tt.and_then(|t| t.candle_interval.as_ref()),
                "1m",
            ),

            candle_lookback: env_toml_usize(
                "CANDLE_LOOKBACK",
                tt.and_then(|t| t.candle_lookback),
                100,
            ),

            rsi_period: env_toml_usize("RSI_PERIOD", tt.and_then(|t| t.rsi_period), 14),

            macd_fast: env_toml_usize("MACD_FAST", tt.and_then(|t| t.macd_fast), 12),

            macd_slow: env_toml_usize("MACD_SLOW", tt.and_then(|t| t.macd_slow), 26),

            macd_signal: env_toml_usize("MACD_SIGNAL", tt.and_then(|t| t.macd_signal), 9),

            volume_min_ratio: std::env::var("VOLUME_MIN_RATIO")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(tt.and_then(|t| t.volume_min_ratio)),

            volume_avg_bars: env_toml_usize(
                "VOLUME_AVG_BARS",
                tt.and_then(|t| t.volume_avg_bars),
                20,
            ),

            max_position_pct: env_toml_decimal(
                "MAX_POSITION_PCT",
                parse_dec_str(&tr.and_then(|k| k.max_position_pct.clone())),
                dec!(0.05),
            ),

            daily_loss_limit_pct: env_toml_decimal(
                "DAILY_LOSS_LIMIT_PCT",
                parse_dec_str(&tr.and_then(|k| k.daily_loss_limit_pct.clone())),
                dec!(0.10),
            ),

            initial_balance: env_toml_decimal(
                "INITIAL_BALANCE",
                parse_dec_str(&tr.and_then(|k| k.initial_balance.clone())),
                dec!(200),
            ),

            dry_run: env_toml_bool("DRY_RUN", te.and_then(|e| e.dry_run), true),

            cycle_secs: std::env::var("CYCLE_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(te.and_then(|e| e.cycle_secs))
                .unwrap_or(60),

            fill_timeout_secs: std::env::var("FILL_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(te.and_then(|e| e.fill_timeout_secs))
                .unwrap_or(300),

            poll_min_order_age_secs: std::env::var("POLL_MIN_ORDER_AGE_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(te.and_then(|e| e.poll_min_order_age_secs))
                .unwrap_or(10),

            gamma_tag_id: std::env::var("GAMMA_TAG_ID")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(ts.and_then(|s| s.gamma_tag_id))
                .unwrap_or(GAMMA_TAG_ID_DEFAULT),

            clob_host: env_toml_string(
                "CLOB_HOST",
                te.and_then(|e| e.clob_host.as_ref()),
                "https://clob.polymarket.com",
            ),

            chain_id: POLYGON,

            signature_type: parse_signature_str(&signature_s),

            funder_address: std::env::var("FUNDER_ADDRESS").ok(),

            builder_api_key: std::env::var("BUILDER_API_KEY").ok(),
            builder_api_secret: std::env::var("BUILDER_API_SECRET").ok(),
            builder_api_passphrase: std::env::var("BUILDER_API_PASSPHRASE").ok(),

            data_dir: env_toml_string("DATA_DIR", te.and_then(|e| e.data_dir.as_ref()), "data"),

            htf_enabled: env_toml_bool("HTF_ENABLED", th.and_then(|h| h.enabled), false),

            htf_interval: env_toml_string(
                "HTF_INTERVAL",
                th.and_then(|h| h.interval.as_ref()),
                "15m",
            ),

            htf_lookback: env_toml_usize("HTF_LOOKBACK", th.and_then(|h| h.lookback), 50),

            htf_ema_period: env_toml_usize("HTF_EMA_PERIOD", th.and_then(|h| h.ema_period), 20),

            adaptive_thresholds: env_toml_bool(
                "ADAPTIVE_THRESHOLDS",
                ta.and_then(|a| a.enabled),
                false,
            ),

            adaptive_trade_window: env_toml_usize(
                "ADAPTIVE_TRADE_WINDOW",
                ta.and_then(|a| a.trade_window),
                50,
            ),

            min_secs_to_close: env_toml_i64(
                "MIN_SECS_TO_CLOSE",
                tc.and_then(|c| c.min_secs_to_close),
            ),

            max_secs_to_close: env_toml_i64(
                "MAX_SECS_TO_CLOSE",
                tc.and_then(|c| c.max_secs_to_close),
            ),

            expiry_dampen_last_secs: env_toml_i64(
                "EXPIRY_DAMPEN_LAST_SECS",
                tc.and_then(|c| c.expiry_dampen_last_secs),
            ),

            min_market_yes_price: env_toml_opt_decimal(
                "MIN_MARKET_YES_PRICE",
                parse_dec_str(&tc.and_then(|c| c.min_market_yes_price.clone())),
            ),

            max_market_yes_price: env_toml_opt_decimal(
                "MAX_MARKET_YES_PRICE",
                parse_dec_str(&tc.and_then(|c| c.max_market_yes_price.clone())),
            ),

            cheap_token_price_threshold: env_toml_decimal(
                "CHEAP_TOKEN_PRICE_THRESHOLD",
                parse_dec_str(&tc.and_then(|c| c.cheap_token_price_threshold.clone())),
                dec!(0.15),
            ),

            cheap_token_max_usdc: env_toml_decimal(
                "CHEAP_TOKEN_MAX_USDC",
                parse_dec_str(&tc.and_then(|c| c.cheap_token_max_usdc.clone())),
                dec!(5),
            ),

            large_order_usdc_hard_cap: env_toml_opt_decimal(
                "LARGE_ORDER_USDC_HARD_CAP",
                parse_dec_str(&tc.and_then(|c| c.large_order_usdc_hard_cap.clone())),
            ),

            volume_use_closed_candle_only: env_toml_bool(
                "VOLUME_USE_CLOSED_CANDLE_ONLY",
                tt.and_then(|t| t.volume_use_closed_candle_only),
                true,
            ),

            cluster_rsi_oversold: env_toml_f64(
                "CLUSTER_RSI_OVERSOLD",
                tc.and_then(|c| c.rsi_oversold),
                40.0,
            ),

            cluster_rsi_overbought: env_toml_f64(
                "CLUSTER_RSI_OVERBOUGHT",
                tc.and_then(|c| c.rsi_overbought),
                60.0,
            ),

            cluster_mom5_abs: env_toml_f64("CLUSTER_MOM5_ABS", tc.and_then(|c| c.mom5_abs), 0.003),

            cluster_mom15_abs: env_toml_f64(
                "CLUSTER_MOM15_ABS",
                tc.and_then(|c| c.mom15_abs),
                0.005,
            ),

            cluster_tie_min_edge_multiplier: env_toml_f64(
                "CLUSTER_TIE_MIN_EDGE_MULTIPLIER",
                tc.and_then(|c| c.cluster_tie_min_edge_multiplier),
                1.0,
            ),

            min_momentum_5m_abs: env_toml_f64(
                "MIN_MOMENTUM_5M_ABS",
                tc.and_then(|c| c.min_momentum_5m_abs),
                0.0008,
            ),

            neutral_taker_edge_multiplier: env_toml_f64(
                "NEUTRAL_TAKER_EDGE_MULTIPLIER",
                tc.and_then(|c| c.neutral_taker_edge_multiplier),
                1.5,
            ),

            rsi_yes_max: env_toml_f64(
                "RSI_YES_MAX",
                tc.and_then(|c| c.rsi_yes_max),
                70.0,
            ),

            rsi_no_min: env_toml_f64(
                "RSI_NO_MIN",
                tc.and_then(|c| c.rsi_no_min),
                30.0,
            ),

            toml: toml_arc,
        };

        config.validate()?;

        for a in &config.assets {
            config.asset_strategy(a).validate().with_context(|| {
                format!(
                    "invalid per-asset strategy overrides for asset {a:?} (see MIN_EDGE_{} etc.)",
                    asset_upper_suffix(a)
                )
            })?;
        }

        Ok(config)
    }

    /// Same as [`Self::load`]. Kept for callers that historically used env-only loading.
    pub fn from_env() -> Result<Self> {
        Self::load()
    }

    /// Effective strategy for `asset` (global env + `KEY_{ASSET}` overrides + optional `config.toml` per asset).
    pub fn asset_strategy(&self, asset: &str) -> AssetStrategy {
        let su = asset_upper_suffix(asset);
        let a = asset_section(self.toml.as_deref(), asset);
        let toml_vol = self.toml.as_deref().and_then(|t| t.volatility.as_ref());
        let vol_sample_global = std::env::var("VOL_SAMPLE_BARS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| toml_vol.and_then(|v| v.sample_bars))
            .unwrap_or(20);
        AssetStrategy {
            min_edge: env_toml_asset_decimal("MIN_EDGE", &su, a, self.min_edge, |x| {
                x.min_edge.as_ref()
            }),
            min_confidence: env_toml_asset_decimal(
                "MIN_CONFIDENCE",
                &su,
                a,
                self.min_confidence,
                |x| x.min_confidence.as_ref(),
            ),
            min_order_usdc: env_toml_asset_decimal(
                "MIN_ORDER_USDC",
                &su,
                a,
                self.min_order_usdc,
                |x| x.min_order_usdc.as_ref(),
            ),
            spot_exchange: env_toml_asset_string(
                "SPOT_EXCHANGE",
                &su,
                a,
                &self.spot_exchange,
                |x| x.spot_exchange.as_ref(),
            ),
            candle_interval: env_toml_asset_string(
                "CANDLE_INTERVAL",
                &su,
                a,
                &self.candle_interval,
                |x| x.candle_interval.as_ref(),
            ),
            candle_lookback: env_toml_asset_usize(
                "CANDLE_LOOKBACK",
                &su,
                a,
                self.candle_lookback,
                |x| x.candle_lookback,
            ),
            rsi_period: env_toml_asset_usize("RSI_PERIOD", &su, a, self.rsi_period, |x| {
                x.rsi_period
            }),
            macd_fast: env_toml_asset_usize("MACD_FAST", &su, a, self.macd_fast, |x| x.macd_fast),
            macd_slow: env_toml_asset_usize("MACD_SLOW", &su, a, self.macd_slow, |x| x.macd_slow),
            macd_signal: env_toml_asset_usize("MACD_SIGNAL", &su, a, self.macd_signal, |x| {
                x.macd_signal
            }),
            volume_min_ratio: env_vol_ratio_opt("VOLUME_MIN_RATIO", &su)
                .or_else(|| a.and_then(|x| x.volume_min_ratio))
                .or(self.volume_min_ratio),
            volume_avg_bars: env_toml_asset_usize(
                "VOLUME_AVG_BARS",
                &su,
                a,
                self.volume_avg_bars,
                |x| x.volume_avg_bars,
            ),
            max_position_pct: env_toml_asset_decimal(
                "MAX_POSITION_PCT",
                &su,
                a,
                self.max_position_pct,
                |x| x.max_position_pct.as_ref(),
            ),
            daily_loss_limit_pct: env_toml_asset_decimal(
                "DAILY_LOSS_LIMIT_PCT",
                &su,
                a,
                self.daily_loss_limit_pct,
                |x| x.daily_loss_limit_pct.as_ref(),
            ),
            volatility_filter: VolatilityFilterConfig {
                min_std_pct: vol_std_with_toml("VOL_MIN_STD_PCT", &su, a, toml_vol),
                max_std_pct: vol_std_with_toml("VOL_MAX_STD_PCT", &su, a, toml_vol),
                sample_bars: env_toml_asset_usize(
                    "VOL_SAMPLE_BARS",
                    &su,
                    a,
                    vol_sample_global,
                    |x| x.vol_sample_bars,
                ),
            },
            htf_enabled: env_toml_asset_bool("HTF_ENABLED", &su, a, self.htf_enabled, |x| {
                x.htf_enabled
            }),
            htf_interval: env_toml_asset_string("HTF_INTERVAL", &su, a, &self.htf_interval, |x| {
                x.htf_interval.as_ref()
            }),
            htf_lookback: env_toml_asset_usize("HTF_LOOKBACK", &su, a, self.htf_lookback, |x| {
                x.htf_lookback
            }),
            htf_ema_period: env_toml_asset_usize(
                "HTF_EMA_PERIOD",
                &su,
                a,
                self.htf_ema_period,
                |x| x.htf_ema_period,
            ),
            adaptive_thresholds: env_toml_asset_bool(
                "ADAPTIVE_THRESHOLDS",
                &su,
                a,
                self.adaptive_thresholds,
                |x| x.adaptive_thresholds,
            ),
            adaptive_trade_window: env_toml_asset_usize(
                "ADAPTIVE_TRADE_WINDOW",
                &su,
                a,
                self.adaptive_trade_window,
                |x| x.adaptive_trade_window,
            ),

            min_secs_to_close: env_toml_asset_opt_i64(
                "MIN_SECS_TO_CLOSE",
                &su,
                a,
                self.min_secs_to_close,
                |x| x.min_secs_to_close,
            ),
            expiry_dampen_last_secs: env_toml_asset_opt_i64(
                "EXPIRY_DAMPEN_LAST_SECS",
                &su,
                a,
                self.expiry_dampen_last_secs,
                |x| x.expiry_dampen_last_secs,
            ),
            min_market_yes_price: env_toml_asset_opt_decimal(
                "MIN_MARKET_YES_PRICE",
                &su,
                a,
                self.min_market_yes_price,
                |x| x.min_market_yes_price.as_ref(),
            ),
            max_market_yes_price: env_toml_asset_opt_decimal(
                "MAX_MARKET_YES_PRICE",
                &su,
                a,
                self.max_market_yes_price,
                |x| x.max_market_yes_price.as_ref(),
            ),
            cheap_token_price_threshold: env_toml_asset_decimal(
                "CHEAP_TOKEN_PRICE_THRESHOLD",
                &su,
                a,
                self.cheap_token_price_threshold,
                |x| x.cheap_token_price_threshold.as_ref(),
            ),
            cheap_token_max_usdc: env_toml_asset_decimal(
                "CHEAP_TOKEN_MAX_USDC",
                &su,
                a,
                self.cheap_token_max_usdc,
                |x| x.cheap_token_max_usdc.as_ref(),
            ),
            large_order_usdc_hard_cap: env_toml_asset_opt_decimal(
                "LARGE_ORDER_USDC_HARD_CAP",
                &su,
                a,
                self.large_order_usdc_hard_cap,
                |x| x.large_order_usdc_hard_cap.as_ref(),
            ),
            volume_use_closed_candle_only: env_toml_asset_bool(
                "VOLUME_USE_CLOSED_CANDLE_ONLY",
                &su,
                a,
                self.volume_use_closed_candle_only,
                |x| x.volume_use_closed_candle_only,
            ),
            cluster_rsi_oversold: env_toml_asset_f64(
                "CLUSTER_RSI_OVERSOLD",
                &su,
                a,
                self.cluster_rsi_oversold,
                |x| x.cluster_rsi_oversold,
            ),
            cluster_rsi_overbought: env_toml_asset_f64(
                "CLUSTER_RSI_OVERBOUGHT",
                &su,
                a,
                self.cluster_rsi_overbought,
                |x| x.cluster_rsi_overbought,
            ),
            cluster_mom5_abs: env_toml_asset_f64(
                "CLUSTER_MOM5_ABS",
                &su,
                a,
                self.cluster_mom5_abs,
                |x| x.cluster_mom5_abs,
            ),
            cluster_mom15_abs: env_toml_asset_f64(
                "CLUSTER_MOM15_ABS",
                &su,
                a,
                self.cluster_mom15_abs,
                |x| x.cluster_mom15_abs,
            ),
            cluster_tie_min_edge_multiplier: env_toml_asset_f64(
                "CLUSTER_TIE_MIN_EDGE_MULTIPLIER",
                &su,
                a,
                self.cluster_tie_min_edge_multiplier,
                |x| x.cluster_tie_min_edge_multiplier,
            ),
            min_momentum_5m_abs: env_toml_asset_f64(
                "MIN_MOMENTUM_5M_ABS",
                &su,
                a,
                self.min_momentum_5m_abs,
                |x| x.min_momentum_5m_abs,
            ),
            neutral_taker_edge_multiplier: env_toml_asset_f64(
                "NEUTRAL_TAKER_EDGE_MULTIPLIER",
                &su,
                a,
                self.neutral_taker_edge_multiplier,
                |x| x.neutral_taker_edge_multiplier,
            ),
            rsi_yes_max: env_toml_asset_f64(
                "RSI_YES_MAX",
                &su,
                a,
                self.rsi_yes_max,
                |x| x.rsi_yes_max,
            ),
            rsi_no_min: env_toml_asset_f64(
                "RSI_NO_MIN",
                &su,
                a,
                self.rsi_no_min,
                |x| x.rsi_no_min,
            ),
            slippage_bps: env_toml_asset_decimal("SLIPPAGE_BPS", &su, a, self.slippage_bps, |x| {
                x.slippage_bps.as_ref()
            }),
            max_secs_to_close: env_toml_asset_opt_i64(
                "MAX_SECS_TO_CLOSE",
                &su,
                a,
                self.max_secs_to_close,
                |x| x.max_secs_to_close,
            ),
            blocked_direction: blocked_direction_for_asset(
                &su,
                a,
                self.toml
                    .as_deref()
                    .and_then(|t| t.strategy.as_ref())
                    .and_then(|s| s.blocked_direction.as_deref()),
            ),
        }
    }

    /// Validate globals and auth. Per-asset TA/strategy rules live in [`AssetStrategy::validate`].
    pub fn validate(&self) -> Result<()> {
        if self.min_edge <= Decimal::ZERO {
            anyhow::bail!("MIN_EDGE must be positive, got: {}", self.min_edge);
        }

        if self.min_edge > dec!(0.50) {
            anyhow::bail!(
                "MIN_EDGE too high (>50%), unrealistic edge expectation: {}",
                self.min_edge
            );
        }

        if self.min_confidence < dec!(0.5) || self.min_confidence > dec!(1.0) {
            anyhow::bail!(
                "MIN_CONFIDENCE must be between 0.5 and 1.0, got: {}",
                self.min_confidence
            );
        }

        if self.max_position_pct <= Decimal::ZERO {
            anyhow::bail!(
                "MAX_POSITION_PCT must be positive, got: {}",
                self.max_position_pct
            );
        }

        if self.max_position_pct > dec!(0.5) {
            anyhow::bail!(
                "MAX_POSITION_PCT too high (>50%), risk of over-leverage: {}",
                self.max_position_pct
            );
        }

        if self.daily_loss_limit_pct <= Decimal::ZERO || self.daily_loss_limit_pct > dec!(1.0) {
            anyhow::bail!(
                "DAILY_LOSS_LIMIT_PCT must be between 0 and 1.0, got: {}",
                self.daily_loss_limit_pct
            );
        }

        if self.initial_balance < dec!(1) {
            anyhow::bail!("INITIAL_BALANCE too low, got: {}", self.initial_balance);
        }

        if self.min_order_usdc < dec!(1) {
            anyhow::bail!(
                "MIN_ORDER_USDC too low (min $1), got: {}",
                self.min_order_usdc
            );
        }

        if self.assets.is_empty() {
            anyhow::bail!("ASSETS cannot be empty");
        }

        if self.durations.is_empty() {
            anyhow::bail!("DURATIONS cannot be empty");
        }

        if let (Some(lo), Some(hi)) = (self.min_market_yes_price, self.max_market_yes_price) {
            if lo >= hi {
                anyhow::bail!(
                    "MIN_MARKET_YES_PRICE ({}) must be < MAX_MARKET_YES_PRICE ({})",
                    lo,
                    hi
                );
            }
        }

        if self.cluster_tie_min_edge_multiplier < 1.0 || self.cluster_tie_min_edge_multiplier > 5.0
        {
            anyhow::bail!(
                "CLUSTER_TIE_MIN_EDGE_MULTIPLIER must be in [1.0, 5.0], got: {}",
                self.cluster_tie_min_edge_multiplier
            );
        }
        if self.min_momentum_5m_abs < 0.0 || self.min_momentum_5m_abs > 0.05 {
            anyhow::bail!(
                "MIN_MOMENTUM_5M_ABS must be in [0.0, 0.05] (0 = off), got: {}",
                self.min_momentum_5m_abs
            );
        }
        if self.neutral_taker_edge_multiplier < 1.0 || self.neutral_taker_edge_multiplier > 5.0 {
            anyhow::bail!(
                "NEUTRAL_TAKER_EDGE_MULTIPLIER must be in [1.0, 5.0], got: {}",
                self.neutral_taker_edge_multiplier
            );
        }
        if self.rsi_yes_max < 0.0 || self.rsi_yes_max > 100.0 {
            anyhow::bail!(
                "RSI_YES_MAX must be in [0, 100] (0 = off), got: {}",
                self.rsi_yes_max
            );
        }
        if self.rsi_no_min < 0.0 || self.rsi_no_min > 100.0 {
            anyhow::bail!(
                "RSI_NO_MIN must be in [0, 100] (0 = off), got: {}",
                self.rsi_no_min
            );
        }
        if self.rsi_yes_max > 0.0 && self.rsi_no_min > 0.0 && self.rsi_no_min >= self.rsi_yes_max {
            anyhow::bail!(
                "RSI_NO_MIN ({}) must be < RSI_YES_MAX ({}) when both are enabled",
                self.rsi_no_min,
                self.rsi_yes_max
            );
        }
        if self.slippage_bps <= Decimal::ZERO || self.slippage_bps > dec!(0.05) {
            anyhow::bail!(
                "SLIPPAGE_BPS must be in (0, 0.05], got: {}",
                self.slippage_bps
            );
        }
        if let (Some(lo), Some(hi)) = (self.min_secs_to_close, self.max_secs_to_close) {
            if hi <= lo {
                anyhow::bail!(
                    "MAX_SECS_TO_CLOSE ({}) must be > MIN_SECS_TO_CLOSE ({}) when both are set",
                    hi,
                    lo
                );
            }
        }

        self.validate_polymarket_auth()?;

        Ok(())
    }
}

impl AppConfig {
    /// Validate signature type and funder configuration
    ///
    /// Note: Funder address is now OPTIONAL for Proxy/GnosisSafe.
    /// SDK will auto-derive via CREATE2 if not provided.
    pub fn validate_polymarket_auth(&self) -> Result<()> {
        match (self.signature_type, &self.funder_address) {
            (SignatureType::Eoa, Some(_)) => {
                anyhow::bail!("EOA signature type cannot have a funder address (funder is auto-derived for EOA wallets)")
            }
            // Proxy/GnosisSafe can have funder (manual) or None (auto-derived)
            _ => Ok(()),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            polymarket_private_key: String::new(),
            assets: vec!["btc".to_string(), "eth".to_string()],
            durations: vec!["5m".to_string(), "15m".to_string()],
            min_edge: dec!(0.06),
            min_confidence: dec!(0.70),
            min_order_usdc: dec!(5),
            slippage_bps: SLIPPAGE_BPS,
            spot_exchange: "binance".to_string(),
            candle_interval: "1m".to_string(),
            candle_lookback: 100,
            rsi_period: 14,
            macd_fast: 12,
            macd_slow: 26,
            macd_signal: 9,
            volume_min_ratio: None,
            volume_avg_bars: 20,
            max_position_pct: dec!(0.05),
            daily_loss_limit_pct: dec!(0.10),
            initial_balance: dec!(200),
            dry_run: true,
            cycle_secs: 60,
            fill_timeout_secs: 300,
            poll_min_order_age_secs: 10,
            gamma_tag_id: GAMMA_TAG_ID_DEFAULT,
            clob_host: "https://clob.polymarket.com".to_string(),
            chain_id: POLYGON,
            signature_type: SignatureType::Eoa,
            funder_address: None,
            builder_api_key: None,
            builder_api_secret: None,
            builder_api_passphrase: None,

            data_dir: "data".to_string(),
            htf_enabled: false,
            htf_interval: "15m".to_string(),
            htf_lookback: 50,
            htf_ema_period: 20,
            adaptive_thresholds: false,
            adaptive_trade_window: 50,

            min_secs_to_close: None,
            max_secs_to_close: None,
            expiry_dampen_last_secs: None,
            min_market_yes_price: None,
            max_market_yes_price: None,
            cheap_token_price_threshold: dec!(0.15),
            cheap_token_max_usdc: dec!(5),
            large_order_usdc_hard_cap: None,
            volume_use_closed_candle_only: true,
            cluster_rsi_oversold: 40.0,
            cluster_rsi_overbought: 60.0,
            cluster_mom5_abs: 0.003,
            cluster_mom15_abs: 0.005,
            cluster_tie_min_edge_multiplier: 1.0,
            min_momentum_5m_abs: 0.0,
            neutral_taker_edge_multiplier: 1.0,
            rsi_yes_max: 70.0,
            rsi_no_min: 30.0,

            toml: None,
        }
    }
}

impl SignatureType {
    /// Convert to Polymarket SDK signature type
    pub fn to_sdk_type(&self) -> polymarket_client_sdk::clob::types::SignatureType {
        match self {
            SignatureType::Eoa => polymarket_client_sdk::clob::types::SignatureType::Eoa,
            SignatureType::Proxy => polymarket_client_sdk::clob::types::SignatureType::Proxy,
            SignatureType::GnosisSafe => {
                polymarket_client_sdk::clob::types::SignatureType::GnosisSafe
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Direction;
    use rust_decimal_macros::dec;

    fn valid_app_config() -> AppConfig {
        let mut c = AppConfig::default();
        c.polymarket_private_key =
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        c
    }

    #[test]
    fn app_config_validate_accepts_sensible_defaults() {
        valid_app_config()
            .validate()
            .expect("default should validate");
    }

    #[test]
    fn app_config_rejects_empty_assets() {
        let mut c = valid_app_config();
        c.assets = vec![];
        assert!(c.validate().is_err());
    }

    #[test]
    fn app_config_rejects_empty_durations() {
        let mut c = valid_app_config();
        c.durations = vec![];
        assert!(c.validate().is_err());
    }

    #[test]
    fn app_config_rejects_min_edge_non_positive() {
        let mut c = valid_app_config();
        c.min_edge = dec!(0);
        assert!(c.validate().is_err());
    }

    #[test]
    fn app_config_rejects_min_edge_above_half() {
        let mut c = valid_app_config();
        c.min_edge = dec!(0.51);
        assert!(c.validate().is_err());
    }

    #[test]
    fn app_config_rejects_min_confidence_below_half() {
        let mut c = valid_app_config();
        c.min_confidence = dec!(0.49);
        assert!(c.validate().is_err());
    }

    #[test]
    fn app_config_rejects_yes_price_band_inverted() {
        let mut c = valid_app_config();
        c.min_market_yes_price = Some(dec!(0.6));
        c.max_market_yes_price = Some(dec!(0.5));
        assert!(c.validate().is_err());
    }

    #[test]
    fn app_config_rejects_eoa_with_funder() {
        let mut c = valid_app_config();
        c.signature_type = SignatureType::Eoa;
        c.funder_address = Some("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string());
        assert!(c.validate().is_err());
    }

    #[test]
    fn asset_strategy_rejects_invalid_cluster_rsi_order() {
        let mut st = valid_app_config().asset_strategy("btc");
        st.cluster_rsi_oversold = 55.0;
        st.cluster_rsi_overbought = 50.0;
        assert!(st.validate().is_err());
    }

    #[test]
    fn asset_strategy_rejects_macd_fast_gte_slow() {
        let mut st = valid_app_config().asset_strategy("btc");
        st.macd_fast = 20;
        st.macd_slow = 12;
        assert!(st.validate().is_err());
    }

    #[test]
    fn asset_strategy_rejects_vol_min_gte_max() {
        let mut st = valid_app_config().asset_strategy("btc");
        st.volatility_filter.min_std_pct = Some(dec!(0.1));
        st.volatility_filter.max_std_pct = Some(dec!(0.05));
        assert!(st.validate().is_err());
    }

    #[test]
    fn asset_strategy_rejects_cluster_tie_multiplier_out_of_range() {
        let mut st = valid_app_config().asset_strategy("btc");
        st.cluster_tie_min_edge_multiplier = 6.0;
        assert!(st.validate().is_err());
    }

    #[test]
    fn asset_strategy_rejects_max_secs_not_above_min_secs() {
        let mut st = valid_app_config().asset_strategy("btc");
        st.min_secs_to_close = Some(600);
        st.max_secs_to_close = Some(500);
        assert!(st.validate().is_err());
    }

    #[test]
    fn asset_strategy_accepts_blocked_direction() {
        let mut st = valid_app_config().asset_strategy("btc");
        st.blocked_direction = Some(Direction::Yes);
        st.validate().expect("blocked_direction should validate");
    }
}
