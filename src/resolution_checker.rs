//! Resolution checker: monitors open trades and resolves them when markets close.
//!
//! Her cycle'da açık pozisyonları kontrol eder, market kapandıysa Gamma API'den
//! sonucu çeker ve RiskManager + resolutions.jsonl'yi günceller.

use anyhow::Result;
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use tracing::{info, warn};

use crate::metrics::{MetricsLogger, ResolutionRecord};
use crate::risk::RiskManager;

/// Açık bir pozisyonun takibi için gerekli bilgiler.
#[derive(Debug, Clone)]
pub struct OpenPosition {
    pub condition_id: String,
    pub order_id: String,
    pub direction: String,   // "YES" | "NO"
    pub entry_price: Decimal,
    pub size_usdc: Decimal,
    pub size_shares: Decimal,
    pub end_date_ms: i64,    // market kapanma zamanı (ms)
}

/// Gamma API'den gelen market sonucu.
#[derive(Debug, Deserialize)]
struct GammaMarketResult {
    #[serde(rename = "conditionId")]
    condition_id: Option<String>,
    pub closed: Option<bool>,
    pub archived: Option<bool>,
    #[serde(rename = "resolutionPrice")]
    resolution_price: Option<String>,
    pub outcomes: Option<String>,
    #[serde(rename = "outcomePrices")]
    outcome_prices: Option<String>,
    pub tokens: Option<Vec<GammaToken>>,
}

#[derive(Debug, Deserialize)]
struct GammaToken {
    pub outcome: String,
    pub price: String,
}

/// Resolution checker: açık pozisyonları takip eder ve çözümler.
pub struct ResolutionChecker {
    http: reqwest::Client,
    gamma_api_base: String,
}

impl ResolutionChecker {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            gamma_api_base: "https://gamma-api.polymarket.com".to_string(),
        }
    }

    /// Açık pozisyonları kontrol et, kapananları çöz.
    ///
    /// `open_positions`: RiskManager'daki açık pozisyonların listesi.
    /// Her cycle sonunda çağrılmalı.
    pub async fn check_and_resolve(
        &self,
        open_positions: &[OpenPosition],
        risk: &mut RiskManager,
        logger: &MetricsLogger,
    ) -> Result<()> {
        let now_ms = Utc::now().timestamp_millis();

        for pos in open_positions {
            // Market henüz kapanmadıysa atla (30 saniyelik buffer ekle)
            if pos.end_date_ms > now_ms + 30_000 {
                continue;
            }

            info!(
                condition_id = %pos.condition_id,
                "checking resolution for closed market"
            );

            match self.fetch_market_result(&pos.condition_id).await {
                Ok(Some(yes_won)) => {
                    let pnl = calculate_pnl(pos, yes_won);

                    risk.record_resolution(pos, pnl);

                    // resolutions.jsonl'e yaz
                    let record = ResolutionRecord {
                        timestamp: Utc::now(),
                        condition_id: pos.condition_id.clone(),
                        order_id: pos.order_id.clone(),
                        outcome: yes_won,
                        pnl: pnl.to_string(),
                    };

                    if let Err(e) = logger.log_resolution(&record) {
                        warn!(error = %e, "failed to log resolution");
                    }

                    info!(
                        condition_id = %pos.condition_id,
                        yes_won = yes_won,
                        pnl = %pnl,
                        "position resolved"
                    );
                }
                Ok(None) => {
                    // Sonuç henüz belli değil, bir sonraki cycle'da tekrar dene
                    info!(
                        condition_id = %pos.condition_id,
                        "market closed but result not yet available, retrying next cycle"
                    );
                }
                Err(e) => {
                    warn!(
                        condition_id = %pos.condition_id,
                        error = %e,
                        "failed to fetch market result"
                    );
                }
            }
        }

        Ok(())
    }

    /// Gamma API'den market sonucunu çek.
    /// `Some(true)` = YES kazandı, `Some(false)` = NO kazandı, `None` = henüz belli değil.
    async fn fetch_market_result(&self, condition_id: &str) -> Result<Option<bool>> {
        let url = format!("{}/markets/{}", self.gamma_api_base, condition_id);

        let resp = self
            .http
            .get(&url)
            .send()
            .await?;

        if !resp.status().is_success() {
            warn!(
                condition_id = %condition_id,
                status = %resp.status(),
                "gamma API returned non-success for market result"
            );
            return Ok(None);
        }

        let market: GammaMarketResult = resp.json().await?;

        // Market kapanmamışsa henüz sonuç yok
        if !market.closed.unwrap_or(false) {
            return Ok(None);
        }

        // resolutionPrice varsa direkt kullan (1.0 = YES, 0.0 = NO)
        if let Some(price_str) = &market.resolution_price {
            if let Ok(price) = price_str.parse::<Decimal>() {
                return Ok(Some(price >= dec!(0.5)));
            }
        }

        // Token fiyatlarına bak — kazanan token 1.0'a yakın olur
        if let Some(tokens) = &market.tokens {
            for token in tokens {
                if let Ok(price) = token.price.parse::<Decimal>() {
                    if price >= dec!(0.99) {
                        let yes_won = matches!(
                            token.outcome.to_uppercase().as_str(),
                            "YES" | "UP"
                        );
                        return Ok(Some(yes_won));
                    }
                }
            }
        }

        // outcomePrices formatına bak
        if let (Some(outcomes_raw), Some(prices_raw)) =
            (&market.outcomes, &market.outcome_prices)
        {
            if let (Ok(outcomes), Ok(prices)) = (
                serde_json::from_str::<Vec<String>>(outcomes_raw),
                serde_json::from_str::<Vec<String>>(prices_raw),
            ) {
                for (outcome, price_str) in outcomes.iter().zip(prices.iter()) {
                    if let Ok(price) = price_str.parse::<Decimal>() {
                        if price >= dec!(0.99) {
                            let yes_won = matches!(
                                outcome.to_uppercase().as_str(),
                                "YES" | "UP"
                            );
                            return Ok(Some(yes_won));
                        }
                    }
                }
            }
        }

        // Henüz resolve edilmemiş
        Ok(None)
    }
}

/// PnL hesapla.
///
/// YES aldıysan ve YES kazandıysa: (1.0 - entry_price) * shares
/// YES aldıysan ve NO kazandıysa: -size_usdc (tüm pozisyonu kaybettik)
/// NO aldıysan ve NO kazandıysa: (1.0 - entry_price) * shares
/// NO aldıysan ve YES kazandıysa: -size_usdc
fn calculate_pnl(pos: &OpenPosition, yes_won: bool) -> Decimal {
    let bought_yes = pos.direction.to_uppercase() == "YES";
    let won = (bought_yes && yes_won) || (!bought_yes && !yes_won);

    if won {
        // Kazanç: (1 - entry_price) * shares
        (dec!(1) - pos.entry_price) * pos.size_shares
    } else {
        // Kayıp: tüm pozisyon
        -pos.size_usdc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn test_position(direction: &str) -> OpenPosition {
        OpenPosition {
            condition_id: "0xtest".to_string(),
            order_id: "order-1".to_string(),
            direction: direction.to_string(),
            entry_price: dec!(0.40),
            size_usdc: dec!(5),
            size_shares: dec!(12.5),  // 5 / 0.40
            end_date_ms: 0,
        }
    }

    #[test]
    fn yes_wins_bought_yes() {
        let pos = test_position("YES");
        let pnl = calculate_pnl(&pos, true);
        // (1 - 0.40) * 12.5 = 0.60 * 12.5 = 7.50
        assert_eq!(pnl, dec!(7.50));
    }

    #[test]
    fn yes_wins_bought_no() {
        let pos = test_position("NO");
        let pnl = calculate_pnl(&pos, true);
        assert_eq!(pnl, dec!(-5));
    }

    #[test]
    fn no_wins_bought_no() {
        let pos = test_position("NO");
        let pnl = calculate_pnl(&pos, false);
        // (1 - 0.40) * 12.5 = 7.50
        assert_eq!(pnl, dec!(7.50));
    }

    #[test]
    fn no_wins_bought_yes() {
        let pos = test_position("YES");
        let pnl = calculate_pnl(&pos, false);
        assert_eq!(pnl, dec!(-5));
    }
}