use std::future::IntoFuture;

pub mod api;
pub mod bot {

    use std::collections::{HashMap, HashSet};
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::api::*;
    use async_tungstenite::tungstenite::Message;
    use async_tungstenite::WebSocketStream;
    use chromimic::boring::x509;
    use chromimic::impersonate::Impersonate;
    use futures::stream::StreamExt;
    use proxied::Proxy;
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
    use tracing::{debug, error, info, warn};

    fn magic_round_sol(decimal: Decimal) -> Decimal {
        let sign = if decimal.abs().is_zero() {
            Decimal::from(0)
        } else {
            decimal / decimal.abs()
        };

        let decimal = decimal.abs();
        let dp = cond::cond! {
            decimal >= Decimal::from(1000) => 0,
            decimal >= Decimal::from(100) => 1,
            decimal >= Decimal::from(10) => 2,
            decimal >= Decimal::from_str("0.01").unwrap() => 3,
            decimal >= Decimal::from_str("0.001").unwrap() => 4,
            _ => 5
        };

        let floored = decimal.floor();
        let decimal_part = decimal - floored;

        let exponent = Decimal::from_scientific(&format!("1e{}", dp)).unwrap();

        let acceptable_decimal_part = (decimal_part * exponent)
            .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
            / exponent;

        sign * (floored + acceptable_decimal_part)
    }

    fn lamport_to_dec_sol(x: u64) -> Decimal {
        Decimal::from(x) * Decimal::from_scientific("1e-9").unwrap()
    }

    #[serde_with::serde_as]
    #[derive(Deserialize, Debug, Clone)]
    pub struct StrProxy(#[serde_as(as = "DisplayFromStr")] Proxy);

    use serde_with::{DeserializeFromStr, DisplayFromStr};
    #[serde_with::serde_as]
    #[derive(Debug, serde::Deserialize)]
    pub struct Config {
        pk: String,
        slugs: Vec<String>,
        #[serde_as(as = "DisplayFromStr")]
        impersonate: Impersonate,
        min_top_bids: u32,
        rpc: String,
        inclusive: bool,
        min_top_bids_override: HashMap<String, u32>,
        proxy: Option<StrProxy>,
    }

    pub struct Bot {
        keypair: Arc<Keypair>,
        sdk: MagicEdenSDK,
        ws: MagicEdenWS,
        rpc: RpcClient,
        config: Config,
        my_bids: HashMap<String, Option<(Decimal, String)>>,
        cached_pools: HashMap<String, Vec<serde_json::Value>>,
        escrow_balance: Decimal,
    }

    impl Bot {
        async fn rebid_all(&mut self) {
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

                let transaction_obj = match (current_bid_value, calc_result) {
                    (None, Some(new_bid)) => {
                        info!("Creating new bid on {} for {}", slug, new_bid);
                        self.sdk
                            .create_escrow_pool(slug, new_bid, ExpiryTime::Day)
                            .await
                    }
                    (Some(_), None) => {
                        info!("Cancelling bid on {}", slug);
                        let current_bid = current_bid.as_ref().unwrap();
                        self.sdk.cancel_offer_escrow(&current_bid.1).await
                    }
                    (Some(old_bid), Some(new_bid)) => {
                        info!("Changing bid on {} {}=>{}", slug, old_bid.0, new_bid);
                        let current_bid = current_bid.as_ref().unwrap();
                        self.sdk
                            .edit_pool_escrow(&current_bid.1, new_bid, ExpiryTime::Day)
                            .await
                    }
                    _ => continue,
                };

                let mut transaction: Transaction = bincode::deserialize(
                    &transaction_obj["txSigned"]["data"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|x| x.as_u64().unwrap() as u8)
                        .collect::<Vec<_>>()
                        .as_slice(),
                )
                .unwrap();
                let blockhash = Hash::from_str(
                    &transaction_obj["blockhashData"]["blockhash"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
                let blockheight = transaction_obj["blockhashData"]["lastValidBlockHeight"]
                    .as_u64()
                    .unwrap();

                let keypair = self.keypair.clone();
                transaction.sign(&[&keypair], blockhash);

                let hash = self
                    .rpc
                    .send_transaction_with_config(
                        &transaction,
                        RpcSendTransactionConfig {
                            skip_preflight: true,
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("Failed to send transaction");

                self.rpc.confirm_transaction(&hash).await.unwrap();

                if let Some(pool_key) = transaction_obj["poolKey"].as_str() {
                    self.my_bids.insert(
                        slug.clone(),
                        Some((calc_result.unwrap(), pool_key.to_string())),
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

                info!("OK!");
            }
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
                                pool["poolKey"].as_str().unwrap()
                                    == state["poolKey"].as_str().unwrap()
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
                                data.as_ref().map(|(_, addr)| addr.as_str()) != Some(pool_addr)
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

        async fn get_escrow_balance(sdk: &mut MagicEdenSDK) -> Decimal {
            magic_round_sol(
                Decimal::from_f64(
                    sdk.get_escrow_balance(None).await["balance"]
                        .as_f64()
                        .unwrap(),
                )
                .expect("Failed to parse escrow balance"),
            )
        }

        pub async fn work(&mut self) -> anyhow::Result<()> {
            info!("Starting to work...");

            while let msg = self.ws.recv().await.unwrap() {
                self.process_update(&msg);
                self.rebid_all().await;
            }

            Ok(())
        }

        pub async fn new(config: Config) -> anyhow::Result<Self> {
            let keypair = Arc::new(Keypair::from_base58_string(&config.pk));

            let proxy = config.proxy.clone().map(|x| x.0);

            let mut client = chromimic::ClientBuilder::new()
                .impersonate(config.impersonate.clone())
                .danger_accept_invalid_certs(true);

            if let Some(ref proxy_data) = proxy {
                let mut proxy = chromimic::Proxy::all(format!(
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
                let mut client = reqwest::ClientBuilder::new();

                if let Some(ref proxy_data) = proxy {
                    let mut proxy = reqwest::Proxy::all(format!(
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

                client.build()?
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
            sdk.auth().await;

            // info!("Auth Success for {}!", self.sdk.address);

            tokio::time::sleep(std::time::Duration::from_secs(5)).await;

            let mut ws = sdk.ws().await;

            let mut cached_pools = HashMap::new();
            let mut my_bids = HashMap::new();

            for slug in &config.slugs {
                info!("subcribing for {} collection", slug);
                let pools = sdk.get_pools(slug).await;
                cached_pools.insert(slug.clone(), pools["results"].as_array().unwrap().clone());
                ws.subscribe_collection(slug, "solana").await;
            }

            let owned_bids = sdk.get_owned_pools(None).await;
            let owned_bids = owned_bids["results"].as_array().unwrap();

            for bid in owned_bids {
                my_bids.insert(
                    bid["collectionSymbol"].as_str().unwrap().to_string(),
                    Some((
                        magic_round_sol(lamport_to_dec_sol(bid["spotPrice"].as_u64().unwrap()))
                            .normalize(),
                        bid["poolKey"].as_str().unwrap().to_string(),
                    )),
                );
            }

            let escrow = Self::get_escrow_balance(&mut sdk).await;

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

            self_obj.rebid_all().await;

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

        last_price.map(|price| {
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
    }
}

#[derive(serde::Deserialize)]
pub struct Multiconfig {
    accounts: Vec<bot::Config>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config: Multiconfig =
        toml::from_str(&std::fs::read_to_string("config.toml").unwrap()).unwrap();

    let mut tasks = tokio::task::JoinSet::new();

    for account in config.accounts {
        tasks.spawn(tokio::task::spawn(async move {
            let mut bot = bot::Bot::new(account).await.unwrap();
            bot.work().await.unwrap();
        }).into_future());
    }

    while let Some(_) = tasks.join_next().await {}
}
