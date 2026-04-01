use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Active Polymarket prediction market.
#[derive(Debug, Clone)]
pub struct Market {
    pub condition_id: String,
    pub question: String,
    pub asset: String,       // "btc" | "eth"
    pub duration: String,    // "5m" | "15m"
    pub yes_price: Decimal,
    pub no_price: Decimal,
    pub end_date_ms: i64,
    pub liquidity: Decimal,
}

impl Market {
    pub fn secs_to_close(&self) -> i64 {
        let now_ms = chrono::Utc::now().timestamp_millis();
        ((self.end_date_ms - now_ms) / 1000).max(0)
    }
}

/// Trade direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Yes, // BUY YES token
    No,  // BUY NO token
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
