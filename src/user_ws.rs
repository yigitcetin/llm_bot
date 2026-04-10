//! User WebSocket: order updates → [`crate::fill_tracker::FillTracker`] (poly-style).

use std::pin::Pin;
use std::str::FromStr;

use futures::StreamExt;
use polymarket_client_sdk::auth::Credentials;
use polymarket_client_sdk::clob::ws::types::response::{OrderMessage, OrderMessageType};
use polymarket_client_sdk::clob::ws::{Client as WsClient, WsMessage};
use polymarket_client_sdk::types::{Address, B256};
use rust_decimal::Decimal;
use tracing::{debug, error, info, warn};

use crate::fill_tracker::FillTracker;

/// Subscribe to user order events for `condition_ids` (hex `0x…` strings), feed fills into `fill_tracker`.
pub async fn run_user_ws(
    credentials: Credentials,
    address: Address,
    fill_tracker: FillTracker,
    condition_ids: Vec<String>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut backoff = std::time::Duration::from_secs(1);
    let max_backoff = std::time::Duration::from_secs(15);

    let markets: Vec<B256> = condition_ids
        .into_iter()
        .filter_map(|s| B256::from_str(&s).ok())
        .collect();

    if markets.is_empty() {
        info!("user_ws: no markets to subscribe — exiting");
        return;
    }

    loop {
        if *shutdown.borrow() {
            info!("user WS shutdown before connect");
            return;
        }

        info!(markets = markets.len(), "connecting user WS channel");

        let ws_client = WsClient::default();
        let auth_client = match ws_client.authenticate(credentials.clone(), address) {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, "failed to authenticate user WS");
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { return; }
                    }
                }
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        };

        let stream = match auth_client.subscribe_user_events(markets.clone()) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "failed to subscribe user events");
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { return; }
                    }
                }
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        };
        let mut stream: Pin<Box<_>> = Box::pin(stream);

        backoff = std::time::Duration::from_secs(1);
        info!("user WS connected, listening for fill events");

        let disconnected = loop {
            tokio::select! {
                msg = stream.next() => {
                    match msg {
                        Some(Ok(WsMessage::Order(order))) => {
                            handle_order_event(&fill_tracker, &order).await;
                        }
                        Some(Ok(WsMessage::Trade(_))) => {}
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            warn!(error = %e, "user WS stream error");
                        }
                        None => {
                            warn!("user WS stream ended");
                            break true;
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("user WS shutdown signal received");
                        break false;
                    }
                }
            }
        };

        if !disconnected {
            return;
        }

        warn!(
            backoff_ms = backoff.as_millis() as u64,
            "user WS reconnecting"
        );
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
        }
        backoff = (backoff * 2).min(max_backoff);
    }
}

async fn handle_order_event(fill_tracker: &FillTracker, order: &OrderMessage) {
    let msg_type = order.msg_type.as_ref();
    let is_update = matches!(msg_type, Some(OrderMessageType::Update));

    if !is_update {
        debug!(
            order_id = %order.id,
            msg_type = ?msg_type,
            "user WS order event (non-update)"
        );
        return;
    }

    let size_matched = order.size_matched.unwrap_or(Decimal::ZERO);

    if size_matched > Decimal::ZERO {
        let ts_ms = order.timestamp.map(|t| t * 1000);
        fill_tracker
            .on_order_update(&order.id, size_matched, ts_ms)
            .await;
    }
}
