//! One full scan–analyze–execute cycle (live trading loop). Used by the binary `main`.

use anyhow::Result;
use futures::future::join_all;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::info;

use crate::adaptive;
use crate::config::{AppConfig, AssetStrategy};
use crate::constants::{MIN_CANDLES_FOR_SIGNAL, MIN_LIQUIDITY_USDC};
use crate::edge;
use crate::execution::Executor;
use crate::gamma::GammaClient;
use crate::indicator_cache::IndicatorCache;
use crate::market_matcher;
use crate::metrics::{MetricsLogger, OrderFailureRecord, SkipRecord, TradeRecord};
use crate::order_tracker::{pending_from_outcome, OrderTracker, PendingTradeMeta};
use crate::prometheus_export;
use crate::risk::{RiskManager, TradeBlockReason};
use crate::signal_extensions::{
    above_max_secs_to_close, apply_market_timing_to_signal, below_min_secs_to_close,
    parse_duration_to_secs,
};
use crate::signals::{compute_volume_ratio, higher_timeframe_aligns};
use crate::spot_price::SpotPriceClient;
use crate::types::{Market, OpenPosition};
use crate::volatility::{compute_return_std_pct, passes_volatility_filter};
use polymarket_client_sdk::clob::types::OrderStatusType;

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

/// Cheap filters before any Binance HTTP (liquidity, price band, time-to-close, open position).
/// Logs and returns `false` when the market should be skipped.
fn passes_pre_candle_filters(
    market: &Market,
    st: &AssetStrategy,
    risk: &RiskManager,
    logger: &MetricsLogger,
) -> bool {
    if risk.has_open_or_reserved(&market.condition_id) {
        log_skip_decision(logger, market, "already_have_open_position", None);
        info!(
            condition_id = %market.condition_id,
            question = %market.question,
            "skip: already have open position or pending order"
        );
        return false;
    }

    if market.liquidity < MIN_LIQUIDITY_USDC {
        log_skip_decision(
            logger,
            market,
            "liquidity_too_low",
            Some(format!(
                "liquidity={}, min={}",
                market.liquidity, MIN_LIQUIDITY_USDC
            )),
        );
        info!(
            condition_id = %market.condition_id,
            question = %market.question,
            liquidity = %market.liquidity,
            min_liquidity = %MIN_LIQUIDITY_USDC,
            "skip: liquidity too low"
        );
        return false;
    }

    if let Some(lo) = st.min_market_yes_price {
        if market.yes_price < lo {
            log_skip_decision(
                logger,
                market,
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
            return false;
        }
    }
    if let Some(hi) = st.max_market_yes_price {
        if market.yes_price > hi {
            log_skip_decision(
                logger,
                market,
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
            return false;
        }
    }

    if below_min_secs_to_close(market, st.min_secs_to_close) {
        log_skip_decision(
            logger,
            market,
            "too_close_to_expiry",
            st.min_secs_to_close
                .map(|m| format!("secs_to_close={}, min_secs={}", market.secs_to_close(), m)),
        );
        info!(
            condition_id = %market.condition_id,
            "skip: too close to market expiry"
        );
        return false;
    }

    if above_max_secs_to_close(market, st.max_secs_to_close) {
        log_skip_decision(
            logger,
            market,
            "too_far_from_expiry",
            st.max_secs_to_close
                .map(|m| format!("secs_to_close={}, max_secs={}", market.secs_to_close(), m)),
        );
        info!(
            condition_id = %market.condition_id,
            "skip: too far from market expiry (max_secs_to_close)"
        );
        return false;
    }

    true
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
    order_tracker: &mut OrderTracker,
) -> Result<()> {
    let markets = gamma.active_markets(&cfg.assets, &cfg.durations).await?;
    prometheus_export::add_markets_scanned(markets.len() as u64);
    info!(count = markets.len(), "markets fetched");

    // Phase 1: cheap filters, then parallel Binance fetches (fan-out).
    let mut filtered: Vec<(Market, AssetStrategy)> = Vec::new();
    for market in markets {
        let st = cfg.asset_strategy(&market.asset);
        if passes_pre_candle_filters(&market, &st, risk, logger) {
            filtered.push((market, st));
        }
    }

    let candle_batches = join_all(filtered.into_iter().map(|(market, st)| async move {
        if st.htf_enabled {
            let (primary_res, htf_res) = tokio::join!(
                spot.fetch_candles_at_exchange(
                    &market.asset,
                    &st.candle_interval,
                    st.candle_lookback,
                    &st.spot_exchange,
                ),
                spot.fetch_candles_at_exchange(
                    &market.asset,
                    &st.htf_interval,
                    st.htf_lookback,
                    &st.spot_exchange,
                ),
            );
            (market, st, primary_res, Some(htf_res))
        } else {
            let primary_res = spot
                .fetch_candles_at_exchange(
                    &market.asset,
                    &st.candle_interval,
                    st.candle_lookback,
                    &st.spot_exchange,
                )
                .await;
            (market, st, primary_res, None)
        }
    }))
    .await;

    // Phase 2: sequential signal / edge / risk / order (fan-in).
    for (market, st, primary_res, htf_res_opt) in candle_batches {
        let mut htf_aligned: Option<bool> = None;
        let signal_config = st.signal_config();

        let candles = match primary_res {
            Ok(c) => c,
            Err(e) => {
                log_skip_decision(
                    logger,
                    &market,
                    "candle_fetch_failed",
                    Some(format!("primary: {e}")),
                );
                info!(
                    condition_id = %market.condition_id,
                    asset = %market.asset,
                    error = %e,
                    "skip: primary candle fetch failed"
                );
                continue;
            }
        };

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
                    log_skip_decision(logger, &market, "spot_volume_below_threshold", Some(detail));
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
        signal =
            apply_market_timing_to_signal(signal, &market, window_secs, st.expiry_dampen_last_secs);

        if st.min_momentum_5m_abs > 0.0 && signal.momentum_5m.abs() < st.min_momentum_5m_abs {
            log_skip_decision(
                logger,
                &market,
                "momentum_5m_too_weak",
                Some(format!(
                    "momentum_5m={:.6}, min_abs={:.6}",
                    signal.momentum_5m, st.min_momentum_5m_abs
                )),
            );
            info!(
                condition_id = %market.condition_id,
                asset = %market.asset,
                momentum_5m = signal.momentum_5m,
                min_abs = st.min_momentum_5m_abs,
                "skip: |momentum_5m| below minimum"
            );
            continue;
        }

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
            log_skip_decision(logger, &market, "volatility_filter", Some(vol_detail));
            info!(
                condition_id = %market.condition_id,
                question = %market.question,
                "skip: volatility regime filter"
            );
            continue;
        }

        let volatility_std_pct = compute_return_std_pct(&candles, st.volatility_filter.sample_bars);

        if st.htf_enabled {
            match htf_res_opt {
                Some(Ok(htf)) => {
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
                Some(Err(e)) => {
                    tracing::warn!(
                        error = %e,
                        asset = %market.asset,
                        "HTF candle fetch failed — continuing without HTF filter"
                    );
                }
                None => {}
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
                signal.confidence, eff_min_confidence
            );
            continue;
        }

        let direction = match market_matcher::match_signal_to_market(&signal, &market) {
            Some(dir) => dir,
            None => {
                log_skip_decision(logger, &market, "cannot_match_market_question", None);
                info!(
                    condition_id = %market.condition_id,
                    question = %market.question,
                    "skip: cannot match signal to market question"
                );
                continue;
            }
        };

        if let Some(blocked) = st.blocked_direction {
            if direction == blocked {
                log_skip_decision(
                    logger,
                    &market,
                    "direction_blocked",
                    Some(format!("direction={blocked:?}, asset={}", market.asset)),
                );
                info!(
                    condition_id = %market.condition_id,
                    ?direction,
                    "skip: direction blocked for asset"
                );
                continue;
            }
        }

        let mut edge_min_for_trade = eff_min_edge;
        if signal.cluster_direction == "TIE" {
            let mult = Decimal::from_f64(st.cluster_tie_min_edge_multiplier).unwrap_or(dec!(1));
            edge_min_for_trade = (eff_min_edge * mult).min(dec!(0.50));
        }
        if let Some(tbr) = signal.taker_buy_ratio {
            if (0.45..=0.55).contains(&tbr) {
                let mult = Decimal::from_f64(st.neutral_taker_edge_multiplier).unwrap_or(dec!(1));
                edge_min_for_trade = (edge_min_for_trade * mult).min(dec!(0.50));
            }
        }

        let edge_result = edge::calculate(
            signal.probability,
            market.yes_price,
            edge_min_for_trade,
            st.slippage_bps,
        );

        let Some(mut trade) = edge_result else {
            log_skip_decision(
                logger,
                &market,
                "edge_too_small",
                Some(format!(
                    "signal_prob={}, market_yes_price={}, min_edge={} (base={}, adaptive={}, cluster_tie={})",
                    signal.probability,
                    market.yes_price,
                    edge_min_for_trade,
                    st.min_edge,
                    st.adaptive_thresholds,
                    signal.cluster_direction == "TIE",
                )),
            );
            info!(
                condition_id = %market.condition_id,
                question = %market.question,
                signal_prob = %signal.probability,
                market_price = %market.yes_price,
                threshold = %edge_min_for_trade,
                "skip: edge too small"
            );
            continue;
        };

        if trade.direction != direction {
            match edge::recalculate_for_direction(
                signal.probability,
                market.yes_price,
                direction,
                st.slippage_bps,
                edge_min_for_trade,
            ) {
                Some(recalc) => trade = recalc,
                None => {
                    log_skip_decision(
                        logger,
                        &market,
                        "no_edge_after_direction_override",
                        Some(format!(
                            "signal_prob={}, yes_price={}, forced_dir={:?}",
                            signal.probability, market.yes_price, direction,
                        )),
                    );
                    info!(
                        condition_id = %market.condition_id,
                        question = %market.question,
                        ?direction,
                        "skip: no positive edge for market_matcher direction"
                    );
                    continue;
                }
            }
        }

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
                Some(format!(
                    "size_usdc={}, min_order_usdc={}",
                    size_usdc, st.min_order_usdc
                )),
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

        match executor
            .place_order(
                &market,
                &trade,
                size_usdc,
                trade.token_price,
                market.end_date_ms,
            )
            .await
        {
            Ok(outcome) => {
                let size_shares = if trade.token_price > Decimal::ZERO {
                    size_usdc / trade.token_price
                } else {
                    Decimal::ZERO
                };

                let meta = PendingTradeMeta {
                    asset: market.asset.clone(),
                    duration: market.duration.clone(),
                    direction: trade.direction,
                    limit_price: trade.token_price,
                    size_usdc,
                    size_shares,
                    signal_probability: signal.probability,
                    confidence: signal.confidence,
                    edge: trade.edge,
                    reasoning: signal.reasoning.clone(),
                    rsi: Some(signal.rsi),
                    macd_histogram: Some(signal.macd_histogram),
                    volume_ratio: Some(signal.volume_ratio),
                    cluster_direction: Some(signal.cluster_direction.clone()),
                    market_yes_price: Some(market.yes_price.to_string()),
                    liquidity: Some(market.liquidity.to_string()),
                    secs_to_close: Some(market.secs_to_close()),
                    volatility_std_pct: volatility_std_pct.and_then(|d| d.to_f64()),
                    kelly_fraction: Some(sizing.kelly_fraction.to_string()),
                    balance_at_signal: balance.to_string(),
                    daily_loss_at_signal: risk.daily_loss().to_string(),
                    htf_aligned,
                    adaptive_min_edge: st.adaptive_thresholds.then(|| eff_min_edge.to_string()),
                    adaptive_min_confidence: st
                        .adaptive_thresholds
                        .then(|| eff_min_confidence.to_string()),
                    sizing_cap_hit: Some(sizing.cap_hit.to_string()),
                    momentum_5m: Some(signal.momentum_5m),
                    momentum_15m: Some(signal.momentum_15m),
                    taker_buy_ratio: signal.taker_buy_ratio,
                    macd_line: Some(signal.macd_line),
                    macd_signal_line: Some(signal.macd_signal_line),
                    question: Some(market.question.clone()),
                    slippage_bps: Some(st.slippage_bps.to_string()),
                    effective_min_edge: Some(edge_min_for_trade.to_string()),
                };

                let immediate_fill =
                    executor.is_dry_run() || matches!(outcome.status, OrderStatusType::Matched);

                if immediate_fill {
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
                        outcome.order_id.clone(),
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
                    record.adaptive_min_edge =
                        st.adaptive_thresholds.then(|| eff_min_edge.to_string());
                    record.adaptive_min_confidence = st
                        .adaptive_thresholds
                        .then(|| eff_min_confidence.to_string());
                    record.sizing_cap_hit = Some(sizing.cap_hit.to_string());
                    record.momentum_5m = Some(signal.momentum_5m);
                    record.momentum_15m = Some(signal.momentum_15m);
                    record.taker_buy_ratio = signal.taker_buy_ratio;
                    record.macd_line = Some(signal.macd_line);
                    record.macd_signal_line = Some(signal.macd_signal_line);
                    record.question = Some(market.question.clone());
                    record.slippage_bps = Some(st.slippage_bps.to_string());
                    record.effective_min_edge = Some(edge_min_for_trade.to_string());
                    record.fill_status = Some("filled".to_string());
                    let _ = logger.log_trade(&record);

                    info!(
                        order_id = %outcome.order_id,
                        condition_id = %market.condition_id,
                        "order placed successfully"
                    );

                    if !executor.is_dry_run() {
                        prometheus_export::record_trade_success();
                    }

                    let position = OpenPosition {
                        condition_id: market.condition_id.clone(),
                        order_id: outcome.order_id.clone(),
                        direction: trade.direction,
                        entry_price: trade.token_price,
                        size_usdc,
                        size_shares,
                        end_date_ms: market.end_date_ms,
                    };
                    risk.record_trade(size_usdc, position);
                } else {
                    risk.reserve_for_order(&market.condition_id, size_usdc);
                    order_tracker.add_pending(pending_from_outcome(
                        &outcome,
                        market.condition_id.clone(),
                        market.end_date_ms,
                        meta,
                    ));
                    info!(
                        order_id = %outcome.order_id,
                        condition_id = %market.condition_id,
                        "GTD order resting — awaiting fill"
                    );
                }
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
