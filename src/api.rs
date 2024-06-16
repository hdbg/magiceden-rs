use async_tungstenite::tokio::TokioAdapter;
use async_tungstenite::tungstenite::client::IntoClientRequest;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::WebSocketStream;
use chromimic as reqwest;
use chromimic::Client;
use proxied::Proxy;
use reqwest::impersonate::Impersonate;
use rust_decimal::prelude::*;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use std::sync::Arc;
use std::time::{self, Duration};
use tokio::time::sleep;
use tracing::info;
use url::Url;

const AUCTION_HOUSE: &str = "E8cU1WiRWjanGxmn96ewBgk9vPTcL6AEZ1t6F6fkgUWe";

pub enum ExpiryTime {
    Hour = 60 * 60,
    HalfDay = 60 * 60 * 12,
    Day = 60 * 60 * 12 * 2,
    ThreeDays = 60 * 60 * 12 * 2 * 3,
    Week = 60 * 60 * 12 * 2 * 3 * 7,
    Month = 60 * 60 * 12 * 2 * 3 * 7 * 30,
}

use futures::sink::Sink;
use futures::stream::Stream;
use futures::{SinkExt, StreamExt};

pub struct MagicEdenWS {
    ws: socky::WebSocket,
    ping_interval: tokio::time::Interval,

    subscriptions: Vec<serde_json::Value>,
}

impl MagicEdenWS {
    async fn connect(token: String, proxy: Option<Proxy>) -> socky::WebSocket {
        let url = format!("wss://wss-mainnet.magiceden.io/{}", token);

        let ws = match proxy {
            Some(proxy) => {
                socky::WebSocket::new_proxy(
                    proxy,
                    "wss-mainnet.magiceden.io".to_string(),
                    443,
                    url.into_client_request().unwrap(),
                )
                .await
            }
            None => socky::WebSocket::new(url.into_client_request().unwrap()).await,
        };

        return ws.unwrap();
    }

    async fn new(token: String, proxy: Option<Proxy>) -> Self {
        let ws = Self::connect(token, proxy).await;
        Self {
            ws,
            ping_interval: tokio::time::interval(std::time::Duration::from_secs(15)),
            subscriptions: Vec::new(),
        }
    }

    pub async fn subscribe_collection(&mut self, slug: &str, chain: &str) {
        let msg = serde_json::json!({
            "type": "subscribeCollection",
            "constraint": {"chain": chain, "collectionSymbol": slug},
        });
        self.subscriptions.push(msg.clone());

        if let Err(err) = self
            .ws
            .send(Message::Text(serde_json::to_string(&msg).unwrap()))
            .await
        {
            panic!("Failed to send subscription message: {:?}", err);
        }
    }

    pub async fn recv(&mut self) -> Option<serde_json::Value> {
        let mut socket_ping_interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = self.ping_interval.tick() => {
                    if let Err(err) = self.ws.send(Message::Text("[\"pong\"]".to_string())).await {
                        panic!("Failed to send ping message: {:?}", err);
                    }
                }
                _ = socket_ping_interval.tick() => {
                    self.ws.send(Message::Ping(Vec::new())).await.unwrap();
                }
                msg = self.ws.next() => {
                    if let Some(Ok(msg)) = msg {
                        if msg.is_ping() {
                            self.ws.send(Message::Pong(Vec::new())).await.unwrap();
                            continue;
                        }
                        if msg == Message::Text("ping".to_string()) {
                            continue;
                        }

                        if let Ok(msg_json) = serde_json::from_str(&msg.to_string()) {
                            return Some(msg_json);
                        }
                    }
                }

            }
        }

        None
    }
}

pub struct MagicEdenSDK {
    keypair: Arc<Keypair>,
    session: Client,
    impersonate: Impersonate,
    token: Option<String>,
    proxy: Option<Proxy>,
}

impl MagicEdenSDK {
    pub fn new(
        keypair: Arc<Keypair>,
        client: Client,
        impersonate: Impersonate,
        proxy: Option<Proxy>,
    ) -> Self {
        Self {
            keypair,
            session: client,
            impersonate,
            token: None,
            proxy,
        }
    }
    async fn _request_challenge(&self) -> serde_json::Value {
        let body = serde_json::json!({
            "address": self.keypair.pubkey().to_string(),
            "blockchain": "solana",
        });

        let response = self
            .session
            .post("https://api-mainnet.magiceden.io/auth/login/v2/init")
            .header("accept", "application/json, text/plain, */*")
            .header("authorization", "Bearer null")
            .header("content-type", "application/json")
            .header("dnt", "1")
            .header("origin", "https://magiceden.io")
            .header("priority", "u=1, i")
            .header("referer", "https://magiceden.io/")
            .header("sec-ch-ua-mobile", "?0")
            .header("sec-ch-ua-platform", "\"Windows\"")
            .header("sec-fetch-dest", "empty")
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-site", "same-site")
            .json(&body)
            .send()
            .await
            .expect("Failed to send request");

        response
            .json()
            .await
            .expect("Failed to parse JSON response")
    }

    async fn _verify_challenge(&self, signature: &str, nonce: &str, token: &str) -> (bool, String) {
        let body = serde_json::json!({
            "address": self.keypair.pubkey().to_string(),
            "blockchain": "solana",
            "is_smart_wallet": false,
            "nonce": nonce,
            "signature": signature,
            "token": token,
            "wallet_name": "Phantom",
        });

        println!("{}", body.to_string());

        let resp = self
            .session
            .post("https://api-mainnet.magiceden.io/auth/login/v2/verify?chainId=solana-mainnet")
            .header("accept", "application/json, text/plain, */*")
            .header("authorization", "Bearer null")
            .header("content-type", "application/json")
            .header("dnt", "1")
            .header("origin", "https://magiceden.io")
            .header("priority", "u=1, i")
            .header("referer", "https://magiceden.io/")
            .header("sec-ch-ua-mobile", "?0")
            .header("sec-ch-ua-platform", "\"Windows\"")
            .header("sec-fetch-dest", "empty")
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-site", "same-site")
            .json(&body)
            .send()
            .await
            .expect("Failed to send request");

        let status = resp.status();

        println!("{:?}", status);

        let text = resp.text().await.expect("Failed to parse JSON response");

        println!("{}", text);

        let json_resp: serde_json::Value =
            serde_json::from_str(&text).expect("Failed to parse JSON response");

        (
            status.is_success(),
            json_resp["token"].as_str().unwrap().to_string(),
        )
    }

    pub async fn auth(&mut self) {
        let challenge = self._request_challenge().await;
        println!("{}", challenge.to_string());
        let challenge_bytes: String = serde_json::from_value(challenge["message"].clone()).unwrap();
        let challenge_bytes = challenge_bytes.as_bytes();
        let signature = self.keypair.sign_message(&challenge_bytes).to_string();

        let (auth_result, token) = self
            ._verify_challenge(
                &signature,
                &challenge["nonce"].as_str().unwrap(),
                &challenge["token"].as_str().unwrap(),
            )
            .await;

        if !auth_result {
            panic!("Unsuccessful authorization");
        }

        println!("{}", token);

        self.token = Some(token);
    }

    pub async fn get_collection_info(&self, slug: &str) -> serde_json::Value {
        let url = format!(
            "https://api-mainnet.magiceden.io/rpc/getCollectionEscrowStats/{}?edge_cache=true",
            slug
        );

        let resp = self
            .session
            .get(&url)
            .header(
                "Authorization",
                format!("Bearer {}", self.token.as_ref().unwrap()),
            )
            .send()
            .await
            .expect("Failed to send request");

        resp.json().await.expect("Failed to parse JSON response")
    }

    pub async fn ws(&self) -> MagicEdenWS {
        let socket = MagicEdenWS::new("".to_owned(), self.proxy.clone()).await;
        socket
    }

    async fn get_ws_token(&self) -> String {
        println!("{}", format!("Bearer {}", self.token.as_ref().unwrap()));
        let resp = self
            .session
            .get("https://api-mainnet.magiceden.io/oauth/encryptedToken")
            .header("accept", "application/json, text/plain, */*")
            .header("accept-language", "en-US,en;q=0.9")
            .header(
                "authorization",
                format!("Bearer {}", self.token.as_ref().unwrap()),
            )
            .header("cache-control", "no-cache")
            .header("origin", "https://magiceden.io")
            .header("pragma", "no-cache")
            .header("referer", "https://magiceden.io/")
            .header("sec-ch-ua-mobile", "?0")
            .header("sec-ch-ua-platform", "\"Windows\"")
            .header("sec-fetch-dest", "empty")
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-site", "same-site")
            .header("User-Agent", self.session.user_agent().unwrap())
            .send()
            .await
            .expect("Failed to send request");

        println!("{:?}", resp.status());

        let text = resp.text().await.expect("Failed to parse JSON response");

        println!("{}", text);

        let resp_json: serde_json::Value =
            serde_json::from_str(&text).expect("Failed to parse JSON response");

        resp_json["token"].to_string()
    }

    pub fn default_pool_opts() -> serde_json::Value {
        serde_json::json!({})
    }

    pub async fn get_pools(&self, slug: &str) -> serde_json::Value {
        let endpoint = "https://api-mainnet.magiceden.io/v2/mmm/pools";

        let params = vec![
            ("showInvalid", "false"),
            ("limit", "150"),
            ("offset", "0"),
            ("filterOnSide", "1"),
            ("hideExpired", "true"),
            ("fundingMode", "0"),
            ("direction", "1"),
            ("field", "5"),
            ("attributesMode", "0"),
            ("attributes", "[]"),
            ("enableSNS", "true"),
            ("collectionSymbol", slug),
        ];

        let resp = self
            .session
            .get(endpoint)
            .query(&params)
            .header(
                "Authorization",
                format!("Bearer {}", self.token.as_ref().unwrap()),
            )
            .send()
            .await
            .expect("Failed to send request");

        resp.json().await.expect("Failed to parse JSON response")
    }

    async fn create_pool_custom(
        &self,
        slug: &str,
        spot_price: Decimal,
        expiry: ExpiryTime,
    ) -> serde_json::Value {
        let mut params = vec![
            ("curveType", String::from("exp")),
            ("curveDelta", String::from("0")),
            ("reinvestBuy", String::from("false")),
            ("reinvestSell", String::from("false")),
            ("lpFeeBp", String::from("0")),
            ("buysideCreatorRoyaltyBp", String::from("0")),
        ];
        params.push(("collectionSymbol", String::from(slug)));
        params.push(("owner", self.keypair.pubkey().to_string()));
        params.push(("spotPrice", spot_price.to_string()));
        params.push(("solDeposit", spot_price.to_string()));
        params.push((
            "paymentMint",
            String::from("11111111111111111111111111111111"),
        ));
        params.push((
            "expiry",
            (time::SystemTime::now()
                .duration_since(time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + expiry as u64)
                .to_string(),
        ));

        let url = "https://api-mainnet.magiceden.io/v2/instructions/mmm/create-pool";

        let resp = self
            .session
            .get(url)
            .query(&params)
            .header(
                "Authorization",
                format!("Bearer {}", self.token.as_ref().unwrap()),
            )
            .send()
            .await
            .expect("Failed to send request");

        resp.json().await.expect("Failed to parse JSON response")
    }

    pub async fn create_escrow_pool(
        &self,
        slug: &str,
        spot_price: Decimal,
        expiry: ExpiryTime,
    ) -> serde_json::Value {
        let mut params = vec![
            ("curveType", String::from("exp")),
            ("curveDelta", String::from("0")),
            ("reinvestBuy", String::from("false")),
            ("reinvestSell", String::from("false")),
            ("lpFeeBp", String::from("0")),
            ("buysideCreatorRoyaltyBp", String::from("0")),
            ("sharedEscrowCount", String::from("1")),
        ];
        params.push(("collectionSymbol", String::from(slug)));
        params.push(("owner", self.keypair.pubkey().to_string()));
        params.push(("spotPrice", spot_price.to_string()));
        params.push(("solDeposit", spot_price.to_string()));
        params.push((
            "paymentMint",
            String::from("11111111111111111111111111111111"),
        ));
        params.push((
            "expiry",
            (time::SystemTime::now()
                .duration_since(time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + expiry as u64)
                .to_string(),
        ));

        let url = "https://api-mainnet.magiceden.io/v2/instructions/mmm/create-pool";

        let resp = self
            .session
            .get(url)
            .query(&params)
            .header(
                "Authorization",
                format!("Bearer {}", self.token.as_ref().unwrap()),
            )
            .send()
            .await
            .expect("Failed to send request");

        resp.json().await.expect("Failed to parse JSON response")
    }

    pub async fn edit_pool_custom(
        &self,
        pool: &str,
        spot_price: Decimal,
        expiry: ExpiryTime,
    ) -> serde_json::Value {
        let mut params = vec![
            ("curveType", String::from("exp")),
            ("curveDelta", String::from("0")),
            ("reinvestBuy", String::from("false")),
            ("reinvestSell", String::from("false")),
            ("lpFeeBp", String::from("0")),
            ("buysideCreatorRoyaltyBp", String::from("0")),
            ("spotPrice", spot_price.to_string()),
            (
                "expiry",
                (time::SystemTime::now()
                    .duration_since(time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + expiry as u64)
                    .to_string(),
            ),
            ("pool", String::from(pool)),
        ];

        let url = "https://api-mainnet.magiceden.io/v2/instructions/mmm/update-pool";

        let resp = self
            .session
            .get(url)
            .query(&params)
            .header(
                "Authorization",
                format!("Bearer {}", self.token.as_ref().unwrap()),
            )
            .send()
            .await
            .expect("Failed to send request");

        resp.json().await.expect("Failed to parse JSON response")
    }

    pub async fn edit_pool_escrow(
        &self,
        pool: &str,
        spot_price: Decimal,
        expiry: ExpiryTime,
    ) -> serde_json::Value {
        let mut params = vec![
            ("curveType", String::from("exp")),
            ("curveDelta", String::from("0")),
            ("reinvestBuy", String::from("false")),
            ("reinvestSell", String::from("false")),
            ("lpFeeBp", String::from("0")),
            ("buysideCreatorRoyaltyBp", String::from("0")),
            ("spotPrice", spot_price.to_string()),
            (
                "expiry",
                (time::SystemTime::now()
                    .duration_since(time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + expiry as u64)
                    .to_string(),
            ),
            ("pool", String::from(pool)),
            ("sharedEscrowCount", String::from("1")),
        ];

        let url = "https://api-mainnet.magiceden.io/v2/instructions/mmm/update-pool";

        let resp = self
            .session
            .get(url)
            .query(&params)
            .header(
                "Authorization",
                format!("Bearer {}", self.token.as_ref().unwrap()),
            )
            .send()
            .await
            .expect("Failed to send request");

        resp.json().await.expect("Failed to parse JSON response")
    }

    pub async fn cancel_offer(&self, pool_key: &str, amount: Decimal) -> serde_json::Value {
        let url = "https://api-mainnet.magiceden.io/v2/instructions/mmm/sol-withdraw-buy";

        let resp = self
            .session
            .get(url)
            .query(&[("pool", pool_key), ("paymentAmount", &amount.to_string())])
            .header(
                "Authorization",
                format!("Bearer {}", self.token.as_ref().unwrap()),
            )
            .send()
            .await
            .expect("Failed to send request");

        resp.json().await.expect("Failed to parse JSON response")
    }

    pub async fn cancel_offer_escrow(&self, pool_key: &str) -> serde_json::Value {
        let url = "https://api-mainnet.magiceden.io/v2/instructions/mmm/sol-close-pool";

        let resp = self
            .session
            .get(url)
            .query(&[("pool", pool_key)])
            .header(
                "Authorization",
                format!("Bearer {}", self.token.as_ref().unwrap()),
            )
            .send()
            .await
            .expect("Failed to send request");

        resp.json().await.expect("Failed to parse JSON response")
    }

    fn default_owned_pool_opts() -> serde_json::Value {
        serde_json::json!({
            "showInvalid": "true",
            "limit": 150,
            "offset": 0,
            "filterOnSide": 1,
            "hideExpired": "false",
            "fundingMode": 0,
            "direction": 1,
            "field": 5,
            "attributesMode": 0,
        })
    }

    pub async fn get_owned_pools(&self, owner: Option<String>) -> serde_json::Value {
        let endpoint = "https://api-mainnet.magiceden.io/v2/mmm/pools";

        let owner = owner.unwrap_or_else(|| self.keypair.pubkey().to_string());
        let mut params = Self::default_pool_opts();
        params["owner"] = serde_json::json!(owner);

        let resp = self
            .session
            .get(endpoint)
            .query(&params)
            .header(
                "authorization",
                format!("bearer {}", self.token.as_ref().unwrap()),
            )
            .send()
            .await
            .expect("Failed to send request");

        resp.json().await.expect("Failed to parse JSON response")
    }

    pub async fn get_escrow_balance(&self, address: Option<String>) -> serde_json::Value {
        let address = address.unwrap_or_else(|| self.keypair.pubkey().to_string());
        let url = format!(
            "https://api-mainnet.magiceden.io/v2/wallets/{}/escrow_balance?auctionHouseAddress={}",
            address, AUCTION_HOUSE
        );

        let resp = self
            .session
            .get(&url)
            .header(
                "Authorization",
                format!("Bearer {}", self.token.as_ref().unwrap()),
            )
            .send()
            .await
            .expect("Failed to send request");

        resp.json().await.expect("Failed to parse JSON response")
    }
}
