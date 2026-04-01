//! One full scan–analyze–execute cycle (live trading loop). Used by the binary `main`.

use anyhow::Result;
use rust_decimal::Decimal;
use tracing::info;

use crate::config::AppConfig;
use crate::constants::MIN_LIQUIDITY_USDC;
use crate::edge;
use crate::execution::Executor;
use crate::gamma::GammaClient;
use crate::indicator_cache::IndicatorCache;
use crate::market_matcher;
use crate::metrics::{MetricsLogger, SkipRecord, TradeRecord};
use crate::prometheus_export;
use crate::resolution_checker::OpenPosition;
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
    let metrics_logger = MetricsLogger::new("data").ok();

    // 1. Fetch active markets
    let markets = gamma.active_markets(&cfg.assets, &cfg.durations).await?;
    prometheus_export::add_markets_scanned(markets.len() as u64);
    info!(count = markets.len(), "markets fetched");

    for market in markets {
        let st = cfg.asset_strategy(&market.asset);
        let signal_config = st.signal_config();

        // Skip if already have a position in this market
        if risk.has_position(&market.condition_id) {
            if let Some(logger) = &metrics_logger {
                let _ = logger.log_skip(&SkipRecord::new(
                    market.condition_id.clone(),
                    market.asset.clone(),
                    market.duration.clone(),
                    market.question.clone(),
                    "already_have_open_position",
                    None,
                ));
            }
            info!(
                condition_id = %market.condition_id,
                question = %market.question,
                "skip: already have open position"
            );
            continue;
        }

        // Skip low-liquidity markets
        if market.liquidity < MIN_LIQUIDITY_USDC {
            if let Some(logger) = &metrics_logger {
                let _ = logger.log_skip(&SkipRecord::new(
                    market.condition_id.clone(),
                    market.asset.clone(),
                    market.duration.clone(),
                    market.question.clone(),
                    "liquidity_too_low",
                    Some(format!("liquidity={}, min={}", market.liquidity, MIN_LIQUIDITY_USDC)),
                ));
            }
            info!(
                condition_id = %market.condition_id,
                question = %market.question,
                liquidity = %market.liquidity,
                min_liquidity = %MIN_LIQUIDITY_USDC,
                "skip: liquidity too low"
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
            if let Some(logger) = &metrics_logger {
                let _ = logger.log_skip(&SkipRecord::new(
                    market.condition_id.clone(),
                    market.asset.clone(),
                    market.duration.clone(),
                    market.question.clone(),
                    "not_enough_candles",
                    Some(format!("candle_count={}", candles.len())),
                ));
            }
            info!(
                condition_id = %market.condition_id,
                question = %market.question,
                asset = %market.asset,
                candle_count = candles.len(),
                "skip: not enough candles"
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
                    if let Some(logger) = &metrics_logger {
                        let _ = logger.log_skip(&SkipRecord::new(
                            market.condition_id.clone(),
                            market.asset.clone(),
                            market.duration.clone(),
                            market.question.clone(),
                            "spot_volume_below_threshold",
                            None,
                        ));
                    }
                    info!(
                        condition_id = %market.condition_id,
                        question = %market.question,
                        "skip: spot volume below VOLUME_MIN_RATIO"
                    );
                } else {
                    if let Some(logger) = &metrics_logger {
                        let _ = logger.log_skip(&SkipRecord::new(
                            market.condition_id.clone(),
                            market.asset.clone(),
                            market.duration.clone(),
                            market.question.clone(),
                            "signal_generation_failed",
                            Some(e.to_string()),
                        ));
                    }
                    info!(
                        condition_id = %market.condition_id,
                        question = %market.question,
                        error = %e,
                        "skip: signal generation failed"
                    );
                }
                continue;
            }
        };

        if !passes_volatility_filter(&candles, &st.volatility_filter) {
            if let Some(logger) = &metrics_logger {
                let _ = logger.log_skip(&SkipRecord::new(
                    market.condition_id.clone(),
                    market.asset.clone(),
                    market.duration.clone(),
                    market.question.clone(),
                    "volatility_filter",
                    None,
                ));
            }
            info!(
                condition_id = %market.condition_id,
                question = %market.question,
                "skip: volatility regime filter"
            );
            continue;
        }

        if signal.confidence < st.min_confidence {
            if let Some(logger) = &metrics_logger {
                let _ = logger.log_skip(&SkipRecord::new(
                    market.condition_id.clone(),
                    market.asset.clone(),
                    market.duration.clone(),
                    market.question.clone(),
                    "confidence_too_low",
                    Some(format!("confidence={}, threshold={}", signal.confidence, st.min_confidence)),
                ));
            }
            info!(
                "skip: signal confidence too low (confidence={}, threshold={})",
                signal.confidence,
                st.min_confidence
            );
            continue;
        }

        // 4. Match signal to market question
        let direction = match market_matcher::match_signal_to_market(signal.as_ref(), &market) {
            Some(dir) => dir,
            None => {
                if let Some(logger) = &metrics_logger {
                    let _ = logger.log_skip(&SkipRecord::new(
                        market.condition_id.clone(),
                        market.asset.clone(),
                        market.duration.clone(),
                        market.question.clone(),
                        "cannot_match_market_question",
                        None,
                    ));
                }
                info!(
                    condition_id = %market.condition_id,
                    question = %market.question,
                    "skip: cannot match signal to market question"
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
            if let Some(logger) = &metrics_logger {
                let _ = logger.log_skip(&SkipRecord::new(
                    market.condition_id.clone(),
                    market.asset.clone(),
                    market.duration.clone(),
                    market.question.clone(),
                    "edge_too_small",
                    Some(format!(
                        "signal_prob={}, market_yes_price={}, min_edge={}",
                        signal.probability, market.yes_price, st.min_edge
                    )),
                ));
            }
            info!(
                condition_id = %market.condition_id,
                question = %market.question,
                signal_prob = %signal.probability,
                market_price = %market.yes_price,
                threshold = %st.min_edge,
                "skip: edge too small"
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
            st.min_order_usdc,
        );

        if size_usdc < st.min_order_usdc {
            if let Some(logger) = &metrics_logger {
                let _ = logger.log_skip(&SkipRecord::new(
                    market.condition_id.clone(),
                    market.asset.clone(),
                    market.duration.clone(),
                    market.question.clone(),
                    "order_size_below_minimum",
                    Some(format!("size_usdc={}, min_order_usdc={}", size_usdc, st.min_order_usdc)),
                ));
            }
            info!(
                condition_id = %market.condition_id,
                question = %market.question,
                size_usdc = %size_usdc,
                min_order_usdc = %st.min_order_usdc,
                "skip: order size below minimum"
            );
            continue;
        }

        // 7. Risk check
        if !risk.can_trade(size_usdc, &market.condition_id, st.max_position_pct) {
            if let Some(logger) = &metrics_logger {
                let _ = logger.log_skip(&SkipRecord::new(
                    market.condition_id.clone(),
                    market.asset.clone(),
                    market.duration.clone(),
                    market.question.clone(),
                    "risk_manager_blocked_trade",
                    Some(format!("size_usdc={}, max_position_pct={}", size_usdc, st.max_position_pct)),
                ));
            }
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
                let size_shares = if trade.token_price > Decimal::ZERO {
                    size_usdc / trade.token_price
                } else {
                    Decimal::ZERO
                };

                if let Some(logger) = &metrics_logger {
                    let record = TradeRecord::new(
                        market.condition_id.clone(),
                        market.asset.clone(),
                        market.duration.clone(),
                        trade.direction,
                        trade.token_price,
                        size_usdc,
                        size_shares,
                        signal.probability,
                        signal.confidence,
                        trade.edge,
                        String::new(),
                        signal.reasoning.clone(),
                        order_id.clone(),
                    );
                    let _ = logger.log_trade(&record);
                }

                info!(
                    order_id = %order_id,
                    condition_id = %market.condition_id,
                    "order placed successfully"
                );

                if !executor.is_dry_run() {
                    prometheus_export::record_trade_success();
                }

                // OpenPosition oluştur ve RiskManager'a kaydet
                let position = OpenPosition {
                    condition_id: market.condition_id.clone(),
                    order_id: order_id.clone(),
                    direction: match trade.direction {
                        crate::types::Direction::Yes => "YES".to_string(),
                        crate::types::Direction::No => "NO".to_string(),
                    },
                    entry_price: trade.token_price,
                    size_usdc,
                    size_shares,
                    end_date_ms: market.end_date_ms,
                };
                risk.record_trade(size_usdc, position);
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