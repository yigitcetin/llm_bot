use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// ── Trading Parameters ────────────────────────────────────────────────────────

/// Default minimum liquidity threshold for markets (in USDC). Override via `min_liquidity` in `config.toml` or `MIN_LIQUIDITY_USDC`.
pub const MIN_LIQUIDITY_USDC: Decimal = dec!(8000);

/// Minimum time until market close (in seconds)
pub const MIN_MARKET_CLOSE_TIME_SECS: i64 = 90;

/// Minimum number of candles required for reliable signal generation
pub const MIN_CANDLES_FOR_SIGNAL: usize = 100;

/// Slippage protection in basis points (20 bps = 0.2%)
pub const SLIPPAGE_BPS: Decimal = dec!(0.002);

// ── API Endpoints ─────────────────────────────────────────────────────────────

/// Gamma API base URL
pub const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";

/// Default Gamma tag for up/down intraday markets (Polymarket tag id).
pub const GAMMA_TAG_ID_DEFAULT: u64 = 102_127;

/// Max events per Gamma `/events` request when filtering by `tag_id`.
pub const GAMMA_EVENTS_FETCH_LIMIT: u32 = 200;

/// Binance API base URL
pub const BINANCE_API_BASE: &str = "https://api.binance.com/api/v3";

/// Max klines per `/klines` request (Binance API limit).
pub const BINANCE_KLINES_MAX: usize = 1000;

// ── Retry Configuration ───────────────────────────────────────────────────────

/// Default maximum retry attempts for HTTP requests
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// Base backoff duration for retry logic (in milliseconds)
pub const RETRY_BACKOFF_BASE_MS: u64 = 1000;

// ── Cache Configuration ───────────────────────────────────────────────────────

/// Indicator cache max age in seconds (5 minutes)
pub const INDICATOR_CACHE_MAX_AGE_SECS: i64 = 300;
