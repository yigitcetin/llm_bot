//! One full scan–analyze–execute cycle (live trading loop). Used by the binary `main`.

use anyhow::Result;
use rust_decimal::Decimal;
use tracing::info;

use crate::adaptive;
use crate::config::AppConfig;
use crate::constants::{MIN_CANDLES_FOR_SIGNAL, MIN_LIQUIDITY_USDC};
use crate::edge;
use crate::execution::Executor;
use crate::gamma::GammaClient;
use crate::indicator_cache::IndicatorCache;
use crate::market_matcher;
use crate::metrics::{MetricsLogger, OrderFailureRecord, SkipRecord, TradeRecord};
use crate::prometheus_export;
use crate::risk::{RiskManager, TradeBlockReason};
use crate::signals::{compute_volume_ratio, higher_timeframe_aligns};
use crate::spot_price::SpotPriceClient;
use crate::types::{Market, OpenPosition};
use crate::volatility::{compute_return_std_pct, passes_volatility_filter};

fn log_skip_decision(
    logger: &MetricsLogger,
    market: &Market,
    reason: &'static str,
    details: Option<String>,
) {
    let _ = logger.log_skip(&SkipRecord::new(
        market.condition_id.clone(),
        market.asset.clone(),
        market.duration.clone(),
        market.question.clone(),
        reason,
        details,
    ));
}

/// One full scan-analyze-execute cycle.
pub async fn run_cycle(
    cfg: &AppConfig,
    gamma: &GammaClient,
    spot: &SpotPriceClient,
    executor: &Executor,
    risk: &mut RiskManager,
    indicator_cache: &mut IndicatorCache,
    logger: &MetricsLogger,
) -> Result<()> {
    let markets = gamma.active_markets(&cfg.assets, &cfg.durations).await?;
    prometheus_export::add_markets_scanned(markets.len() as u64);
    info!(count = markets.len(), "markets fetched");

    for market in markets {
        let st = cfg.asset_strategy(&market.asset);
        let signal_config = st.signal_config();

        if risk.has_position(&market.condition_id) {
            log_skip_decision(
                logger,
                &market,
                "already_have_open_position",
                None,
            );
            info!(
                condition_id = %market.condition_id,
                question = %market.question,
                "skip: already have open position"
            );
            continue;
        }

        if market.liquidity < MIN_LIQUIDITY_USDC {
            log_skip_decision(
                logger,
                &market,
                "liquidity_too_low",
                Some(format!("liquidity={}, min={}", market.liquidity, MIN_LIQUIDITY_USDC)),
            );
            info!(
                condition_id = %market.condition_id,
                question = %market.question,
                liquidity = %market.liquidity,
                min_liquidity = %MIN_LIQUIDITY_USDC,
                "skip: liquidity too low"
            );
            continue;
        }

        let candles = spot
            .fetch_candles_at_exchange(
                &market.asset,
                &st.candle_interval,
                st.candle_lookback,
                &st.spot_exchange,
            )
            .await?;

        if candles.len() < MIN_CANDLES_FOR_SIGNAL {
            log_skip_decision(
                logger,
                &market,
                "not_enough_candles",
                Some(format!("candle_count={}", candles.len())),
            );
            info!(
                condition_id = %market.condition_id,
                question = %market.question,
                asset = %market.asset,
                candle_count = candles.len(),
                "skip: not enough candles"
            );
            continue;
        }

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
                    let vr = compute_volume_ratio(&candles, signal_config.volume_avg_bars.max(5));
                    let detail = signal_config
                        .volume_min_ratio
                        .map(|v| format!("volume_ratio={:.4}, min_ratio={:.4}", vr, v))
                        .unwrap_or_else(|| format!("volume_ratio={:.4}", vr));
                    log_skip_decision(
                        logger,
                        &market,
                        "spot_volume_below_threshold",
                        Some(detail),
                    );
                    info!(
                        condition_id = %market.condition_id,
                        question = %market.question,
                        "skip: spot volume below VOLUME_MIN_RATIO"
                    );
                } else {
                    log_skip_decision(
                        logger,
                        &market,
                        "signal_generation_failed",
                        Some(e.to_string()),
                    );
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
            let vol_detail = compute_return_std_pct(&candles, st.volatility_filter.sample_bars)
                .map(|v| {
                    format!(
                        "vol_std_pct={}, sample_bars={}, min={:?}, max={:?}",
                        v,
                        st.volatility_filter.sample_bars,
                        st.volatility_filter.min_std_pct,
                        st.volatility_filter.max_std_pct
                    )
                })
                .unwrap_or_else(|| "vol_std_pct=unknown".to_string());
            log_skip_decision(
                logger,
                &market,
                "volatility_filter",
                Some(vol_detail),
            );
            info!(
                condition_id = %market.condition_id,
                question = %market.question,
                "skip: volatility regime filter"
            );
            continue;
        }

        if st.htf_enabled {
            match spot
                .fetch_candles_at_exchange(
                    &market.asset,
                    &st.htf_interval,
                    st.htf_lookback,
                    &st.spot_exchange,
                )
                .await
            {
                Ok(htf) => {
                    if !higher_timeframe_aligns(signal.direction, &htf, st.htf_ema_period) {
                        log_skip_decision(
                            logger,
                            &market,
                            "htf_trend_mismatch",
                            Some(format!(
                                "htf_interval={}, ema_period={}, lookback={}",
                                st.htf_interval, st.htf_ema_period, st.htf_lookback
                            )),
                        );
                        info!(
                            condition_id = %market.condition_id,
                            direction = ?signal.direction,
                            "skip: higher timeframe trend mismatch"
                        );
                        continue;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        asset = %market.asset,
                        "HTF candle fetch failed — continuing without HTF filter"
                    );
                }
            }
        }

        let trades_path = format!("{}/trades.jsonl", cfg.data_dir);
        let (eff_min_edge, eff_min_confidence) = adaptive::effective_thresholds(
            &trades_path,
            &market.asset,
            st.min_edge,
            st.min_confidence,
            st.adaptive_trade_window,
            st.adaptive_thresholds,
        );

        if signal.confidence < eff_min_confidence {
            log_skip_decision(
                logger,
                &market,
                "confidence_too_low",
                Some(format!(
                    "confidence={}, threshold={} (base={}, adaptive={})",
                    signal.confidence,
                    eff_min_confidence,
                    st.min_confidence,
                    st.adaptive_thresholds
                )),
            );
            info!(
                "skip: signal confidence too low (confidence={}, threshold={})",
                signal.confidence,
                eff_min_confidence
            );
            continue;
        }

        let direction = match market_matcher::match_signal_to_market(signal.as_ref(), &market) {
            Some(dir) => dir,
            None => {
                log_skip_decision(
                    logger,
                    &market,
                    "cannot_match_market_question",
                    None,
                );
                info!(
                    condition_id = %market.condition_id,
                    question = %market.question,
                    "skip: cannot match signal to market question"
                );
                continue;
            }
        };

        let edge_result = edge::calculate(
            signal.probability,
            market.yes_price,
            eff_min_edge,
        );

        let Some(mut trade) = edge_result else {
            log_skip_decision(
                logger,
                &market,
                "edge_too_small",
                Some(format!(
                    "signal_prob={}, market_yes_price={}, min_edge={} (base={}, adaptive={})",
                    signal.probability,
                    market.yes_price,
                    eff_min_edge,
                    st.min_edge,
                    st.adaptive_thresholds
                )),
            );
            info!(
                condition_id = %market.condition_id,
                question = %market.question,
                signal_prob = %signal.probability,
                market_price = %market.yes_price,
                threshold = %eff_min_edge,
                "skip: edge too small"
            );
            continue;
        };

        trade.direction = direction;

        let balance = risk.available_balance();
        let size_usdc = edge::kelly_size(
            trade.edge,
            signal.confidence,
            balance,
            st.max_position_pct,
            st.min_order_usdc,
        );

        if size_usdc < st.min_order_usdc {
            log_skip_decision(
                logger,
                &market,
                "order_size_below_minimum",
                Some(format!("size_usdc={}, min_order_usdc={}", size_usdc, st.min_order_usdc)),
            );
            info!(
                condition_id = %market.condition_id,
                question = %market.question,
                size_usdc = %size_usdc,
                min_order_usdc = %st.min_order_usdc,
                "skip: order size below minimum"
            );
            continue;
        }

        if let Some(reason) =
            risk.trade_block_reason(size_usdc, &market.condition_id, st.max_position_pct)
        {
            log_skip_decision(
                logger,
                &market,
                "risk_manager_blocked_trade",
                Some(format!(
                    "sub_reason={}, size_usdc={}, balance={}, max_position_pct={}, daily_loss_limit_hit={}",
                    reason,
                    size_usdc,
                    risk.available_balance(),
                    st.max_position_pct,
                    matches!(reason, TradeBlockReason::DailyLossLimit),
                )),
            );
            tracing::warn!(
                condition_id = %market.condition_id,
                reason = %reason,
                "risk manager blocked trade"
            );
            continue;
        }

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
                    signal.reasoning.clone(),
                    order_id.clone(),
                );
                let _ = logger.log_trade(&record);

                info!(
                    order_id = %order_id,
                    condition_id = %market.condition_id,
                    "order placed successfully"
                );

                if !executor.is_dry_run() {
                    prometheus_export::record_trade_success();
                }

                let position = OpenPosition {
                    condition_id: market.condition_id.clone(),
                    order_id: order_id.clone(),
                    direction: trade.direction,
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
                let _ = logger.log_order_failure(&OrderFailureRecord::new(
                    market.condition_id.clone(),
                    market.asset.clone(),
                    market.duration.clone(),
                    market.question.clone(),
                    e.to_string(),
                ));
            }
        }
    }

    Ok(())
}
