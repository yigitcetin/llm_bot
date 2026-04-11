//! Binary entrypoint: loads env, tracing, metrics server, then runs the trading loop.

use anyhow::Result;
use reqwest::header::{HeaderMap, ACCEPT, USER_AGENT};
use tokio::sync::watch;
use tracing::info;
use tracing::Instrument;

use polymarket_llm_bot::config::AppConfig;
use polymarket_llm_bot::constants;
use polymarket_llm_bot::execution;
use polymarket_llm_bot::fill_tracker::FillTracker;
use polymarket_llm_bot::gamma;
use polymarket_llm_bot::indicator_cache;
use polymarket_llm_bot::metrics;
use polymarket_llm_bot::order_tracker::OrderTracker;
use polymarket_llm_bot::prometheus_export;
use polymarket_llm_bot::resolution_checker;
use polymarket_llm_bot::risk;
use polymarket_llm_bot::spot_price;
use polymarket_llm_bot::telemetry;
use polymarket_llm_bot::trading_loop::run_cycle;
use polymarket_llm_bot::user_ws;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    telemetry::init_tracing()?;
    telemetry::spawn_otel_shutdown_on_ctrl_c();

    if metrics_enabled() {
        prometheus_export::spawn_metrics_server()?;
    }

    let cfg = AppConfig::load()?;

    info!(
        dry_run = cfg.dry_run,
        assets = ?cfg.assets,
        gamma_tag_id = cfg.gamma_tag_id,
        "polymarket technical trading bot starting"
    );

    // Gamma / some CDNs reject the default `reqwest` User-Agent; send JSON Accept + a common browser UA.
    let mut default_headers = HeaderMap::new();
    default_headers.insert(ACCEPT, "application/json".parse().expect("static header"));
    default_headers.insert(
        USER_AGENT,
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
            .parse()
            .expect("static header"),
    );

    let http = reqwest::Client::builder()
        .default_headers(default_headers)
        .timeout(std::time::Duration::from_secs(10))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .pool_max_idle_per_host(10)
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .build()?;

    let gamma = gamma::GammaClient::new(http.clone(), cfg.gamma_tag_id);
    let spot = spot_price::SpotPriceClient::new(http.clone(), cfg.spot_exchange.clone());
    let executor = execution::Executor::new(http.clone(), &cfg).await;
    let mut risk = risk::RiskManager::new(&cfg);
    let resolver = resolution_checker::ResolutionChecker::new(http.clone(), &cfg.clob_host);
    let logger = metrics::MetricsLogger::new(&cfg.data_dir)?;

    let (fill_tracker, mut fill_rx) = FillTracker::new();
    let mut order_tracker = OrderTracker::new(fill_tracker);

    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut ws_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut last_ws_cids: Vec<String> = Vec::new();

    let mut indicator_cache =
        indicator_cache::IndicatorCache::new(constants::INDICATOR_CACHE_MAX_AGE_SECS);

    info!("all components initialized, entering main loop");

    loop {
        if let Err(e) = order_tracker.process_fill_channel(&mut fill_rx, &mut risk, &logger) {
            tracing::error!(error = %e, "fill channel processing (start of cycle)");
        }

        sync_user_ws(
            &executor,
            &order_tracker,
            &mut ws_task,
            &mut last_ws_cids,
            shutdown_rx.clone(),
        );

        let cycle_start = std::time::Instant::now();
        let cycle_result = run_cycle(
            &cfg,
            &gamma,
            &spot,
            &executor,
            &mut risk,
            &mut indicator_cache,
            &logger,
            &mut order_tracker,
        )
        .instrument(tracing::info_span!("trading_cycle"))
        .await;
        prometheus_export::observe_cycle_duration(cycle_start);

        if let Err(e) = cycle_result {
            tracing::error!(error = %e, "cycle error — sleeping before retry");
        }

        indicator_cache.cleanup();

        sync_user_ws(
            &executor,
            &order_tracker,
            &mut ws_task,
            &mut last_ws_cids,
            shutdown_rx.clone(),
        );

        tokio::time::sleep(std::time::Duration::from_secs(cfg.cycle_secs)).await;

        if let Err(e) = order_tracker.process_fill_channel(&mut fill_rx, &mut risk, &logger) {
            tracing::error!(error = %e, "fill channel processing (after sleep)");
        }

        if let Err(e) = order_tracker
            .poll_and_reconcile(
                &executor,
                &mut risk,
                &logger,
                cfg.fill_timeout_secs,
                cfg.poll_min_order_age_secs,
            )
            .await
        {
            tracing::error!(error = %e, "poll_and_reconcile");
        }

        let open = risk.open_positions_detail();
        resolver
            .check_and_resolve(&open, &mut risk, &logger)
            .await?;

        // File-based resolution: resolve trades from trades.jsonl whose market close time
        // (parsed from question string) has passed. Covers dry-run mode and bot restarts.
        match resolver.resolve_unresolved_trades(&logger).await {
            Ok(n) if n > 0 => info!(resolved = n, "resolved trades from trades.jsonl"),
            Err(e) => tracing::error!(error = %e, "resolve_unresolved_trades"),
            _ => {}
        }

        match resolver.resolve_unresolved_shadow_trades(&logger).await {
            Ok(n) if n > 0 => info!(resolved = n, "resolved shadow trades from shadow_trades.jsonl"),
            Err(e) => tracing::error!(error = %e, "resolve_unresolved_shadow_trades"),
            _ => {}
        }
    }
}

/// (Re)spawn user WebSocket when pending `condition_id` set changes; abort previous task.
fn sync_user_ws(
    executor: &execution::Executor,
    order_tracker: &OrderTracker,
    ws_task: &mut Option<tokio::task::JoinHandle<()>>,
    last_ws_cids: &mut Vec<String>,
    shutdown_rx: watch::Receiver<bool>,
) {
    if executor.is_dry_run() {
        return;
    }
    let mut cids = order_tracker.ws_condition_ids();
    cids.sort();
    cids.dedup();
    if cids == *last_ws_cids {
        return;
    }
    if let Some(t) = ws_task.take() {
        t.abort();
    }
    *last_ws_cids = cids.clone();
    if cids.is_empty() {
        return;
    }
    let Some((creds, addr)) = executor.ws_auth() else {
        return;
    };
    let ft = order_tracker.fill_tracker();
    *ws_task = Some(tokio::spawn(async move {
        user_ws::run_user_ws(creds, addr, ft, cids, shutdown_rx).await;
    }));
}

fn metrics_enabled() -> bool {
    !std::env::var("METRICS_ENABLED")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}
