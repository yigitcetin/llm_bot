//! Open position tracking (`Position`, `PositionTracker`).
//!
//! Not yet wired into the live [`crate::trading_loop`] — reserved for future portfolio / resolution flows.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;

use crate::types::Direction;

/// A single open position in a prediction market.
#[derive(Debug, Clone)]
pub struct Position {
    pub condition_id: String,
    pub asset: String,
    pub duration: String,
    pub side: Direction,
    pub entry_price: Decimal,
    pub size_shares: Decimal,
    pub size_usdc: Decimal,
    pub opened_at: DateTime<Utc>,
    pub order_id: String,
}

/// Tracks all open positions.
pub struct PositionTracker {
    positions: HashMap<String, Position>,
}

impl PositionTracker {
    pub fn new() -> Self {
        Self {
            positions: HashMap::new(),
        }
    }

    /// Record a new position.
    pub fn add(
        &mut self,
        condition_id: String,
        asset: String,
        duration: String,
        side: Direction,
        entry_price: Decimal,
        size_shares: Decimal,
        size_usdc: Decimal,
        order_id: String,
    ) {
        let position = Position {
            condition_id: condition_id.clone(),
            asset,
            duration,
            side,
            entry_price,
            size_shares,
            size_usdc,
            opened_at: Utc::now(),
            order_id,
        };

        self.positions.insert(condition_id, position);
    }

    /// Check if we have a position in this market.
    pub fn has(&self, condition_id: &str) -> bool {
        self.positions.contains_key(condition_id)
    }

    /// Get a position by condition_id.
    pub fn get(&self, condition_id: &str) -> Option<&Position> {
        self.positions.get(condition_id)
    }

    /// Remove a position (when market resolves).
    pub fn remove(&mut self, condition_id: &str) -> Option<Position> {
        self.positions.remove(condition_id)
    }

    /// Get all open positions.
    pub fn all(&self) -> Vec<&Position> {
        self.positions.values().collect()
    }

    /// Count of open positions.
    pub fn count(&self) -> usize {
        self.positions.len()
    }
}

impl Default for PositionTracker {
    fn default() -> Self {
        Self::new()
    }
}
