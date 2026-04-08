use anyhow::{Context, Result};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::{info, warn};

use polymarket_client_sdk::auth::builder::Builder;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Kind, Normal, Signer};
use polymarket_client_sdk::clob;
use polymarket_client_sdk::clob::types::{Amount, OrderType, Side};
use polymarket_client_sdk::types::U256;

use crate::config::AppConfig;
use crate::types::{Direction, Market, TradeSignal};

/// Authenticated CLOB client: either L2-normal or Builder-promoted (both implement order flow via [`Kind`]).
enum AuthenticatedClobClient {
    Normal(clob::Client<Authenticated<Normal>>),
    Builder(clob::Client<Authenticated<Builder>>),
}

/// Polymarket requires order sizes with at most 2 decimal places.
fn truncate_size(size: Decimal) -> Decimal {
    use rust_decimal::prelude::RoundingStrategy;
    size.round_dp_with_strategy(2, RoundingStrategy::ToZero)
}

/// Handles order submission to Polymarket CLOB.
pub struct Executor {
    clob_client: Option<AuthenticatedClobClient>,
    signer: Option<alloy::signers::local::PrivateKeySigner>,
    dry_run: bool,
}

impl Executor {
    #[must_use]
    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    pub async fn new(_http: reqwest::Client, cfg: &AppConfig) -> Self {
        // In dry-run mode, don't initialize client/signer
        if cfg.dry_run {
            return Self {
                clob_client: None,
                signer: None,
                dry_run: true,
            };
        }

        // Parse private key
        let signer = match cfg
            .polymarket_private_key
            .parse::<alloy::signers::local::PrivateKeySigner>()
        {
            Ok(s) => s.with_chain_id(Some(cfg.chain_id)),
            Err(e) => {
                warn!(error = %e, "failed to parse POLYMARKET_PRIVATE_KEY — falling back to dry-run");
                return Self {
                    clob_client: None,
                    signer: None,
                    dry_run: true,
                };
            }
        };

        // Initialize CLOB client with authentication
        let clob_config = polymarket_client_sdk::clob::Config::builder()
            .use_server_time(true)
            .build();

        let base_client = match clob::Client::new(&cfg.clob_host, clob_config) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "failed to create CLOB client — falling back to dry-run");
                return Self {
                    clob_client: None,
                    signer: None,
                    dry_run: true,
                };
            }
        };

        // Build authentication with proper signature type
        let mut auth_builder = base_client
            .authentication_builder(&signer)
            .signature_type(cfg.signature_type.to_sdk_type());

        // Handle funder address:
        // - For Proxy/GnosisSafe: SDK auto-derives via CREATE2 if not provided
        // - For EOA: No funder needed
        // - Manual override: Use FUNDER_ADDRESS env var if set
        if let Some(ref funder_addr) = cfg.funder_address {
            match funder_addr.parse::<alloy::primitives::Address>() {
                Ok(funder) => {
                    info!(funder = %funder, "using manual funder address (overriding SDK auto-derivation)");
                    auth_builder = auth_builder.funder(funder);
                }
                Err(e) => {
                    warn!(error = %e, "invalid FUNDER_ADDRESS format — falling back to dry-run");
                    return Self {
                        clob_client: None,
                        signer: None,
                        dry_run: true,
                    };
                }
            }
        } else {
            // SDK will auto-derive funder for Proxy/GnosisSafe via CREATE2
            info!(
                signature_type = ?cfg.signature_type,
                "no manual funder specified, SDK will auto-derive for Proxy/GnosisSafe"
            );
        }

        let authenticated = match auth_builder.authenticate().await {
            Ok(auth_client) => auth_client,
            Err(e) => {
                warn!(error = %e, "CLOB authentication failed — falling back to dry-run");
                return Self {
                    clob_client: None,
                    signer: None,
                    dry_run: true,
                };
            }
        };

        // Promote to Builder API if credentials provided (consumes `authenticated` on attempt).
        let clob_client = if let (Some(key), Some(secret), Some(passphrase)) = (
            &cfg.builder_api_key,
            &cfg.builder_api_secret,
            &cfg.builder_api_passphrase,
        ) {
            match (
                key.parse::<uuid::Uuid>(),
                secret.clone(),
                passphrase.clone(),
            ) {
                (Ok(api_key), api_secret, api_pass) => {
                    let builder_credentials = polymarket_client_sdk::auth::Credentials::new(
                        api_key, api_secret, api_pass,
                    );
                    let builder_config =
                        polymarket_client_sdk::auth::builder::Config::local(builder_credentials);

                    match authenticated.promote_to_builder(builder_config).await {
                        Ok(builder_client) => {
                            info!("promoted to Builder API client");
                            AuthenticatedClobClient::Builder(builder_client)
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                "failed to promote to Builder API — CLOB client was consumed; falling back to dry-run"
                            );
                            return Self {
                                clob_client: None,
                                signer: Some(signer),
                                dry_run: true,
                            };
                        }
                    }
                }
                (Err(e), _, _) => {
                    warn!(error = %e, "invalid BUILDER_API_KEY format — continuing with regular auth");
                    AuthenticatedClobClient::Normal(authenticated)
                }
            }
        } else {
            AuthenticatedClobClient::Normal(authenticated)
        };

        Self {
            clob_client: Some(clob_client),
            signer: Some(signer),
            dry_run: false,
        }
    }

    /// Place a market order (FAK - Fill and Kill).
    /// `size_usdc` is the USDC amount to spend.
    ///
    /// Note: We calculate shares manually (USDC / price) to validate minimum size
    /// before submitting. Alternatively, could use Amount::usdc() and let SDK
    /// walk the orderbook, but manual calculation gives us better logging/control.
    pub async fn place_order(
        &self,
        market: &Market,
        trade: &TradeSignal,
        size_usdc: Decimal,
    ) -> Result<String> {
        // Derive shares from USDC and token price
        let shares = if trade.token_price > Decimal::ZERO {
            size_usdc / trade.token_price
        } else {
            anyhow::bail!("token price is zero");
        };

        let shares = truncate_size(shares);

        // Polymarket minimum order size (5 shares)
        if shares < dec!(5) {
            anyhow::bail!("order size {} below Polymarket minimum (5 shares)", shares);
        }

        let side_str = match trade.direction {
            Direction::Yes => "BUY",
            Direction::No => "BUY",
        };

        // Derive token_id from condition_id and direction
        let token_id = derive_token_id(&market.condition_id, trade.direction)?;

        info!(
            condition_id = %market.condition_id,
            asset        = %market.asset,
            side         = %side_str,
            token_price  = %trade.token_price,
            size_usdc    = %size_usdc,
            shares       = %shares,
            token_id     = %token_id,
            "placing market order (FAK)"
        );

        // Dry-run mode: just log and return fake order ID
        if self.dry_run {
            let order_id = format!("dry-run-{}", uuid::Uuid::new_v4());
            info!(order_id = %order_id, "DRY RUN — order not sent");
            return Ok(order_id);
        }

        let signer = self.signer.as_ref().context("Signer not initialized")?;

        let order_id = match self
            .clob_client
            .as_ref()
            .context("CLOB client not initialized")?
        {
            AuthenticatedClobClient::Normal(client) => {
                post_fak_market_order(client, signer, token_id, shares).await?
            }
            AuthenticatedClobClient::Builder(client) => {
                post_fak_market_order(client, signer, token_id, shares).await?
            }
        };

        Ok(order_id)
    }
}

/// Submit a FAK market order using any authenticated CLOB client state ([`Normal`] or [`Builder`]).
async fn post_fak_market_order<K: Kind>(
    client: &clob::Client<Authenticated<K>>,
    signer: &alloy::signers::local::PrivateKeySigner,
    token_id: U256,
    shares: Decimal,
) -> Result<String> {
    let amount = Amount::shares(shares).context("failed to build Amount::shares")?;

    let order = client
        .market_order()
        .token_id(token_id)
        .amount(amount)
        .side(Side::Buy)
        .order_type(OrderType::FAK)
        .build()
        .await
        .context("failed to build market order")?;

    let signed = client
        .sign(signer, order)
        .await
        .context("failed to sign order")?;

    let response = client
        .post_order(signed)
        .await
        .map_err(|e| anyhow::anyhow!("order submission failed: {}", e))?;

    let order_id = response.order_id.clone();
    let filled = matches!(
        response.status,
        polymarket_client_sdk::clob::types::OrderStatusType::Matched
    );

    info!(
        order_id = %order_id,
        filled = filled,
        taking_amount = %response.taking_amount,
        "order placed"
    );

    if !response.success {
        warn!(order_id = %order_id, "order rejected by CLOB");
        anyhow::bail!("order rejected");
    }

    Ok(order_id)
}

/// Derive token ID from condition_id and direction.
/// YES token: keccak256(condition_id + index_set_yes)
/// NO token:  keccak256(condition_id + index_set_no)
///
/// For binary markets: index_set_yes = 0x01, index_set_no = 0x02
fn derive_token_id(condition_id: &str, direction: Direction) -> Result<U256> {
    use alloy::primitives::keccak256;

    // Parse condition_id as B256
    let cid_bytes =
        hex::decode(condition_id.trim_start_matches("0x")).context("invalid condition_id hex")?;
    if cid_bytes.len() != 32 {
        anyhow::bail!("condition_id must be 32 bytes");
    }

    // Index sets for binary markets
    let index_set: u8 = match direction {
        Direction::Yes => 0x01,
        Direction::No => 0x02,
    };

    // Concatenate condition_id + index_set
    let mut input = cid_bytes.to_vec();
    input.push(index_set);

    // Keccak256 hash
    let hash = keccak256(&input);

    Ok(U256::from_be_bytes(*hash))
}
