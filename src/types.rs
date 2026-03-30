use rust_decimal::Decimal;

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

/// A single news article.
#[derive(Debug, Clone)]
pub struct NewsItem {
    pub title: String,
    pub description: Option<String>,
    pub published_at: chrono::DateTime<chrono::Utc>,
    pub source: String,
    pub url: String,
}

/// LLM analysis result for a market.
#[derive(Debug, Clone)]
pub struct LlmSignal {
    /// Estimated probability YES resolves true (0.0 - 1.0)
    pub probability: Decimal,
    /// LLM's confidence in its estimate (0.0 - 1.0)
    pub confidence: Decimal,
    /// Short reasoning string
    pub reasoning: String,
    /// Whether the news is actually relevant to this market
    pub news_relevant: bool,
}

/// Trade direction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Direction {
    Yes, // BUY YES token
    No,  // BUY NO token
}

/// Result of edge calculation — only produced when edge is large enough.
#[derive(Debug, Clone)]
pub struct TradeSignal {
    pub direction: Direction,
    /// Absolute edge: |llm_probability - market_price|
    pub edge: Decimal,
    /// The token price we'll pay
    pub token_price: Decimal,
}
