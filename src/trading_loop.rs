//! One full scan–analyze–execute cycle (live trading loop). Used by the binary `main`.

use anyhow::Result;
use tracing::info;

use crate::config::AppConfig;
use crate::constants::MIN_LIQUIDITY_USDC;
use crate::edge;
use crate::execution::Executor;
use crate::gamma::GammaClient;
use crate::indicator_cache::IndicatorCache;
use crate::market_matcher;
use crate::prometheus_export;
use crate::risk::RiskManager;
use crate::spot_price::SpotPriceClient;
use crate::volatility::passes_volatility_filter;

/// One full scan-analyze-execute cycle.
pub async fn run_cycle(
    cfg: &AppConfig,
    gamma: &GammaClient,
    spot: &SpotPriceClient,
    executor: &Executor,
    risk: &mut RiskManager,
    indicator_cache: &mut IndicatorCache,
) -> Result<()> {
    // 1. Fetch active BTC/ETH 5m markets
    let markets = gamma.active_markets(&cfg.assets, &cfg.durations).await?;
    prometheus_export::add_markets_scanned(markets.len() as u64);
    info!(count = markets.len(), "markets fetched");

    for market in markets {
        let st = cfg.asset_strategy(&market.asset);
        let signal_config = st.signal_config();

        // Skip if already have a position in this market
        if risk.has_position(&market.condition_id) {
            continue;
        }

        // Skip low-liquidity markets
        if market.liquidity < MIN_LIQUIDITY_USDC {
            tracing::debug!(
                condition_id = %market.condition_id,
                liquidity = %market.liquidity,
                "liquidity too low — skipping"
            );
            continue;
        }

        // 2. Fetch spot price candles
        let candles = spot
            .fetch_candles_at_exchange(
                &market.asset,
                &st.candle_interval,
                st.candle_lookback,
                &st.spot_exchange,
            )
            .await?;

        if candles.len() < 100 {
            tracing::debug!(
                condition_id = %market.condition_id,
                asset = %market.asset,
                candle_count = candles.len(),
                "not enough candles — skipping"
            );
            continue;
        }

        // 3. Generate technical signal (with caching)
        let signal = match indicator_cache.get_or_compute(
            &market.asset,
            &st.candle_interval,
            &candles,
            &signal_config,
        ) {
            Ok(s) => s,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("volume below") {
                    tracing::debug!(
                        condition_id = %market.condition_id,
                        "spot volume below VOLUME_MIN_RATIO — skipping"
                    );
                } else {
                    tracing::debug!(
                        condition_id = %market.condition_id,
                        error = %e,
                        "signal generation failed — skipping"
                    );
                }
                continue;
            }
        };

        if !passes_volatility_filter(&candles, &st.volatility_filter) {
            tracing::debug!(
                condition_id = %market.condition_id,
                "volatility regime filter — skipping"
            );
            continue;
        }

        if signal.confidence < st.min_confidence {
            tracing::debug!(
                condition_id = %market.condition_id,
                confidence = %signal.confidence,
                threshold = %st.min_confidence,
                "signal: confidence too low — skipping"
            );
            continue;
        }

        // 4. Match signal to market question
        let direction = match market_matcher::match_signal_to_market(signal.as_ref(), &market) {
            Some(dir) => dir,
            None => {
                tracing::debug!(
                    condition_id = %market.condition_id,
                    question = %market.question,
                    "cannot parse market question — skipping"
                );
                continue;
            }
        };

        // 5. Edge calculation
        let edge_result = edge::calculate(
            signal.probability,
            market.yes_price,
            st.min_edge,
        );

        let Some(mut trade) = edge_result else {
            tracing::debug!(
                condition_id = %market.condition_id,
                signal_prob = %signal.probability,
                market_price = %market.yes_price,
                "edge too small — skipping"
            );
            continue;
        };

        // Override trade direction with matched direction
        trade.direction = direction;

        // 6. Position sizing (half-Kelly)
        let balance = risk.available_balance();
        let size_usdc = edge::kelly_size(
            trade.edge,
            signal.confidence,
            balance,
            st.max_position_pct,
        );

        if size_usdc < st.min_order_usdc {
            continue;
        }

        // 7. Risk check
        if !risk.can_trade(size_usdc, &market.condition_id, st.max_position_pct) {
            tracing::warn!(
                condition_id = %market.condition_id,
                "risk manager blocked trade"
            );
            continue;
        }

        // 8. Execute
        info!(
            condition_id = %market.condition_id,
            asset = %market.asset,
            direction = ?trade.direction,
            signal_prob = %signal.probability,
            market_price = %market.yes_price,
            edge = %trade.edge,
            size_usdc = %size_usdc,
            confidence = %signal.confidence,
            reasoning = %signal.reasoning,
            "placing order"
        );

        match executor.place_order(&market, &trade, size_usdc).await {
            Ok(order_id) => {
                info!(
                    order_id = %order_id,
                    condition_id = %market.condition_id,
                    "order placed successfully"
                );
                if !executor.is_dry_run() {
                    prometheus_export::record_trade_success();
                }
                risk.record_trade(size_usdc, market.condition_id.clone());
            }
            Err(e) => {
                prometheus_export::record_order_failure();
                tracing::error!(
                    error = %e,
                    condition_id = %market.condition_id,
                    "order placement failed"
                );
            }
        }
    }

    Ok(())
}
