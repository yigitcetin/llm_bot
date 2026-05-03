use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// Active Polymarket prediction market.
#[derive(Debug, Clone)]
pub struct Market {
    pub condition_id: String,
    pub question: String,
    pub asset: String,    // "btc" | "eth"
    pub duration: String, // "5m" | "15m"
    pub yes_price: Decimal,
    pub no_price: Decimal,
    pub end_date_ms: i64,
    pub liquidity: Decimal,
    /// CLOB outcome token id for YES/UP (from Gamma `clobTokenIds`, aligned with outcomes).
    pub yes_token_id: String,
    /// CLOB outcome token id for NO/DOWN.
    pub no_token_id: String,
}

impl Market {
    pub fn secs_to_close(&self) -> i64 {
        let now_ms = chrono::Utc::now().timestamp_millis();
        ((self.end_date_ms - now_ms) / 1000).max(0)
    }
}

/// Trade direction (`YES` / `NO` in TOML and env).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Direction {
    Yes, // BUY YES token
    No,  // BUY NO token
}

impl Direction {
    /// Canonical JSON / log token (`YES` / `NO`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Yes => "YES",
            Direction::No => "NO",
        }
    }
}

/// Open position tracked in [`crate::risk::RiskManager`] and resolved by [`crate::resolution_checker::ResolutionChecker`].
#[derive(Debug, Clone)]
pub struct OpenPosition {
    pub condition_id: String,
    pub order_id: String,
    pub direction: Direction,
    pub entry_price: Decimal,
    pub size_usdc: Decimal,
    pub size_shares: Decimal,
    /// Market close time (ms).
    pub end_date_ms: i64,
}

impl OpenPosition {
    /// PnL in USDC when the market resolves (`yes_won`: YES/UP token pays out).
    pub fn pnl_on_resolution(&self, yes_won: bool) -> Decimal {
        let bought_yes = self.direction == Direction::Yes;
        let won = (bought_yes && yes_won) || (!bought_yes && !yes_won);

        if won {
            (dec!(1) - self.entry_price) * self.size_shares
        } else {
            -self.size_usdc
        }
    }
}

/// Result of edge calculation — only produced when edge is large enough.
#[derive(Debug, Clone)]
pub struct TradeSignal {
    pub direction: Direction,
    /// Absolute edge: |technical_probability - market_price|
    pub edge: Decimal,
    /// The token price we'll pay
    pub token_price: Decimal,
}
