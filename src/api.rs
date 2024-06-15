use async_tungstenite::tungstenite::Message;
use async_tungstenite::WebSocketStream;
use chromimic as reqwest;
use chromimic::Client;
use proxied::Proxy;
use rust_decimal::prelude::*;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
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
    ws: WebSocketStream<async_tungstenite::tokio::TokioAdapter<tokio::net::TcpStream>>,
    ping_interval: tokio::time::Interval,

    subscriptions: Vec<serde_json::Value>,
}

impl MagicEdenWS {
    async fn connect(
        token: String,
    ) -> WebSocketStream<async_tungstenite::tokio::TokioAdapter<tokio::net::TcpStream>> {
        let url = format!("wss://wss-mainnet.magiceden.io/{}", token);

        let (ws_stream, _) = async_tungstenite::tokio::connect_async(url)
            .await
            .expect("Failed to connect to WebSocket");

        return ws_stream;
    }

    async fn new(token: String) -> Self {
        let ws = Self::connect(token).await;
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
            info!("Failed to send subscription message: {:?}", err);
        }
    }

    pub async fn recv(&mut self) -> Option<serde_json::Value> {
        if let Some(Ok(msg)) = self.ws.next().await {
            if msg == Message::Text("ping".to_string()) {
                return None;
            }

            if let Ok(msg_json) = serde_json::from_str(&msg.to_string()) {
                return Some(msg_json);
            }
        }

        None
    }
}

pub struct MagicEdenSDK {
    keypair: Keypair,
    session: Client,
    impersonate: Option<String>,
    token: Option<String>,
    proxy: Option<String>,
}

impl MagicEdenSDK {
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

        let resp = self
            .session
            .post("https://api-mainnet.magiceden.io/auth/login/v2/verify?chainId=solana-mainnet")
            .header(
                "authorization",
                format!("Bearer {}", self.token.as_ref().unwrap()),
            )
            .json(&body)
            .send()
            .await
            .expect("Failed to send request");

        let status = resp.status();
        let json_resp: serde_json::Value =
            resp.json().await.expect("Failed to parse JSON response");

        (status.is_success(), json_resp["token"].to_string())
    }

    pub async fn auth(&mut self) {
        let challenge = self._request_challenge().await;
        let challenge_bytes: Vec<u8> =
            serde_json::from_value(challenge["message"].clone()).unwrap();
        let signature = self.keypair.sign_message(&challenge_bytes).to_string();

        let (auth_result, token) = self
            ._verify_challenge(
                &signature,
                &challenge["nonce"].to_string(),
                &challenge["token"].to_string(),
            )
            .await;

        if !auth_result {
            panic!("Unsuccessful authorization");
        }

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
        let socket = MagicEdenWS::new(self.get_ws_token().await).await;
        socket
    }

    async fn get_ws_token(&self) -> String {
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
            .send()
            .await
            .expect("Failed to send request");

        let resp_json: serde_json::Value =
            resp.json().await.expect("Failed to parse JSON response");

        resp_json["token"].to_string()
    }

    pub fn default_pool_opts() -> serde_json::Value {
        serde_json::json!({
            "showInvalid": "false",
            "limit": 150,
            "offset": 0,
            "filterOnSide": 1,
            "hideExpired": "true",
            "fundingMode": 0,
            "direction": 1,
            "field": 5,
            "attributesMode": 0,
            "attributes": [],
            "enableSNS": "true",
        })
    }

    pub async fn get_pools(
        &self,
        slug: Option<&str>,
        opts: serde_json::Value,
    ) -> serde_json::Value {
        let endpoint = "https://api-mainnet.magiceden.io/v2/mmm/pools";
        let mut params = opts;

        if let Some(slug) = slug {
            params["collectionSymbol"] = serde_json::json!(slug);
        }

        for (key, value) in Self::default_pool_opts().as_object().unwrap() {
            if !params.as_object().unwrap().contains_key(key) {
                params[key] = value.clone();
            }
        }

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

    fn create_pool_opts_default() -> serde_json::Value {
        serde_json::json!({
            "curveType": "exp",
            "curveDelta": 0,
            "reinvestBuy": "false",
            "reinvestSell": "false",
            "lpFeeBp": 0,
            "buysideCreatorRoyaltyBp": 0,
        })
    }

    async fn create_pool_custom(
        &self,
        slug: &str,
        spot_price: f64,
        expiry: ExpiryTime,
        opts: serde_json::Value,
    ) -> serde_json::Value {
        let mut params = opts;
        params["collectionSymbol"] = serde_json::json!(slug);
        params["owner"] = serde_json::json!(self.keypair.pubkey().to_string());
        params["spotPrice"] = serde_json::json!(spot_price);
        params["solDeposit"] = serde_json::json!(spot_price);
        params["paymentMint"] = serde_json::json!("11111111111111111111111111111111");
        params["expiry"] = serde_json::json!(
            time::SystemTime::now()
                .duration_since(time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + expiry as u64
        );

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
        spot_price: f64,
        expiry: ExpiryTime,
        opts: serde_json::Value,
    ) -> serde_json::Value {
        self.create_pool_custom(slug, spot_price, expiry, opts.clone())
            .await
    }

    pub async fn edit_pool_custom(
        &self,
        pool: &str,
        spot_price: f64,
        expiry: ExpiryTime,
        opts: serde_json::Value,
    ) -> serde_json::Value {
        let mut params = opts;
        params["spotPrice"] = serde_json::json!(spot_price);
        params["expiry"] = serde_json::json!(
            time::SystemTime::now()
                .duration_since(time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + expiry as u64
        );
        params["pool"] = serde_json::json!(pool);

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
        spot_price: f64,
        expiry: ExpiryTime,
        opts: serde_json::Value,
    ) -> serde_json::Value {
        self.edit_pool_custom(pool, spot_price, expiry, opts.clone())
            .await
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
