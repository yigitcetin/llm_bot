//! Polymarket technical trading bot — library crate (strategy, clients, analysis).
//! The binary (`main`) only wires env, telemetry, and the async loop.

pub mod constants;
pub mod types;
pub mod config;
pub use config::AssetStrategy;
pub mod volatility;
pub use volatility::{VolatilityFilterConfig, passes_volatility_filter};
pub mod signals;
pub use signals::SignalError;
pub mod spot_price;
pub mod edge;
pub mod gamma;
pub mod market_matcher;
pub mod execution;
pub mod risk;
pub mod indicator_cache;
pub mod prometheus_export;
pub mod telemetry;
pub mod trading_loop;
pub mod metrics;
pub mod backtest;
pub mod walk_forward;
pub mod positions;
pub mod resolution;
pub mod redeem;
