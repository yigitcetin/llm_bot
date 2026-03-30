//! Prometheus metrics (HTTP `/metrics`). Separate from [`crate::metrics`] (file trade logs).

use std::net::SocketAddr;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::http::header::CONTENT_TYPE;
use axum::{routing::get, Router};
use once_cell::sync::Lazy;
use prometheus::{
    register_histogram, register_int_counter, Encoder, Histogram, IntCounter, TextEncoder,
};

static TRADES_TOTAL: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "trades_total",
        "Successful order placements (excludes dry-run)"
    )
    .expect("register trades_total")
});

static ORDERS_FAILED_TOTAL: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "orders_failed_total",
        "Failed order placement attempts"
    )
    .expect("register orders_failed_total")
});

static CYCLE_DURATION_SECONDS: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "cycle_duration_seconds",
        "Wall time for one full trading cycle (run_cycle)",
        vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0]
    )
    .expect("register cycle_duration_seconds")
});

static MARKETS_SCANNED_TOTAL: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "markets_scanned_total",
        "Markets returned from Gamma per cycle (sum of cycles)"
    )
    .expect("register markets_scanned_total")
});

pub fn record_trade_success() {
    TRADES_TOTAL.inc();
}

pub fn record_order_failure() {
    ORDERS_FAILED_TOTAL.inc();
}

pub fn observe_cycle_duration(start: Instant) {
    let secs = start.elapsed().as_secs_f64();
    CYCLE_DURATION_SECONDS.observe(secs);
}

pub fn add_markets_scanned(n: u64) {
    MARKETS_SCANNED_TOTAL.inc_by(n);
}

async fn metrics_text() -> impl axum::response::IntoResponse {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
        tracing::error!(error = %e, "prometheus encode failed");
        buffer = format!("# encode error: {}\n", e).into_bytes();
    }
    ([(CONTENT_TYPE, prometheus::TEXT_FORMAT)], buffer)
}

/// Binds `METRICS_BIND` (default `127.0.0.1:9090`) and serves `GET /metrics`.
pub fn spawn_metrics_server() -> Result<()> {
    let bind = std::env::var("METRICS_BIND").unwrap_or_else(|_| "127.0.0.1:9090".to_string());
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid METRICS_BIND: {bind}"))?;

    let app = Router::new().route("/metrics", get(|| async { metrics_text().await }));

    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    addr = %addr,
                    "metrics server bind failed — Prometheus scrape disabled"
                );
                return;
            }
        };
        tracing::info!(addr = %addr, "Prometheus metrics server listening");
        let server = axum::serve(listener, app);
        if let Err(e) = server.await {
            tracing::error!(error = %e, "metrics server error");
        }
    });

    Ok(())
}
