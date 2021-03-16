use crate::errors::{ProtocolError, Result};
use crate::graphql;
use crate::graphql::place_limit_order;
use crate::graphql::place_market_order;
use crate::types::neo::PublicKey as NeoPublicKey;
use crate::types::PublicKey;
use crate::types::{
    Asset, Blockchain, BuyOrSell, Nonce, OrderCancellationPolicy, OrderRate, Rate
};
use crate::utils::pad_zeros;
use graphql_client::GraphQLQuery;
use std::convert::TryInto;

use super::super::signer::Signer;
use super::super::{general_canonical_string, RequestPayloadSignature, State};
use crate::protocol::place_order::blockchain::{btc, eth, neo, FillOrder};
use super::types::{
    LimitOrdersConstructor, LimitOrdersRequest,
    MarketOrderConstructor, MarketOrdersRequest
};

use tokio::sync::RwLock;
use std::sync::Arc;

use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use crate::protocol::place_order::types::{LimitOrderConstructor, PayloadNonces};

/// The form in which queries are sent over HTTP in most implementations. This will be built using the [`GraphQLQuery`] trait normally.
#[derive(Debug, Serialize, Deserialize)]
pub struct MultiQueryBody {
    /// The values for the variables. They must match those declared in the queries. This should be the `Variables` struct from the generated module corresponding to the query.
    pub variables: HashMap<String, serde_json::Value>,
    /// The GraphQL query, as a string.
    pub query: String,
    /// The GraphQL operation name, as a string.
    #[serde(rename = "operationName")]
    pub operation_name: &'static str,
}

type LimitOrdersMutation = MultiQueryBody;
type MarketOrderMutation = graphql_client::QueryBody<place_market_order::Variables>;
type MarketBlockchainSignatures = Vec<Option<place_market_order::BlockchainSignature>>;

impl LimitOrdersRequest {
    // Buy or sell `amount` of `A` in price of `B` for an A/B market. Returns a builder struct
    // of `LimitOrderConstructor` that can be used to create smart contract and graphql payloads
    pub async fn make_constructor(&self, state: Arc<RwLock<State>>) -> Result<LimitOrdersConstructor> {
        let mut constructors = Vec::new();
        for request in &self.requests {
            constructors.push(request.make_constructor(state.clone()).await?);
        }
        Ok(LimitOrdersConstructor { constructors })
    }
}


impl MarketOrdersRequest {
    // Buy or sell `amount` of `A` in price of `B` for an A/B market. Returns a builder struct
    // of `LimitOrderConstructor` that can be used to create smart contract and graphql payloads
    pub async fn make_constructor(&self, state: Arc<RwLock<State>>) -> Result<MarketOrderConstructor> {
        let state = state.read().await;

        let market = match state.get_market(&self.market) {
            Ok(market) => market,
            Err(_) => {
                let reverse_market: Vec<&str> = self.market.split('_').rev().collect();
                let reverse_market = reverse_market.join("_");
                match state.get_market(&reverse_market) {
                    Ok(market) => market.invert(),
                    Err(err) => return Err(err)
                }
            }
        };

        let source = market.asset_a.with_amount(&self.amount)?;
        let destination =  market.asset_b;

        Ok(MarketOrderConstructor {
            client_order_id: self.client_order_id.clone(),
            me_amount: source.clone(),
            market: market.clone(),
            source,
            destination,
        })
    }
}

// If an asset is on another chain, convert it into a crosschain nonce
// FIXME: maybe Nonce should also keep track of asset type to make this easier?
fn map_crosschain(nonce: Nonce, chain: Blockchain, asset: Asset) -> Nonce {
    if asset.blockchain() == chain {
        nonce
    } else {
        Nonce::Crosschain
    }
}

impl LimitOrdersConstructor {
    /// Create a GraphQL request with everything filled in besides blockchain order payloads
    /// and signatures (for both the overall request and blockchain payloads)
    pub fn graphql_request(
        &self,
        current_time: i64,
        affiliate: Option<String>,
    ) -> Result<Vec<place_limit_order::Variables>> {
        let mut result = Vec::new();
        for (index, request) in self.constructors.iter().enumerate() {
            let cancel_at = match request.cancellation_policy {
                OrderCancellationPolicy::GoodTilTime(time) => Some(format!("{:?}", time)),
                _ => None,
            };
            result.push(place_limit_order::Variables {
                payload: place_limit_order::PlaceLimitOrderParams {
                    client_order_id: request.client_order_id.clone(),
                    allow_taker: request.allow_taker,
                    buy_or_sell: request.buy_or_sell.into(),
                    cancel_at,
                    cancellation_policy: request.cancellation_policy.into(),
                    market_name: request.market.market_name(),
                    amount: request.me_amount.clone().try_into()?,
                    // These two nonces are deprecated...
                    nonce_from: 1234,
                    nonce_to: 1234,
                    nonce_order: (current_time as u32) as i64 + index as i64, // 4146194029, // Fixme: what do we validate on this?
                    timestamp: current_time,
                    limit_price: place_limit_order::CurrencyPriceParams {
                        // This format is confusing, but prices are always in
                        // B for an A/B market, so reverse the normal thing
                        currency_a: request.market.asset_b.asset.name().to_string(),
                        currency_b: request.market.asset_a.asset.name().to_string(),
                        amount: request.me_rate.to_bigdecimal()?.to_string(),
                    },
                    blockchain_signatures: vec![],
                },
                affiliate: affiliate.clone(),
                signature: RequestPayloadSignature::empty().into(),
            });
        }
        Ok(result)
    }

    /// Create a signed GraphQL request with blockchain payloads that can be submitted
    /// to Nash
    pub async fn signed_graphql_request(
        &self,
        current_time: i64,
        affiliate: Option<String>,
        state: Arc<RwLock<State>>,
    ) -> Result<LimitOrdersMutation> {
        let variables = self.graphql_request(current_time, affiliate)?;
        let mut map = HashMap::new();
        let mut params = String::new();
        let mut calls = String::new();
        for (index, (mut variable, constructor)) in variables.into_iter().zip(self.constructors.iter()).enumerate() {
            // FIXME: This current_time + index for nonces is replicated in graphql_request. We would benefit to abstract this logic somewhere.
            let nonces = constructor.make_payload_nonces(state.clone(), current_time + index as i64).await?;
            let state = state.read().await;
            let signer = state.signer()?;
            // compute and add blockchain signatures
            let bc_sigs = constructor.blockchain_signatures(signer, &nonces)?;
            variable.payload.blockchain_signatures = bc_sigs;
            // now compute overall request payload signature
            let canonical_string = limit_order_canonical_string(&variable)?;
            let sig: place_limit_order::Signature =
                signer.sign_canonical_string(&canonical_string).into();
            variable.signature = sig;

            let payload = format!("payload{}", index);
            let signature = format!("signature{}", index);
            let affiliate = format!("affiliate{}", index);
            params = if index == 0 { params } else { format!("{}, ", params)};
            params = format!("{}${}: PlaceLimitOrderParams!, ${}: Signature!, ${}: AffiliateDeveloperCode", params, payload, signature, affiliate);
            calls = format!(r#"
                {}
                response{}: placeLimitOrder(payload: ${}, signature: ${}, affiliateDeveloperCode: ${}) {{
                    id
                    status
                    ordersTillSignState,
                    buyOrSell,
                    market {{
                        name
                    }},
                    placedAt,
                    type
                }}
                "#, calls, index, payload, signature, affiliate);
            map.insert(payload, serde_json::to_value(variable.payload).unwrap());
            map.insert(signature, serde_json::to_value(variable.signature).unwrap());
            map.insert(affiliate, serde_json::to_value(variable.affiliate).unwrap());
        }
        Ok(LimitOrdersMutation {
            variables: map,
            operation_name: "PlaceLimitOrder",
            query: format!(r#"
                mutation PlaceLimitOrder({}) {{
                    {}
                }}
            "#, params, calls)
        })
    }
}

impl MarketOrderConstructor {
    /// Helper to transform a limit order into signed fillorder data on every blockchain
    pub fn make_fill_order(
        &self,
        chain: Blockchain,
        pub_key: &PublicKey,
        nonces: &PayloadNonces,
    ) -> Result<FillOrder> {
        // Rate is in "dest per source", so a higher rate is always beneficial to a user
        // Here we insure the minimum rate is the rate they specified
        let min_order = Rate::MinOrderRate;
        let max_order = Rate::MaxOrderRate;
        // Amount is specified in the "source" asset
        let amount = self.source.amount.clone();
        let fee_rate = Rate::MinOrderRate; // 0

        match chain {
            Blockchain::Ethereum => Ok(FillOrder::Ethereum(eth::FillOrder::new(
                pub_key.to_address()?.try_into()?,
                self.source.asset.into(),
                self.destination.into(),
                map_crosschain(nonces.nonce_from, chain, self.source.asset.into()),
                map_crosschain(nonces.nonce_to, chain, self.destination.into()),
                amount,
                min_order,
                max_order,
                fee_rate,
                nonces.order_nonce,
            ))),
            Blockchain::Bitcoin => Ok(FillOrder::Bitcoin(btc::FillOrder::new(
                map_crosschain(nonces.nonce_from, chain, self.source.asset.into()),
                map_crosschain(nonces.nonce_to, chain, self.destination.into()),
            ))),
            Blockchain::NEO => {
                // FIXME: this can still be improved...
                let neo_pub_key: NeoPublicKey = pub_key.clone().try_into()?;
                let neo_order = neo::FillOrder::new(
                    neo_pub_key,
                    self.source.asset.into(),
                    self.destination.into(),
                    map_crosschain(nonces.nonce_from, chain, self.source.asset.into()),
                    map_crosschain(nonces.nonce_to, chain, self.destination.into()),
                    amount,
                    min_order,
                    max_order,
                    fee_rate,
                    nonces.order_nonce,
                );
                Ok(FillOrder::NEO(neo_order))
            }
        }
    }

    /// Create a signed blockchain payload in the format expected by GraphQL when
    /// given `nonces` and a `Client` as `signer`. FIXME: handle other chains
    pub fn blockchain_signatures(
        &self,
        signer: &Signer,
        nonces: &[PayloadNonces],
    ) -> Result<MarketBlockchainSignatures> {
        let mut order_payloads = Vec::new();
        let blockchains = self.market.blockchains();
        for blockchain in blockchains {
            let pub_key = signer.child_public_key(blockchain)?;
            for nonce_group in nonces {
                let fill_order = self.make_fill_order(blockchain, &pub_key, nonce_group)?;
                order_payloads.push(Some(fill_order.to_market_blockchain_signature(signer)?))
            }
        }
        Ok(order_payloads)
    }

    /// Create a GraphQL request with everything filled in besides blockchain order payloads
    /// and signatures (for both the overall request and blockchain payloads)
    pub fn graphql_request(
        &self,
        current_time: i64,
        affiliate: Option<String>,
    ) -> Result<place_market_order::Variables> {
        let order_args = place_market_order::Variables {
            payload: place_market_order::PlaceMarketOrderParams {
                buy_or_sell: BuyOrSell::Sell.into(),
                client_order_id: self.client_order_id.clone(),
                market_name: self.market.market_name(),
                amount: self.me_amount.clone().try_into()?,
                // These two nonces are deprecated...
                nonce_from: Some(0),
                nonce_to: Some(0),
                nonce_order: (current_time as u32) as i64, // 4146194029, // Fixme: what do we validate on this?
                timestamp: current_time,
                blockchain_signatures: vec![],
            },
            affiliate,
            signature: RequestPayloadSignature::empty().into(),
        };
        Ok(order_args)
    }

    /// Create a signed GraphQL request with blockchain payloads that can be submitted
    /// to Nash
    pub fn signed_graphql_request(
        &self,
        nonces: Vec<PayloadNonces>,
        current_time: i64,
        affiliate: Option<String>,
        signer: &Signer,
    ) -> Result<MarketOrderMutation> {
        let mut request = self.graphql_request(current_time, affiliate)?;
        // compute and add blockchain signatures
        let bc_sigs = self.blockchain_signatures(signer, &nonces)?;
        request.payload.blockchain_signatures = bc_sigs;
        // now compute overall request payload signature
        let canonical_string = market_order_canonical_string(&request)?;
        let sig: place_market_order::Signature =
            signer.sign_canonical_string(&canonical_string).into();
        request.signature = sig;
        Ok(graphql::PlaceMarketOrder::build_query(request))
    }

    // Construct payload nonces with source as `from` asset name and destination as
    // `to` asset name. Nonces will be retrieved from current values in `State`
    pub async fn make_payload_nonces(
        &self,
        state: Arc<RwLock<State>>,
        current_time: i64,
    ) -> Result<Vec<PayloadNonces>> {
        let state = state.read().await;
        let asset_nonces = state.asset_nonces.as_ref()
            .ok_or(ProtocolError("Asset nonce map does not exist"))?;
        let (from, to) = (
            self.market.asset_a.asset.name(),
            self.market.asset_b.asset.name(),
        );
        let nonce_froms: Vec<Nonce> = asset_nonces
            .get(from)
            .ok_or(ProtocolError("Asset nonce for source does not exist"))?
            .iter()
            .map(|nonce| Nonce::Value(*nonce))
            .collect();
        let nonce_tos: Vec<Nonce> = asset_nonces
            .get(to)
            .ok_or(ProtocolError(
                "Asset nonce for destination a does not exist",
            ))?
            .iter()
            .map(|nonce| Nonce::Value(*nonce))
            .collect();
        let mut nonce_combinations = Vec::new();
        for nonce_from in &nonce_froms {
            for nonce_to in &nonce_tos {
                nonce_combinations.push(PayloadNonces {
                    nonce_from: *nonce_from,
                    nonce_to: *nonce_to,
                    order_nonce: Nonce::Value(current_time as u32),
                })
            }
        }
        Ok(nonce_combinations)
    }
}

pub fn limit_order_canonical_string(variables: &place_limit_order::Variables) -> Result<String> {
    let serialized_all = serde_json::to_string(variables).map_err(|_|ProtocolError("Failed to serialize limit order into canonical string"))?;

    Ok(general_canonical_string(
        "place_limit_order".to_string(),
        serde_json::from_str(&serialized_all).map_err(|_|ProtocolError("Failed to deserialize limit order into canonical string"))?,
        vec!["blockchain_signatures".to_string()],
    ))
}

pub fn market_order_canonical_string(variables: &place_market_order::Variables) -> Result<String> {
    let serialized_all = serde_json::to_string(variables).map_err(|_|ProtocolError("Failed to serialize market order into canonical string"))?;

    Ok(general_canonical_string(
        "place_market_order".to_string(),
        serde_json::from_str(&serialized_all).map_err(|_|ProtocolError("Failed to deserialize market order into canonical string"))?,
        vec!["blockchain_signatures".to_string()],
    ))
}

