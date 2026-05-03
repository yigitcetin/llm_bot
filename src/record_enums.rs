//! Typed values persisted in JSONL / logs (`TradeRecord`, telemetry) with backward-tolerant parsing.

use serde::{Deserialize, Deserializer, Serialize};

/// RSI+momentum cluster vote (stored uppercase in JSONL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ClusterDirection {
    Up,
    Down,
    Tie,
}

/// GTD / lifecycle outcome for a trade row (`filled` | `partial` | `expired` in JSON).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FillStatus {
    Filled,
    Partial,
    Expired,
}

fn parse_cluster_direction_loose(raw: &str) -> Option<ClusterDirection> {
    match raw.trim().to_ascii_uppercase().as_str() {
        "UP" => Some(ClusterDirection::Up),
        "DOWN" => Some(ClusterDirection::Down),
        "TIE" => Some(ClusterDirection::Tie),
        _ => None,
    }
}

fn parse_fill_status_loose(raw: &str) -> Option<FillStatus> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "filled" => Some(FillStatus::Filled),
        "partial" => Some(FillStatus::Partial),
        "expired" => Some(FillStatus::Expired),
        _ => None,
    }
}

/// Legacy JSONL may use odd casing or unknown strings — map to `None` instead of failing parse.
pub fn deserialize_opt_cluster_direction<'de, D>(deserializer: D) -> Result<Option<ClusterDirection>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
        .map(|opt| opt.and_then(|s| parse_cluster_direction_loose(&s)))
}

pub fn deserialize_opt_fill_status<'de, D>(deserializer: D) -> Result<Option<FillStatus>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(|opt| opt.and_then(|s| parse_fill_status_loose(&s)))
}
