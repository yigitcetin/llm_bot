//! One full scan–analyze–execute cycle (live trading loop). Used by the binary `main`.

use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
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
use crate::signal_extensions::{
    apply_market_timing_to_signal, below_min_secs_to_close, parse_duration_to_secs,
};
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
        let mut htf_aligned: Option<bool> = None;
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

        if let Some(lo) = st.min_market_yes_price {
            if market.yes_price < lo {
                log_skip_decision(
                    logger,
                    &market,
                    "market_yes_price_out_of_band",
                    Some(format!(
                        "yes_price={}, min_yes_price={}",
                        market.yes_price, lo
                    )),
                );
                info!(
                    condition_id = %market.condition_id,
                    yes_price = %market.yes_price,
                    "skip: YES price below minimum band"
                );
                continue;
            }
        }
        if let Some(hi) = st.max_market_yes_price {
            if market.yes_price > hi {
                log_skip_decision(
                    logger,
                    &market,
                    "market_yes_price_out_of_band",
                    Some(format!(
                        "yes_price={}, max_yes_price={}",
                        market.yes_price, hi
                    )),
                );
                info!(
                    condition_id = %market.condition_id,
                    yes_price = %market.yes_price,
                    "skip: YES price above maximum band"
                );
                continue;
            }
        }

        if below_min_secs_to_close(&market, st.min_secs_to_close) {
            log_skip_decision(
                logger,
                &market,
                "too_close_to_expiry",
                st.min_secs_to_close
                    .map(|m| format!("secs_to_close={}, min_secs={}", market.secs_to_close(), m)),
            );
            info!(
                condition_id = %market.condition_id,
                "skip: too close to market expiry"
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

        let signal_arc = match indicator_cache.get_or_compute(
            &market.asset,
            &st.candle_interval,
            &candles,
            &signal_config,
        ) {
            Ok(s) => s,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("volume below") {
                    let vol_slice: &[crate::spot_price::Candle] =
                        if signal_config.volume_use_closed_candle_only && candles.len() > 1 {
                            &candles[..candles.len() - 1]
                        } else {
                            &candles
                        };
                    let vr = compute_volume_ratio(vol_slice, signal_config.volume_avg_bars.max(5));
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

        let window_secs = parse_duration_to_secs(&market.duration);
        let mut signal = (*signal_arc).clone();
        signal = apply_market_timing_to_signal(
            signal,
            &market,
            window_secs,
            st.expiry_dampen_last_secs,
        );

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

        let volatility_std_pct =
            compute_return_std_pct(&candles, st.volatility_filter.sample_bars);

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
                    htf_aligned = Some(true);
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

        let direction = match market_matcher::match_signal_to_market(&signal, &market) {
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
        let sizing = edge::kelly_size_with_caps_detail(
            trade.edge,
            signal.confidence,
            balance,
            st.max_position_pct,
            st.min_order_usdc,
            trade.token_price,
            st.cheap_token_price_threshold,
            st.cheap_token_max_usdc,
            st.large_order_usdc_hard_cap,
        );
        let size_usdc = sizing.size_usdc;

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

                let mut record = TradeRecord::new(
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
                record.rsi = Some(signal.rsi);
                record.macd_histogram = Some(signal.macd_histogram);
                record.volume_ratio = Some(signal.volume_ratio);
                record.cluster_direction = Some(signal.cluster_direction.clone());
                record.market_yes_price = Some(market.yes_price.to_string());
                record.liquidity = Some(market.liquidity.to_string());
                record.secs_to_close = Some(market.secs_to_close());
                record.volatility_std_pct = volatility_std_pct.and_then(|d| d.to_f64());
                record.kelly_fraction = Some(sizing.kelly_fraction.to_string());
                record.balance_at_trade = Some(balance.to_string());
                record.daily_loss_at_trade = Some(risk.daily_loss().to_string());
                record.htf_aligned = htf_aligned;
                record.adaptive_min_edge = st
                    .adaptive_thresholds
                    .then(|| eff_min_edge.to_string());
                record.adaptive_min_confidence = st
                    .adaptive_thresholds
                    .then(|| eff_min_confidence.to_string());
                record.sizing_cap_hit = Some(sizing.cap_hit.to_string());
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
