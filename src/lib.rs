//! Polymarket technical trading bot — library crate (strategy, clients, analysis).
//! The binary (`main`) only wires env, telemetry, and the async loop.

pub mod config;
mod config_toml;
pub mod constants;
pub mod types;
pub use config::AssetStrategy;
pub mod volatility;
pub use volatility::{passes_volatility_filter, VolatilityFilterConfig};
pub mod signals;
pub use signals::SignalError;
pub mod adaptive;
pub mod backtest;
pub mod edge;
pub mod execution;
pub mod fill_tracker;
pub mod gamma;
pub mod indicator_cache;
pub mod market_matcher;
pub mod metrics;
pub mod order_tracker;
pub mod prometheus_export;
pub mod resolution_checker;
pub mod risk;
pub mod signal_extensions;
pub mod spot_price;
pub mod telemetry;
pub mod trading_loop;
pub mod user_ws;
