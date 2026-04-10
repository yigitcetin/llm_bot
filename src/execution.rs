use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::{info, warn};

use polymarket_client_sdk::auth::builder::Builder;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Credentials, Kind, Normal, Signer};
use polymarket_client_sdk::clob;
use polymarket_client_sdk::clob::types::request::CancelMarketOrderRequest;
use polymarket_client_sdk::clob::types::response::PostOrderResponse;
use polymarket_client_sdk::clob::types::{OrderStatusType, OrderType, Side};
use polymarket_client_sdk::types::{Address, B256, U256};

use crate::config::AppConfig;
use crate::types::{Direction, Market, TradeSignal};

/// Polymarket GTD minimum: expiration must be >= now + 60s (per docs).
const MIN_GTD_EXPIRY_SECS: i64 = 60;

/// Result of posting a GTD limit order (used to branch immediate fill vs resting).
#[derive(Debug, Clone)]
pub struct PlaceOrderOutcome {
    pub order_id: String,
    pub status: OrderStatusType,
    /// Share size requested (matches CLOB `original_size` for new orders).
    pub original_size_shares: Decimal,
}

/// Snapshot from `GET /data/order/{id}` for fill tracking / reconciliation.
#[derive(Debug, Clone)]
pub struct OrderPollResult {
    pub status: OrderStatusType,
    pub size_matched: Decimal,
    pub original_size: Decimal,
    pub price: Decimal,
}

/// Authenticated CLOB client: either L2-normal or Builder-promoted (both implement order flow via [`Kind`]).
enum AuthenticatedClobClient {
    Normal(clob::Client<Authenticated<Normal>>),
    Builder(clob::Client<Authenticated<Builder>>),
}

/// Polymarket requires order sizes (maker amount) with at most 2 decimal places.
fn truncate_size(size: Decimal) -> Decimal {
    use rust_decimal::prelude::RoundingStrategy;
    size.round_dp_with_strategy(2, RoundingStrategy::ToZero)
}

fn ws_auth_from(clob: &AuthenticatedClobClient) -> (Credentials, Address) {
    match clob {
        AuthenticatedClobClient::Normal(c) => (c.credentials().clone(), c.address()),
        AuthenticatedClobClient::Builder(c) => (c.credentials().clone(), c.address()),
    }
}

/// Handles order submission to Polymarket CLOB.
pub struct Executor {
    clob_client: Option<AuthenticatedClobClient>,
    signer: Option<alloy::signers::local::PrivateKeySigner>,
    dry_run: bool,
    /// API credentials + wallet address for user WebSocket (`clob::ws`).
    ws_auth: Option<(Credentials, Address)>,
}

impl Executor {
    #[must_use]
    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// Credentials and address for [`crate::user_ws`] (only when not dry-run and CLOB auth succeeded).
    #[must_use]
    pub fn ws_auth(&self) -> Option<(Credentials, Address)> {
        self.ws_auth.clone()
    }

    pub async fn new(_http: reqwest::Client, cfg: &AppConfig) -> Self {
        // In dry-run mode, don't initialize client/signer
        if cfg.dry_run {
            return Self {
                clob_client: None,
                signer: None,
                dry_run: true,
                ws_auth: None,
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
                    ws_auth: None,
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
                    ws_auth: None,
                };
            }
        };

        // Build authentication with proper signature type
        let mut auth_builder = base_client
            .authentication_builder(&signer)
            .signature_type(cfg.signature_type.to_sdk_type());

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
                        ws_auth: None,
                    };
                }
            }
        } else {
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
                    ws_auth: None,
                };
            }
        };

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
                                ws_auth: None,
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

        let ws_auth = Some(ws_auth_from(&clob_client));

        Self {
            clob_client: Some(clob_client),
            signer: Some(signer),
            dry_run: false,
            ws_auth,
        }
    }

    /// Place a **GTD** limit buy: rests on the book until fill or market `end_date_ms`.
    pub async fn place_order(
        &self,
        market: &Market,
        trade: &TradeSignal,
        size_usdc: Decimal,
        worst_price_limit: Decimal,
        end_date_ms: i64,
    ) -> Result<PlaceOrderOutcome> {
        let shares = if trade.token_price > Decimal::ZERO {
            size_usdc / trade.token_price
        } else {
            anyhow::bail!("token price is zero");
        };

        let shares = truncate_size(shares);

        if shares < dec!(5) {
            anyhow::bail!("order size {} below Polymarket minimum (5 shares)", shares);
        }

        let now_secs = Utc::now().timestamp();
        let market_end_secs = end_date_ms / 1000;
        if market_end_secs < now_secs + MIN_GTD_EXPIRY_SECS {
            anyhow::bail!(
                "market end too soon for GTD (expiration must be >= now + {}s)",
                MIN_GTD_EXPIRY_SECS
            );
        }

        let side_str = match trade.direction {
            Direction::Yes => "BUY",
            Direction::No => "BUY",
        };

        let token_id_str = match trade.direction {
            Direction::Yes => market.yes_token_id.as_str(),
            Direction::No => market.no_token_id.as_str(),
        };
        let token_id: U256 = token_id_str
            .parse()
            .context("invalid token_id from Gamma API")?;

        info!(
            condition_id = %market.condition_id,
            asset        = %market.asset,
            side         = %side_str,
            token_price  = %trade.token_price,
            size_usdc    = %size_usdc,
            shares       = %shares,
            token_id     = %token_id,
            end_date_ms  = end_date_ms,
            "placing limit order (GTD)"
        );

        if self.dry_run {
            let order_id = format!("dry-run-{}", uuid::Uuid::new_v4());
            info!(order_id = %order_id, "DRY RUN — order not sent");
            return Ok(PlaceOrderOutcome {
                order_id,
                status: OrderStatusType::Matched,
                original_size_shares: shares,
            });
        }

        let signer = self.signer.as_ref().context("Signer not initialized")?;

        let response = match self
            .clob_client
            .as_ref()
            .context("CLOB client not initialized")?
        {
            AuthenticatedClobClient::Normal(client) => {
                post_gtd_limit_order(
                    client,
                    signer,
                    token_id,
                    shares,
                    worst_price_limit,
                    end_date_ms,
                )
                .await?
            }
            AuthenticatedClobClient::Builder(client) => {
                post_gtd_limit_order(
                    client,
                    signer,
                    token_id,
                    shares,
                    worst_price_limit,
                    end_date_ms,
                )
                .await?
            }
        };

        let order_id = response.order_id.clone();
        let filled = matches!(response.status, OrderStatusType::Matched);

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

        Ok(PlaceOrderOutcome {
            order_id,
            status: response.status,
            original_size_shares: shares,
        })
    }

    /// `GET /data/order/{order_id}` — REST fallback when user WS misses an update.
    pub async fn poll_order(&self, order_id: &str) -> Result<OrderPollResult> {
        if self.dry_run {
            anyhow::bail!("poll_order not available in dry-run");
        }
        let client = self
            .clob_client
            .as_ref()
            .context("CLOB client not initialized")?;
        let r = match client {
            AuthenticatedClobClient::Normal(c) => c.order(order_id).await,
            AuthenticatedClobClient::Builder(c) => c.order(order_id).await,
        }
        .map_err(|e| anyhow::anyhow!("poll order failed: {}", e))?;

        Ok(OrderPollResult {
            status: r.status,
            size_matched: r.size_matched,
            original_size: r.original_size,
            price: r.price,
        })
    }

    /// Cancel a single open order by id.
    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let client = self
            .clob_client
            .as_ref()
            .context("CLOB client not initialized")?;
        match client {
            AuthenticatedClobClient::Normal(c) => c.cancel_order(order_id).await,
            AuthenticatedClobClient::Builder(c) => c.cancel_order(order_id).await,
        }
        .map_err(|e| anyhow::anyhow!("cancel_order failed: {}", e))?;
        Ok(())
    }

    /// Cancel all orders for a market (`condition_id` hex string).
    pub async fn cancel_market_orders(&self, condition_id: &str) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let market = B256::from_str(condition_id)
            .map_err(|e| anyhow::anyhow!("invalid condition_id: {}", e))?;
        let request = CancelMarketOrderRequest::builder().market(market).build();
        let client = self
            .clob_client
            .as_ref()
            .context("CLOB client not initialized")?;
        match client {
            AuthenticatedClobClient::Normal(c) => c.cancel_market_orders(&request).await,
            AuthenticatedClobClient::Builder(c) => c.cancel_market_orders(&request).await,
        }
        .map_err(|e| anyhow::anyhow!("cancel_market_orders failed: {}", e))?;
        Ok(())
    }
}

async fn post_gtd_limit_order<K: Kind>(
    client: &clob::Client<Authenticated<K>>,
    signer: &alloy::signers::local::PrivateKeySigner,
    token_id: U256,
    shares: Decimal,
    limit_price: Decimal,
    end_date_ms: i64,
) -> Result<PostOrderResponse> {
    let exp =
        DateTime::from_timestamp(end_date_ms / 1000, 0).unwrap_or(DateTime::<Utc>::UNIX_EPOCH);

    let order = client
        .limit_order()
        .token_id(token_id)
        .size(shares)
        .price(limit_price)
        .side(Side::Buy)
        .order_type(OrderType::GTD)
        .expiration(exp)
        .build()
        .await
        .context("failed to build limit order")?;

    let signed = client
        .sign(signer, order)
        .await
        .context("failed to sign order")?;

    client
        .post_order(signed)
        .await
        .map_err(|e| anyhow::anyhow!("order submission failed: {}", e))
}
