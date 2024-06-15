pub mod api;
pub mod bot {

    use std::collections::{HashMap, HashSet};
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::api::*;
    use async_tungstenite::tungstenite::Message;
    use async_tungstenite::WebSocketStream;
    use futures::stream::StreamExt;
    use rust_decimal::Decimal;
    use serde_json::json;
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

    struct Config {
        pk: String,
        slugs: Vec<String>,
        impersonate: String,
        min_top_bids: i32,
        rpc: String,
        inclusive: bool,
        min_top_bids_override: HashMap<String, i32>,
        proxy: Option<String>,
    }

    struct Bot {
        sdk: MagicEdenSDK,
        ws: MagicEdenWS,
        rpc: RpcClient,
        config: Config,
        my_bids: HashMap<String, Option<Decimal>>,
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

                if current_bid_value == calc_result.as_ref() {
                    continue;
                }

                let transaction_obj = match (current_bid_value, calc_result) {
                    (None, Some(new_bid)) => {
                        info!("Creating new bid on {} for {}", slug, new_bid);
                        self.sdk.create_escrow_pool(slug, new_bid.to_string()).await
                    }
                    (Some(_), None) => {
                        info!("Cancelling bid on {}", slug);
                        let current_bid = current_bid.as_ref().unwrap();
                        self.sdk
                            .cancel_offer_escrow(current_bid.pool_key.clone())
                            .await
                    }
                    (Some(old_bid), Some(new_bid)) => {
                        info!("Changing bid on {} {}=>{}", slug, old_bid, new_bid);
                        let current_bid = current_bid.as_ref().unwrap();
                        self.sdk
                            .edit_pool_escrow(current_bid.pool_key.clone(), new_bid.to_string())
                            .await
                    }
                    _ => continue,
                };

                let transaction = Transaction::from_bytes(&transaction_obj.tx_signed.data);
                let blockhash = Hash::from_string(&transaction_obj.blockhash_data.blockhash);
                let blockheight = transaction_obj.blockhash_data.last_valid_block_height;

                let keypair = self.keypair.clone();
                let transaction = transaction.sign(&[&keypair], &blockhash);

                let transaction = VersionedTransaction::from_legacy(transaction);

                let opts = TxOpts {
                    skip_preflight: true,
                    skip_confirmation: false,
                    preflight_commitment: "confirmed".to_string(),
                    last_valid_block_height: blockheight,
                    max_retries: 10,
                };

                self.rpc
                    .send_transaction(&transaction, &opts)
                    .await
                    .expect("Failed to send transaction");

                if let Some(pool_key) = transaction_obj.pool_key {
                    self.my_bids.insert(
                        slug.clone(),
                        Some(json!({"poolKey": pool_key, "spotPrice": })),
                    );
                } else if let Some(bid) = self.my_bids.get_mut(slug) {
                    bid.as_mut().unwrap().spot_price = calc_result;
                }

                info!("OK!");
            }
        }

        fn process_update(&mut self, msg: &Box) {
            if msg.name != "fullUpdate" {
                return;
            }

            let state = &msg.pool_state;
            let new_pool = state.clone();

            if state.pool_type == "invalid" || state.buy_orders_amount < 1 {
                self.cached_pools
                    .get_mut(&state.collection_symbol)
                    .map(|pools| {
                        if let Some(index) = pools
                            .iter()
                            .position(|pool| pool["poolKey"].as_str() == state.pool_key)
                        {
                            debug!(
                                "Deleted pool {} from {}",
                                state.pool_key, state.collection_symbol
                            );
                            pools.remove(index);
                        }
                    });
                return;
            }

            let present_index = self
                .cached_pools
                .get(&state.collection_symbol)
                .and_then(|pools| {
                    pools
                        .iter()
                        .position(|pool| pool["poolKey"].as_str().unwrap() == state.pool_key)
                });

            match (present_index, new_pool) {
                (None, None) => return,
                (Some(i), None) => {
                    debug!(
                        "Deleted pool {} from {}",
                        state.pool_key, state.collection_symbol
                    );
                    self.cached_pools
                        .get_mut(&state.collection_symbol)
                        .map(|pools| pools.remove(i));
                }
                (None, Some(new)) => {
                    debug!(
                        "Added pool {} from {}",
                        state.pool_key, state.collection_symbol
                    );
                    self.cached_pools
                        .entry(state.collection_symbol.clone())
                        .or_insert_with(Vec::new)
                        .push(new);
                }
                (Some(old), Some(new)) => {
                    self.cached_pools
                        .get_mut(&state.collection_symbol)
                        .map(|pools| pools[old] = new);
                    debug!(
                        "Changed pool {} from {}",
                        state.pool_key, state.collection_symbol
                    );
                }
            }

            self.cached_pools
                .get_mut(&state.collection_symbol)
                .map(|pools| pools.sort_by_key(|p| p.spot_price.clone()));
        }

        async fn update_escrow(&mut self) {
            self.escrow_balance = magic_round_sol(
                Decimal::from_str(&self.sdk.get_escrow_balance(None).await)
                    .expect("Failed to parse escrow balance"),
            );
        }

        async fn work(&mut self) {
            let keypair = LocalWallet::from_base58_string(&self.config.pk).unwrap();
            self.rpc = RpcClient::new(&self.config.rpc, self.config.proxy.clone())
                .await
                .unwrap();

            self.sdk = MagicEdenSDK::new(
                Arc::new(keypair),
                self.config.impersonate.clone(),
                self.config.proxy.clone(),
            )
            .await
            .unwrap();
            self.sdk.auth().await.unwrap();

            success!(format!("Auth Success for {}!", self.sdk.address));

            self.ws = self.sdk.ws().await.unwrap();

            for slug in &self.config.slugs {
                info!(format!("subcribing for {} collection", slug));
                let pools = self.sdk.get_pools(slug).await.unwrap();
                self.cached_pools.insert(slug.clone(), pools);
                self.ws.subscribe_collection(slug).await.unwrap();
            }

            let owned_bids = self.sdk.get_owned_pools().await.unwrap();

            for bid in owned_bids {
                self.my_bids.insert(
                    bid.collection_symbol.clone(),
                    Some(Box {
                        pool_key: bid.pool_key,
                        spot_price: magic_round_sol(lamport_to_dec_sol(bid.spot_price)).normalize(),
                    }),
                );
            }

            self.update_escrow().await.unwrap();

            let not_yet_set_bids: HashSet<_> = self
                .config
                .slugs
                .iter()
                .cloned()
                .collect::<HashSet<_>>()
                .difference(&self.my_bids.keys().cloned().collect::<HashSet<_>>())
                .cloned()
                .collect();

            for slug in not_yet_set_bids {
                self.my_bids.insert(slug, None);
            }

            self.rebid_all().await;

            info!("Starting to work...");

            while let Some(msg) = self.ws.recv().await.unwrap() {
                self.process_update(&msg);
                self.rebid_all().await.unwrap();
            }
        }
    }

    fn bid_calculator(
        pools: &[serde_json::Value],
        min_top_bids: i32,
        inclusive: bool,
    ) -> Option<Decimal> {
        let mut current_top_bids = 0;
        let mut last_price = None;
        let mut seen_prices = HashSet::new();

        for pool in pools {
            let sol_rounded_price = magic_round_sol(lamport_to_dec_sol(pool.spot_price));

            if !inclusive {
                current_top_bids += pool.buy_orders_amount;
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
                        Decimal::from_scientific(&format!("1e{}", last_price.scale())).unwrap();
                    magic_round_sol(last_price - min_sub).normalize()
                }
            }
        })
    }
}

fn main() {
    println!("Hello, world!");
}
