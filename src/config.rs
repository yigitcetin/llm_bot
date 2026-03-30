use anyhow::{Context, Result};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use polymarket_client_sdk::{POLYGON, PRIVATE_KEY_VAR};

use crate::constants::GAMMA_TAG_ID_DEFAULT;
use crate::signals::SignalConfig;
use crate::volatility::VolatilityFilterConfig;

#[derive(Debug, Clone)]
pub struct AppConfig {
    // Auth
    pub polymarket_private_key: String,

    // Strategy
    pub assets: Vec<String>,
    pub durations: Vec<String>,

    // Edge / signal thresholds
    pub min_edge: Decimal,          // minimum technical prob vs market price gap
    pub min_confidence: Decimal,    // minimum technical confidence score
    pub min_order_usdc: Decimal,    // minimum order size in USDC

    // Technical Analysis
    pub spot_exchange: String,      // "binance", "coinbase", etc.
    pub candle_interval: String,    // "1m", "5m"
    pub candle_lookback: usize,     // how many candles to analyze
    pub rsi_period: usize,          // RSI calculation period
    pub macd_fast: usize,           // MACD fast EMA
    pub macd_slow: usize,           // MACD slow EMA
    pub macd_signal: usize,         // MACD signal line
    /// Spot hacim kalitesi: son mum / ortalama altındaysa sinyal yok (`None` = veto kapalı).
    pub volume_min_ratio: Option<f64>,
    pub volume_avg_bars: usize,     // hacim ortalaması için mum sayısı

    // Risk
    pub max_position_pct: Decimal,  // max % of balance per trade
    pub daily_loss_limit_pct: Decimal,
    pub initial_balance: Decimal,

    // Execution
    pub dry_run: bool,
    pub cycle_secs: u64,

    // Polymarket
    /// Gamma API `tag_id` filter for listing events (see Polymarket tags).
    pub gamma_tag_id: u64,
    pub clob_host: String,
    pub chain_id: u64,
    pub signature_type: SignatureType,
    pub funder_address: Option<String>,  // Required for Proxy/GnosisSafe, must be None for EOA

    // Builder API (optional - for market makers)
    pub builder_api_key: Option<String>,
    pub builder_api_secret: Option<String>,
    pub builder_api_passphrase: Option<String>,
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

/// RSI + MACD settings mirrored in [`crate::signals::SignalConfig`].
#[derive(Debug, Clone, Copy)]
pub struct TaParams {
    pub rsi_period: usize,
    pub macd_fast: usize,
    pub macd_slow: usize,
    pub macd_signal: usize,
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
            anyhow::bail!("RSI_PERIOD_* must be between 5 and 50, got: {}", self.rsi_period);
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
                anyhow::bail!(
                    "VOLUME_MIN_RATIO_* must be in (0, 5], got: {}",
                    r
                );
            }
        }
        if self.volume_avg_bars < 5 || self.volume_avg_bars > 200 {
            anyhow::bail!(
                "VOLUME_AVG_BARS_* must be between 5 and 200, got: {}",
                self.volume_avg_bars
            );
        }
        self.volatility_filter.validate()?;
        Ok(())
    }
}

fn asset_upper_suffix(asset: &str) -> String {
    asset.trim().to_lowercase().to_uppercase()
}

fn env_override_decimal(key: &str, asset_upper: &str, fallback: Decimal) -> Decimal {
    let k = format!("{key}_{asset_upper}");
    std::env::var(&k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

fn env_override_usize(key: &str, asset_upper: &str, fallback: usize) -> usize {
    let k = format!("{key}_{asset_upper}");
    std::env::var(&k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

fn env_override_string(key: &str, asset_upper: &str, fallback: &str) -> String {
    let k = format!("{key}_{asset_upper}");
    std::env::var(&k)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// `VOL_MIN_STD_PCT_BTC` sonra global `VOL_MIN_STD_PCT`.
fn env_vol_std_opt(prefix: &str, asset_upper: &str) -> Option<Decimal> {
    let k = format!("{prefix}_{asset_upper}");
    std::env::var(&k)
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| std::env::var(prefix).ok().and_then(|v| v.parse().ok()))
}

fn env_vol_ratio_opt(prefix: &str, asset_upper: &str) -> Option<f64> {
    let k = format!("{prefix}_{asset_upper}");
    std::env::var(&k)
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| std::env::var(prefix).ok().and_then(|v| v.parse().ok()))
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        let config = Self {
            polymarket_private_key: std::env::var(PRIVATE_KEY_VAR)
                .context("POLYMARKET_PRIVATE_KEY not set")?,

            assets: std::env::var("ASSETS")
                .unwrap_or_else(|_| "btc,eth".to_string())
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .collect(),

            durations: std::env::var("DURATIONS")
                .unwrap_or_else(|_| "5m".to_string())
                .split(',')
                .map(|s| s.trim().to_string())
                .collect(),

            min_edge: std::env::var("MIN_EDGE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(dec!(0.06)),           // 6% minimum edge

            min_confidence: std::env::var("MIN_CONFIDENCE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(dec!(0.70)),           // 70% technical confidence

            min_order_usdc: std::env::var("MIN_ORDER_USDC")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(dec!(5)),

            spot_exchange: std::env::var("SPOT_EXCHANGE")
                .unwrap_or_else(|_| "binance".to_string()),

            candle_interval: std::env::var("CANDLE_INTERVAL")
                .unwrap_or_else(|_| "1m".to_string()),

            candle_lookback: std::env::var("CANDLE_LOOKBACK")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100),                  // 100 candles for TA

            rsi_period: std::env::var("RSI_PERIOD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(14),                   // standard RSI period

            macd_fast: std::env::var("MACD_FAST")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(12),                   // standard MACD fast EMA

            macd_slow: std::env::var("MACD_SLOW")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(26),                   // standard MACD slow EMA

            macd_signal: std::env::var("MACD_SIGNAL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(9),                    // standard MACD signal

            volume_min_ratio: std::env::var("VOLUME_MIN_RATIO")
                .ok()
                .and_then(|v| v.parse().ok()),

            volume_avg_bars: std::env::var("VOLUME_AVG_BARS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(20),

            max_position_pct: std::env::var("MAX_POSITION_PCT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(dec!(0.05)),           // 5% of balance max

            daily_loss_limit_pct: std::env::var("DAILY_LOSS_LIMIT_PCT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(dec!(0.10)),           // 10% daily loss limit

            initial_balance: std::env::var("INITIAL_BALANCE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(dec!(200)),

            dry_run: std::env::var("DRY_RUN")
                .unwrap_or_else(|_| "true".to_string())
                .parse()
                .unwrap_or(true),

            cycle_secs: std::env::var("CYCLE_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),                   // scan every 60 seconds

            gamma_tag_id: std::env::var("GAMMA_TAG_ID")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(GAMMA_TAG_ID_DEFAULT),

            clob_host: std::env::var("CLOB_HOST")
                .unwrap_or_else(|_| "https://clob.polymarket.com".to_string()),

            chain_id: POLYGON,

            signature_type: match std::env::var("SIGNATURE_TYPE")
                .unwrap_or_else(|_| "EOA".to_string())
                .to_uppercase()
                .as_str()
            {
                "GNOSIS_SAFE" | "GNOSISSAFE" | "GNOSIS" | "2" => SignatureType::GnosisSafe,
                "PROXY" | "1" => SignatureType::Proxy,
                "EOA" | "0" | _ => SignatureType::Eoa,
            },

            funder_address: std::env::var("FUNDER_ADDRESS").ok(),

            // Builder API credentials (optional)
            builder_api_key: std::env::var("BUILDER_API_KEY").ok(),
            builder_api_secret: std::env::var("BUILDER_API_SECRET").ok(),
            builder_api_passphrase: std::env::var("BUILDER_API_PASSPHRASE").ok(),
        };

        // Validate configuration before returning
        config.validate()?;

        for a in &config.assets {
            config.asset_strategy(a).validate().with_context(|| {
                format!("invalid per-asset strategy overrides for asset {a:?} (see MIN_EDGE_{} etc.)", asset_upper_suffix(a))
            })?;
        }

        Ok(config)
    }

    /// Effective strategy for `asset` (global env + `KEY_{ASSET}` overrides).
    pub fn asset_strategy(&self, asset: &str) -> AssetStrategy {
        let su = asset_upper_suffix(asset);
        AssetStrategy {
            min_edge: env_override_decimal("MIN_EDGE", &su, self.min_edge),
            min_confidence: env_override_decimal("MIN_CONFIDENCE", &su, self.min_confidence),
            min_order_usdc: env_override_decimal("MIN_ORDER_USDC", &su, self.min_order_usdc),
            spot_exchange: env_override_string("SPOT_EXCHANGE", &su, &self.spot_exchange),
            candle_interval: env_override_string("CANDLE_INTERVAL", &su, &self.candle_interval),
            candle_lookback: env_override_usize("CANDLE_LOOKBACK", &su, self.candle_lookback),
            rsi_period: env_override_usize("RSI_PERIOD", &su, self.rsi_period),
            macd_fast: env_override_usize("MACD_FAST", &su, self.macd_fast),
            macd_slow: env_override_usize("MACD_SLOW", &su, self.macd_slow),
            macd_signal: env_override_usize("MACD_SIGNAL", &su, self.macd_signal),
            volume_min_ratio: env_vol_ratio_opt("VOLUME_MIN_RATIO", &su)
                .or(self.volume_min_ratio),
            volume_avg_bars: env_override_usize("VOLUME_AVG_BARS", &su, self.volume_avg_bars),
            max_position_pct: env_override_decimal("MAX_POSITION_PCT", &su, self.max_position_pct),
            daily_loss_limit_pct: env_override_decimal(
                "DAILY_LOSS_LIMIT_PCT",
                &su,
                self.daily_loss_limit_pct,
            ),
            volatility_filter: VolatilityFilterConfig {
                min_std_pct: env_vol_std_opt("VOL_MIN_STD_PCT", &su),
                max_std_pct: env_vol_std_opt("VOL_MAX_STD_PCT", &su),
                sample_bars: {
                    let g = std::env::var("VOL_SAMPLE_BARS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(20);
                    env_override_usize("VOL_SAMPLE_BARS", &su, g)
                },
            },
        }
    }

    pub fn ta_params(&self) -> TaParams {
        TaParams {
            rsi_period: self.rsi_period,
            macd_fast: self.macd_fast,
            macd_slow: self.macd_slow,
            macd_signal: self.macd_signal,
        }
    }

    /// Validate all configuration parameters for business logic correctness
    pub fn validate(&self) -> Result<()> {
        // Trading parameter validation
        if self.min_edge <= Decimal::ZERO {
            anyhow::bail!("MIN_EDGE must be positive, got: {}", self.min_edge);
        }

        if self.min_edge > dec!(0.50) {
            anyhow::bail!("MIN_EDGE too high (>50%), unrealistic edge expectation: {}", self.min_edge);
        }

        if self.min_confidence < dec!(0.5) || self.min_confidence > dec!(1.0) {
            anyhow::bail!("MIN_CONFIDENCE must be between 0.5 and 1.0, got: {}", self.min_confidence);
        }

        if self.max_position_pct <= Decimal::ZERO {
            anyhow::bail!("MAX_POSITION_PCT must be positive, got: {}", self.max_position_pct);
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

        if self.initial_balance < dec!(50) {
            anyhow::bail!(
                "INITIAL_BALANCE too low for meaningful trading (min $50), got: {}",
                self.initial_balance
            );
        }

        if self.min_order_usdc < dec!(1) {
            anyhow::bail!("MIN_ORDER_USDC too low (min $1), got: {}", self.min_order_usdc);
        }

        // Technical indicator validation
        if self.rsi_period < 5 || self.rsi_period > 50 {
            anyhow::bail!("RSI_PERIOD must be between 5 and 50, got: {}", self.rsi_period);
        }

        if self.macd_fast >= self.macd_slow {
            anyhow::bail!(
                "MACD_FAST ({}) must be less than MACD_SLOW ({})",
                self.macd_fast,
                self.macd_slow
            );
        }

        if self.candle_lookback < 50 {
            anyhow::bail!(
                "CANDLE_LOOKBACK too low for reliable signals (min 50), got: {}",
                self.candle_lookback
            );
        }

        if let Some(r) = self.volume_min_ratio {
            if r <= 0.0 || r > 5.0 {
                anyhow::bail!("VOLUME_MIN_RATIO must be in (0, 5], got: {}", r);
            }
        }
        if self.volume_avg_bars < 5 || self.volume_avg_bars > 200 {
            anyhow::bail!(
                "VOLUME_AVG_BARS must be between 5 and 200, got: {}",
                self.volume_avg_bars
            );
        }

        // Asset validation
        if self.assets.is_empty() {
            anyhow::bail!("ASSETS cannot be empty");
        }

        if self.durations.is_empty() {
            anyhow::bail!("DURATIONS cannot be empty");
        }

        // Polymarket specific validation
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
            durations: vec!["5m".to_string()],
            min_edge: dec!(0.06),
            min_confidence: dec!(0.70),
            min_order_usdc: dec!(5),
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
            gamma_tag_id: GAMMA_TAG_ID_DEFAULT,
            clob_host: "https://clob.polymarket.com".to_string(),
            chain_id: POLYGON,
            signature_type: SignatureType::Eoa,
            funder_address: None,
            builder_api_key: None,
            builder_api_secret: None,
            builder_api_passphrase: None,
        }
    }
}

impl SignatureType {
    /// Convert to Polymarket SDK signature type
    pub fn to_sdk_type(&self) -> polymarket_client_sdk::clob::types::SignatureType {
        match self {
            SignatureType::Eoa => polymarket_client_sdk::clob::types::SignatureType::Eoa,
            SignatureType::Proxy => polymarket_client_sdk::clob::types::SignatureType::Proxy,
            SignatureType::GnosisSafe => polymarket_client_sdk::clob::types::SignatureType::GnosisSafe,
        }
    }
}
