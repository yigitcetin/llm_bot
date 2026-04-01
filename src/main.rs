//! Binary entrypoint: loads env, tracing, metrics server, then runs the trading loop.

use anyhow::Result;
use reqwest::header::{HeaderMap, ACCEPT, USER_AGENT};
use tracing::info;
use tracing::Instrument;

use polymarket_llm_bot::config::AppConfig;
use polymarket_llm_bot::constants;
use polymarket_llm_bot::execution;
use polymarket_llm_bot::gamma;
use polymarket_llm_bot::indicator_cache;
use polymarket_llm_bot::prometheus_export;
use polymarket_llm_bot::risk;
use polymarket_llm_bot::spot_price;
use polymarket_llm_bot::telemetry;
use polymarket_llm_bot::trading_loop::run_cycle;
use polymarket_llm_bot::metrics;
use polymarket_llm_bot::resolution_checker;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    telemetry::init_tracing()?;
    telemetry::spawn_otel_shutdown_on_ctrl_c();

    if metrics_enabled() {
        prometheus_export::spawn_metrics_server()?;
    }

    let cfg = AppConfig::from_env()?;

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
    let resolver = resolution_checker::ResolutionChecker::new(http.clone());
    let logger = metrics::MetricsLogger::new("data")?;

    let mut indicator_cache =
        indicator_cache::IndicatorCache::new(constants::INDICATOR_CACHE_MAX_AGE_SECS);

    info!("all components initialized, entering main loop");

    loop {
        let cycle_start = std::time::Instant::now();
        let cycle_result = run_cycle(
            &cfg,
            &gamma,
            &spot,
            &executor,
            &mut risk,
            &mut indicator_cache,
        )
        .instrument(tracing::info_span!("trading_cycle"))
        .await;
        prometheus_export::observe_cycle_duration(cycle_start);

        if let Err(e) = cycle_result {
            tracing::error!(error = %e, "cycle error — sleeping before retry");
        }

        indicator_cache.cleanup();

        tokio::time::sleep(std::time::Duration::from_secs(cfg.cycle_secs)).await;

        let open = risk.open_positions_detail();
        resolver.check_and_resolve(&open, &mut risk, &logger).await?;
    }
}

fn metrics_enabled() -> bool {
    !std::env::var("METRICS_ENABLED")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}
