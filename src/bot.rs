use chrono::Utc;
use rand::prelude::*;
use rand::RngCore;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ops::{Range, RangeInclusive};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use crate::api::*;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::WebSocketStream;
use futures::stream::StreamExt;
use proxied::Proxy;
use rand::seq::IteratorRandom;
use rquest::tls::Impersonate;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::RpcClientConfig;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_rpc_client::http_sender::HttpSender;
use solana_sdk::hash::Hash;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::{Transaction, VersionedTransaction};
use tokio::task;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

#[serde_with::serde_as]
#[derive(Deserialize, Debug, Clone)]
pub struct StrProxy(#[serde_as(as = "DisplayFromStr")] Proxy);

static AUTO_DEPOSIT_RANGE: RangeInclusive<u64> = (89..=92);

use serde_with::{DeserializeFromStr, DisplayFromStr};
#[serde_with::serde_as]
#[derive(Debug, serde::Deserialize, Clone)]
pub struct Config {
    pub pk: String,
    slugs: Vec<String>,
    #[serde_as(as = "DisplayFromStr")]
    impersonate: Impersonate,
    min_top_bids: u32,
    rpc: String,
    inclusive: bool,
    min_top_bids_override: HashMap<String, u32>,
    pub proxy: Option<StrProxy>,
}

pub type BidKey = String;

pub struct Bot {
    keypair: Arc<Keypair>,
    sdk: MagicEdenSDK,
    ws: MagicEdenWS,
    rpc: RpcClient,
    config: Config,
    my_bids: HashMap<String, Option<(Decimal, BidKey, serde_json::Value)>>,
    cached_pools: HashMap<String, Vec<serde_json::Value>>,
    escrow_balance: Decimal,
}

impl Bot {
    async fn rebid_all(&mut self) -> anyhow::Result<()> {
        for (_, bid) in self.my_bids.iter_mut() {
            if let Some((_, bid_key, value)) = bid {
                if let Some(unix_epoch) = value.get("expiry").and_then(Value::as_i64) {
                    if chrono::DateTime::from_timestamp(unix_epoch, 0)
                        .is_some_and(|x| Utc::now() > x)
                    {
                        // should cancel

                        let _ = timeout(
                            Duration::from_secs(60),
                            self.sdk.cancel_offer_escrow(&bid_key),
                        )
                        .await??;

                        *bid = None;
                    }
                }
            }
        }
        // self.my_bids.retain(|_, bid_data| {
        //     if let Some((_, _, value)) = bid_data {
        //            if let Some(unix_epoch) = value.get("expiry").and_then(Value::as_u64) {

        //                if chrono::DateTime::from_timestamp(unix_epoch, 0).is_some_and(|x| Utc::now() > x) {
        //                    /// should cancel
        //                }
        //            }

        //     }
        //     true
        // });

        for (slug, pools) in &self.cached_pools {
            let min_top_bids = self.config.min_top_bids;
            let min_top_bids_override = self.config.min_top_bids_override.get(slug);

            let min_top_bids = match min_top_bids_override {
                Some(override_value) => *override_value,
                None => min_top_bids,
            };

            let calc_result = bid_calculator(pools, min_top_bids, self.config.inclusive);

            let calc_result = calc_result.map(|x| x.min(self.escrow_balance));

            let current_bid = self.my_bids.get(slug).unwrap_or(&None);
            let current_bid_value = current_bid.as_ref();

            if current_bid_value.as_ref().map(|x| &x.0) == calc_result.as_ref() {
                continue;
            }

            let mut expiry_time = None;
            let (mut transaction, mut blockhash, pool_key) = match (current_bid_value, calc_result)
            {
                (None, Some(new_bid)) => {
                    info!("Creating new bid on {} for {}", slug, new_bid.to_string());
                    expiry_time = Some((Utc::now() + chrono::TimeDelta::days(1)).timestamp());
                    timeout(
                        Duration::from_secs(60),
                        self.sdk.create_escrow_pool(slug, new_bid, ExpiryTime::Day),
                    )
                    .await??
                }

                (Some(_), None) => {
                    info!("Cancelling bid on {}", slug);
                    let current_bid = current_bid.as_ref().unwrap();
                    timeout(
                        Duration::from_secs(60),
                        self.sdk.cancel_offer_escrow(&current_bid.1),
                    )
                    .await??
                }
                (Some(old_bid), Some(new_bid)) => {
                    info!("Changing bid on {} {}=>{}", slug, old_bid.0, new_bid);
                    let current_bid = current_bid.as_ref().unwrap();
                    expiry_time = Some((Utc::now() + chrono::TimeDelta::days(1)).timestamp());

                    timeout(
                        Duration::from_secs(60),
                        self.sdk
                            .edit_pool_escrow(&current_bid.1, new_bid, ExpiryTime::Day),
                    )
                    .await??
                }
                _ => continue,
            };

            tokio::time::sleep(std::time::Duration::from_secs(10)).await;

            Self::sign_send_and_confirm_transaction(
                &self.keypair,
                transaction,
                blockhash,
                &self.rpc,
            )
            .await;

            if let Some(pool_key) = pool_key {
                self.my_bids.insert(
                    slug.clone(),
                    Some((
                        calc_result.unwrap(),
                        pool_key.to_string(),
                        serde_json::json!({"expiry": expiry_time}),
                    )),
                );
            } else {
                match calc_result {
                    Some(new_bid) => {
                        self.my_bids.get_mut(slug).unwrap().as_mut().unwrap().0 = new_bid;
                    }
                    None => {
                        self.my_bids.insert(slug.clone(), None);
                    }
                }
            }

            info!(event = "OK!", wallet = self.keypair.pubkey().to_string());
        }

        Ok(())
    }

    async fn sign_send_and_confirm_transaction(
        keypair: &Keypair,
        mut transaction: Transaction,
        blockhash: solana_sdk::hash::Hash,
        rpc: &RpcClient,
    ) {
        let keypair = keypair.clone();
        transaction.sign(&[&keypair], blockhash);

        let hash = rpc
            .send_transaction_with_config(
                &transaction,
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..Default::default()
                },
            )
            .await
            .expect("Failed to send transaction");

        rpc.confirm_transaction(&hash).await.unwrap();
    }

    fn process_update(&mut self, msg: &serde_json::Value) {
        if msg["name"].as_str() != Some("fullUpdate") {
            return;
        }

        match msg["name"].as_str() {
            Some("fullUpdate") => {
                let state = &msg["poolState"];
                let new_pool = state.clone();

                if state["poolType"].as_str() == Some("invalid")
                    || state["buyOrdersAmount"].as_i64() < Some(1)
                {
                    self.cached_pools
                        .get_mut(&(state["collectionSymbol"].as_str().unwrap().to_string()))
                        .map(|pools| {
                            if let Some(index) = pools.iter().position(|pool| {
                                pool["poolKey"].as_str() == state["poolKey"].as_str()
                            }) {
                                debug!(
                                    "Deleted pool {} from {}",
                                    state["poolKey"].as_str().unwrap(),
                                    state["collectionSymbol"].as_str().unwrap()
                                );
                                pools.remove(index);
                            }
                        });
                    return;
                }

                let present_index = self
                    .cached_pools
                    .get(&(state["collectionSymbol"].as_str().unwrap().to_string()))
                    .and_then(|pools| {
                        pools.iter().position(|pool| {
                            pool["poolKey"].as_str().unwrap() == state["poolKey"].as_str().unwrap()
                        })
                    });

                match present_index {
                    None => {
                        debug!(
                            "Added pool {} from {}",
                            state["poolKey"].as_str().unwrap(),
                            state["collectionSymbol"].as_str().unwrap()
                        );
                        self.cached_pools
                            .entry(state["collectionSymbol"].as_str().unwrap().to_string())
                            .or_insert_with(Vec::new)
                            .push(new_pool);
                    }
                    Some(old) => {
                        self.cached_pools
                            .get_mut(&(state["collectionSymbol"].as_str().unwrap().to_string()))
                            .map(|pools| pools[old] = new_pool);
                        debug!(
                            "Changed pool {} from {}",
                            state["poolKey"].as_str().unwrap(),
                            state["collectionSymbol"].as_str().unwrap()
                        );
                    }
                }

                self.cached_pools
                    .get_mut(&(state["collectionSymbol"].as_str().unwrap().to_string()))
                    .map(|pools| {
                        pools.sort_by_key(|p| p["spotPrice"].as_u64().unwrap());
                        pools.reverse();
                    });
            }

            Some("closed") => {
                if let Some(pool_addr) = msg["poolAddress"].as_str() {
                    let is_own_pool = self
                        .my_bids
                        .values()
                        .any(|x| x.as_ref().map(|x| x.1.as_str()) == Some(pool_addr));

                    if is_own_pool {
                        self.my_bids.retain(|_, data| {
                            data.as_ref().map(|(_, addr, _)| addr.as_str()) != Some(pool_addr)
                        });
                    } else {
                        self.cached_pools.values_mut().for_each(|pools| {
                            pools.retain(|pool| pool["poolKey"].as_str() != Some(pool_addr))
                        });
                    }
                }
            }
            _ => {}
        }
    }

    pub async fn work(&mut self) -> anyhow::Result<()> {
        info!("Starting to work...");

        while let msg = self.ws.recv().await? {
            self.process_update(&msg);
            self.rebid_all().await?;
        }

        Ok(())
    }

    pub async fn new(mut config: Config) -> anyhow::Result<Self> {
        let keypair = Arc::new(Keypair::from_base58_string(&config.pk));

        println!("{:#?}", keypair.pubkey());

        let proxy = config.proxy.clone().map(|x| x.0);

        let mut client = rquest::ClientBuilder::new()
            .impersonate(config.impersonate.clone())
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(30))
            .danger_accept_invalid_certs(true);

        if let Some(ref proxy_data) = proxy {
            // println!("{}", proxy_data);

            let mut proxy = rquest::Proxy::all(format!(
                "{}://{}:{}",
                proxy_data.kind.to_string(),
                proxy_data.addr,
                proxy_data.port
            ))?;

            if let Some((ref login, ref password)) = proxy_data.creds {
                proxy = proxy.basic_auth(login, password);
            }

            client = client.proxy(proxy);
        }

        let client_rpc = {
            let mut client = solana_client::client_error::reqwest::ClientBuilder::new();

            if let Some(ref proxy_data) = proxy {
                let mut proxy = solana_client::client_error::reqwest::Proxy::all(format!(
                    "http://{}:{}",
                    // proxy_data.kind.to_string(),
                    proxy_data.addr,
                    proxy_data.port
                ))?;

                if let Some((ref login, ref password)) = proxy_data.creds {
                    proxy = proxy.basic_auth(login, password);
                }

                client = client.proxy(proxy);
            }

            client.timeout(Duration::from_secs(60)).build()?
        };

        let rpc_sender = HttpSender::new_with_client(config.rpc.clone(), client_rpc);
        let rpc = RpcClient::new_sender(
            rpc_sender,
            RpcClientConfig {
                ..Default::default()
            },
        );

        let client = client.build()?;

        let mut sdk = MagicEdenSDK::new(
            keypair.clone(),
            client,
            config.impersonate.clone(),
            proxy.clone(),
        );
        timeout(Duration::from_secs(120), sdk.auth()).await??;

        info!(event = "auth.ok", addr = sdk.address());

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let mut ws = sdk.ws().await?;

        let mut cached_pools = HashMap::new();
        let mut my_bids = HashMap::new();
        let mut escrow = sdk.get_escrow_balance(None).await?;
        let balance_decimal = magic_round_sol(lamport_to_dec_sol(
            rpc.get_balance(&keypair.pubkey()).await?,
        ));

        println!(
            "[{}]: Escrow = {}, SOL = {}",
            keypair.pubkey(),
            escrow.to_string(),
            balance_decimal.to_string()
        );

        if balance_decimal > escrow && balance_decimal > Decimal::from_str("0.01").unwrap() {
            let top_up_percent = AUTO_DEPOSIT_RANGE
                .clone()
                .choose(&mut rand::thread_rng())
                .unwrap();

            let as_decimal = balance_decimal
                * (Decimal::from(1)
                    - ((Decimal::from(100) - Decimal::from(top_up_percent)) / Decimal::from(100)));
            info!("topping up escrow by {} SOL", as_decimal);

            let (transaction, blockhash, _) =
                timeout(Duration::from_secs(60), sdk.deposit_escrow(as_decimal)).await??;

            Self::sign_send_and_confirm_transaction(&keypair, transaction, blockhash, &rpc).await;

            while sdk.get_escrow_balance(None).await?.is_zero() {
                tokio::time::sleep(Duration::from_secs(5)).await
            }

            escrow = sdk.get_escrow_balance(None).await?;
            info!(
                event = "balance.topped_up",
                for_sol = as_decimal.to_string()
            );

            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }

        config.slugs.shuffle(&mut rand::thread_rng());

        for slug in &config.slugs {
            info!(event = "collection.sub", slug = slug);
            let pools = sdk.get_pools(slug).await?;
            cached_pools.insert(slug.clone(), pools["results"].as_array().unwrap().clone());
            ws.subscribe_collection(slug, "solana").await;
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }

        let owned_bids = sdk.get_owned_pools(None).await?;
        let owned_bids = owned_bids["results"].as_array().unwrap();

        for bid in owned_bids {
            my_bids.insert(
                bid["collectionSymbol"].as_str().unwrap().to_string(),
                Some((
                    magic_round_sol(lamport_to_dec_sol(bid["spotPrice"].as_u64().unwrap()))
                        .normalize(),
                    bid["poolKey"].as_str().unwrap().to_string(),
                    bid.clone(),
                )),
            );
        }

        let not_yet_set_bids: HashSet<_> = config
            .slugs
            .iter()
            .cloned()
            .collect::<HashSet<_>>()
            .difference(&my_bids.keys().cloned().collect::<HashSet<_>>())
            .cloned()
            .collect();

        for slug in not_yet_set_bids {
            my_bids.insert(slug, None);
        }

        let mut self_obj = Self {
            keypair,
            sdk,
            ws,
            rpc,
            config,
            my_bids,
            cached_pools,
            escrow_balance: escrow,
        };

        self_obj.rebid_all().await?;

        Ok(self_obj)
    }
}

fn bid_calculator(
    pools: &[serde_json::Value],
    min_top_bids: u32,
    inclusive: bool,
) -> Option<Decimal> {
    let mut current_top_bids = 0;
    let mut last_price = None;
    let mut seen_prices = HashSet::new();

    for pool in pools {
        let sol_rounded_price =
            magic_round_sol(lamport_to_dec_sol(pool["spotPrice"].as_u64().unwrap()));

        if !inclusive {
            current_top_bids += pool["buyOrdersAmount"].as_u64().unwrap() as u32;
        } else if !seen_prices.contains(&sol_rounded_price) {
            current_top_bids += 1;
            seen_prices.insert(sol_rounded_price.clone());
        }

        if current_top_bids >= min_top_bids {
            last_price = Some(sol_rounded_price);
            break;
        }
    }

    last_price
        .map(|price| {
            let last_price = magic_round_sol(price.normalize());

            if inclusive {
                last_price
            } else {
                if last_price <= Decimal::from_scientific("1e-5").unwrap() {
                    last_price
                } else {
                    let min_sub =
                        Decimal::from_scientific(&format!("1e-{}", last_price.scale())).unwrap();
                    magic_round_sol(last_price - min_sub).normalize()
                }
            }
        })
        .filter(|price| price >= &Decimal::from_scientific("1e-5").unwrap())
        .filter(|price| !price.is_zero())
}
