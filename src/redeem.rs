///! Redeem functionality for winning positions after market resolution.
///!
///! ## How to Redeem Winning Positions
///!
///! After a Polymarket market resolves, you can redeem your winning outcome tokens
///! for USDC collateral using the Polymarket CTF (Conditional Token Framework) contract.
///!
///! ### Requirements:
///! - Market must be resolved
///! - You must hold winning outcome tokens (YES or NO)
///! - Private key with sufficient gas for transaction
///!
///! ### Example Code:
///!
///! ```rust,no_run
///! use alloy::providers::ProviderBuilder;
///! use alloy::signers::local::PrivateKeySigner;
///! use polymarket_client_sdk::ctf::Client;
///! use polymarket_client_sdk::ctf::types::RedeemPositionsRequest;
///! use polymarket_client_sdk::types::address;
///! use std::str::FromStr;
///!
///! #[tokio::main]
///! async fn main() -> anyhow::Result<()> {
///!     // 1. Setup signer and provider
///!     let private_key = std::env::var("POLYMARKET_PRIVATE_KEY")?;
///!     let signer = PrivateKeySigner::from_str(&private_key)?;
///!
///!     let provider = ProviderBuilder::new()
///!         .wallet(signer)
///!         .on_http("https://polygon-rpc.com".parse()?);
///!
///!     // 2. Create CTF client
///!     let ctf_client = Client::new(provider, 137)?; // 137 = Polygon
///!
///!     // 3. Prepare redeem request
///!     let usdc = address!("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
///!     let condition_id = "0x..."; // From resolved market
///!     let condition_id_bytes = alloy::primitives::B256::from_str(condition_id)?;
///!
///!     let request = RedeemPositionsRequest::for_binary_market(
///!         usdc,
///!         condition_id_bytes,
///!     );
///!
///!     // 4. Execute redeem
///!     let response = ctf_client.redeem_positions(&request).await?;
///!
///!     println!("Redeem successful!");
///!     println!("  TX hash: {}", response.transaction_hash);
///!     println!("  Block: {}", response.block_number);
///!
///!     Ok(())
///! }
///! ```
///!
///! ### Notes:
///! - The `for_binary_market()` method automatically uses index sets [1, 2] for YES/NO
///! - Redeem will fail if market is not resolved yet
///! - You need ETH/MATIC for gas fees on Polygon
///! - Redeemed USDC goes directly to your wallet address; canonical values are re-exported below from [`crate::constants`]

#[allow(dead_code)]
pub const USDC_POLYGON: &str = crate::constants::USDC_POLYGON;
#[allow(dead_code)]
pub const CTF_CONTRACT: &str = crate::constants::CTF_CONTRACT;
#[allow(dead_code)]
pub const POLYGON_RPC: &str = crate::constants::POLYGON_RPC;
