//! Cumulative fill deduplication for Polymarket order updates (WS + REST poll).
//!
//! Mirrors the pattern used in the poly market-maker: [`OrderMessage`] reports cumulative
//! `size_matched`; we convert to incremental deltas for [`FillResult`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use tokio::sync::mpsc;

/// Incremental fill size for one [`FillResult`] broadcast.
#[derive(Debug, Clone)]
pub struct FillResult {
    pub order_id: String,
    /// Delta shares matched since last seen cumulative.
    pub size_matched: Decimal,
}

struct FillTrackerInner {
    broadcast_tx: mpsc::UnboundedSender<FillResult>,
    /// Cumulative `size_matched` last seen per `order_id` (from API / WS).
    last_seen: Mutex<HashMap<String, Decimal>>,
}

/// Shared fill tracker: clone shares the same channel and dedup map (poly-style).
#[derive(Clone)]
pub struct FillTracker {
    inner: Arc<FillTrackerInner>,
}

impl FillTracker {
    /// Returns a tracker and the receiver for incremental fills.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<FillResult>) {
        let (broadcast_tx, broadcast_rx) = mpsc::unbounded_channel();
        (
            Self {
                inner: Arc::new(FillTrackerInner {
                    broadcast_tx,
                    last_seen: Mutex::new(HashMap::new()),
                }),
            },
            broadcast_rx,
        )
    }

    fn broadcast(&self, result: &FillResult) {
        let _ = self.inner.broadcast_tx.send(result.clone());
    }

    /// Cumulative fill already recorded (WS or poll).
    pub fn get_seen(&self, order_id: &str) -> Decimal {
        self.inner
            .last_seen
            .lock()
            .expect("fill_tracker lock")
            .get(order_id)
            .copied()
            .unwrap_or(Decimal::ZERO)
    }

    /// Record cumulative fill without broadcasting (poll path before WS duplicate).
    pub fn mark_seen(&self, order_id: &str, cumulative_size: Decimal) {
        let mut map = self.inner.last_seen.lock().expect("fill_tracker lock");
        let prev = map.get(order_id).copied().unwrap_or(Decimal::ZERO);
        if cumulative_size > prev {
            map.insert(order_id.to_string(), cumulative_size);
        }
    }

    /// Process a user-channel order update: `size_matched` is cumulative.
    pub async fn on_order_update(
        &self,
        order_id: &str,
        size_matched: Decimal,
        _timestamp_ms: Option<i64>,
    ) {
        if size_matched <= Decimal::ZERO {
            return;
        }

        let mut map = self.inner.last_seen.lock().expect("fill_tracker lock");
        let prev = map.get(order_id).copied().unwrap_or(Decimal::ZERO);
        let delta = size_matched - prev;
        if delta > Decimal::ZERO {
            map.insert(order_id.to_string(), size_matched);
            drop(map);
            self.broadcast(&FillResult {
                order_id: order_id.to_string(),
                size_matched: delta,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[tokio::test]
    async fn duplicate_cumulative_does_not_broadcast_twice() {
        let (ft, mut rx) = FillTracker::new();
        ft.on_order_update("o1", dec!(10), None).await;
        let f = rx.recv().await.expect("first fill");
        assert_eq!(f.size_matched, dec!(10));
        ft.on_order_update("o1", dec!(10), None).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn incremental_updates_broadcast_delta() {
        let (ft, mut rx) = FillTracker::new();
        ft.on_order_update("o1", dec!(5), None).await;
        let _ = rx.recv().await;
        ft.on_order_update("o1", dec!(12), None).await;
        let f = rx.recv().await.expect("delta");
        assert_eq!(f.size_matched, dec!(7));
    }
}
