use anyhow::anyhow;
use async_tungstenite::tokio::TokioAdapter;
use async_tungstenite::tungstenite::client::IntoClientRequest;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::WebSocketStream;
use proxied::Proxy;
use rquest::header::{HeaderMap, HeaderValue};
use rquest::tls::Impersonate;
use rquest::Client;
use rust_decimal::prelude::*;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::Transaction;
use std::sync::Arc;
use std::time::{self, Duration};
use tokio::time::sleep;
use tracing::info;
use url::Url;

const AUCTION_HOUSE: &str = "E8cU1WiRWjanGxmn96ewBgk9vPTcL6AEZ1t6F6fkgUWe";

pub type TransactionData = (Transaction, solana_sdk::hash::Hash, Option<String>);

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

pub fn magic_round_sol(decimal: Decimal) -> Decimal {
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

pub fn lamport_to_dec_sol(x: u64) -> Decimal {
    Decimal::from(x) * Decimal::from_scientific("1e-9").unwrap()
}
pub struct MagicEdenWS {
    ws: socky::WebSocket,
    ping_interval: tokio::time::Interval,

    subscriptions: Vec<serde_json::Value>,
}

impl MagicEdenWS {
    async fn connect(token: String, proxy: Option<Proxy>) -> anyhow::Result<socky::WebSocket> {
        let url = format!("wss://wss-mainnet.magiceden.io");

        let ws = socky::WebSocket::new(url.into_client_request()?, proxy, false).await?;

        return Ok(ws);
    }

    async fn new(token: String, proxy: Option<Proxy>) -> anyhow::Result<Self> {
        let ws = Self::connect(token, proxy).await?;
        Ok(Self {
            ws,
            ping_interval: tokio::time::interval(std::time::Duration::from_secs(15)),
            subscriptions: Vec::new(),
        })
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

    pub async fn recv(&mut self) -> anyhow::Result<serde_json::Value> {
        let mut socket_ping_interval = tokio::time::interval(Duration::from_secs(5));
        let mut alive_interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            tokio::select! {
                _ = self.ping_interval.tick() => {
                    if let Err(err) = self.ws.send(Message::Text("[\"pong\"]".to_string())).await {
                        panic!("Failed to send ping message: {:?}", err);
                    }
                }
                _ = socket_ping_interval.tick() => {
                    self.ws.send(Message::Ping(Vec::new())).await?;
                }
                msg = self.ws.next() => {
                    if let Some(Ok(msg)) = msg {
                        if msg.is_ping() {
                            self.ws.send(Message::Pong(Vec::new())).await?;
                            tokio::task::yield_now().await;
                            continue;
                        }
                        if msg == Message::Text("ping".to_string()) {
                            tokio::task::yield_now().await;
                            continue;
                        }

                        if let Ok(msg_json) = serde_json::from_str(&msg.to_string()) {
                            return Ok(msg_json);
                        }
                    }
                }

                _ = alive_interval.tick() => {
                    tracing::info!(event = "still.alive");
                            tokio::task::yield_now().await;

                }

            }
            tokio::task::yield_now().await;
        }
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
        // println!("{}", keypair.pubkey().to_string());
        Self {
            keypair,
            session: client,
            impersonate,
            token: None,
            proxy,
        }
    }

    async fn _get_nonce(&self) -> anyhow::Result<String> {
        let mut headers = HeaderMap::new();
        headers.insert("Accept", HeaderValue::from_static("*/*"));
        headers.insert(
            "Accept-Encoding",
            HeaderValue::from_static("gzip, deflate, br, zstd"),
        );
        headers.insert(
            "Accept-Language",
            HeaderValue::from_static("en-US,en;q=0.9"),
        );
        headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        headers.insert("Dnt", HeaderValue::from_static("1"));
        headers.insert("Origin", HeaderValue::from_static("https://magiceden.io"));
        headers.insert("Pragma", HeaderValue::from_static("no-cache"));
        headers.insert("Priority", HeaderValue::from_static("u=1, i"));
        headers.insert("Referer", HeaderValue::from_static("https://magiceden.io/"));
        headers.insert(
            "Sec-Ch-Ua",
            HeaderValue::from_static("\"Not/A)Brand\";v=\"8\", \"Chromium\";v=\"126\""),
        );
        headers.insert("Sec-Ch-Ua-Mobile", HeaderValue::from_static("?0"));
        headers.insert("Sec-Fetch-Dest", HeaderValue::from_static("empty"));
        headers.insert("Sec-Fetch-Mode", HeaderValue::from_static("cors"));
        headers.insert("Sec-Fetch-Site", HeaderValue::from_static("same-site"));
        headers.insert("X-Dyn-Api-Version", HeaderValue::from_static("API/0.0.497"));
        headers.insert("X-Dyn-Version", HeaderValue::from_static("WalletKit/2.5.1"));

        let resp = self
            .session
            .get("https://auth.magiceden.io/api/v0/sdk/c1314b5b-ece8-4b4f-a879-3894dda364e4/nonce")
            .send()
            .await?;

        let resp: serde_json::Value = resp.json().await?;

        Ok(resp
            .get("nonce")
            .and_then(serde_json::Value::as_str)
            .ok_or(anyhow!("failed to parse nonce"))?
            .to_string())
    }
    async fn _request_challenge(&self) -> anyhow::Result<String> {
        let pubkey = self.keypair.pubkey().to_string();
        let now = chrono::Utc::now();
        let timestamp = now.to_rfc3339();
        let nonce = self._get_nonce().await?;
        let payload = format!("magiceden.io wants you to sign in with your Solana account:\n{pubkey}\n\nWelcome to Magic Eden. Signing is the only way we can truly know that you are the owner of the wallet you are connecting. Signing is a safe, gas-less transaction that does not in any way give Magic Eden permission to perform any transactions with your wallet.\n\nURI: https://magiceden.io/rewards\nVersion: 1\nChain ID: mainnet\nNonce: {nonce}\nIssued At: {timestamp}\nRequest ID: c1314b5b-ece8-4b4f-a879-3894dda364e4");

        // let response = self
        //     .session
        //     .post("https://api-mainnet.magiceden.io/auth/login/v2/init")
        //     .header("accept", "application/json, text/plain, */*")
        //     .header("authorization", "Bearer null")
        //     .header("content-type", "application/json")
        //     .header("dnt", "1")
        //     .header("origin", "https://magiceden.io")
        //     .header("priority", "u=1, i")
        //     .header("referer", "https://magiceden.io/")
        //     .header("sec-ch-ua-mobile", "?0")
        //     .header("sec-ch-ua-platform", "\"Windows\"")
        //     .header("sec-fetch-dest", "empty")
        //     .header("sec-fetch-mode", "cors")
        //     .header("sec-fetch-site", "same-site")
        //     .json(&body)
        //     .send()
        //     .await
        //     .expect("Failed to send request");

        // response
        //     .json()
        //     .await

        //     .expect("Failed to parse JSON response")

        Ok(payload)
    }

    async fn _verify_challenge(
        &self,
        signature: &str,
        message: &str,
    ) -> anyhow::Result<(bool, String)> {
        let body = serde_json::json!({
            "additionalWalletAddresses": [],
            "chain": "SOL",
            "messageToSign": message,
            "network": "mainnet",
            "publicWalletAddress": self.keypair.pubkey().to_string(),
            "signedMessage": signature,
            "walletName": "phantom",
            "walletProvider": "browserExtension"
        });

        let resp = self
            .session
            .post(
                "https://auth.magiceden.io/api/v0/sdk/c1314b5b-ece8-4b4f-a879-3894dda364e4/verify",
            )
            .header("accept", "application/json, text/plain, */*")
            .header("content-type", "application/json")
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
            .await?;

        let status = resp.status();

        let token = resp
            .cookies()
            .find(|x| x.name() == "DYNAMIC_JWT_TOKEN")
            .map(|x| x.value().to_string())
            .ok_or(anyhow!("token not found"))?;
        Ok((status.is_success(), token))
    }

    pub async fn auth(&mut self) -> anyhow::Result<()> {
        let challenge = self._request_challenge().await?;
        let challenge_bytes = challenge.as_bytes();
        let signature = self.keypair.sign_message(&challenge_bytes).to_string();

        let (auth_result, token) = self._verify_challenge(&signature, &challenge).await?;

        if !auth_result {
            panic!("Unsuccessful authorization");
        }

        self.token = Some(token);
        Ok(())
    }

    pub async fn get_collection_info(&self, slug: &str) -> anyhow::Result<serde_json::Value> {
        let url = format!(
            "https://api-mainnet.magiceden.io/rpc/getCollectionEscrowStats/{}?edge_cache=true",
            slug
        );

        let resp = self
            .session
            .get(&url)
            .header(
                "Cookie",
                format!("DYNAMIC_JWT_TOKEN={};", self.token.as_ref().unwrap()),
            )
            .send()
            .await?;

        resp.json().await.map_err(anyhow::Error::from)
    }

    pub async fn ws(&self) -> anyhow::Result<MagicEdenWS> {
        let socket = MagicEdenWS::new("".to_owned(), self.proxy.clone()).await?;
        Ok(socket)
    }

    async fn get_ws_token(&self) -> anyhow::Result<String> {
        let resp = self
            .session
            .get("https://api-mainnet.magiceden.io/oauth/encryptedToken")
            .header("accept", "application/json, text/plain, */*")
            .header("accept-language", "en-US,en;q=0.9")
            .header(
                "authorization",
                format!("DYNAMIC_JWT_TOKEN={};", self.token.as_ref().unwrap()),
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
            .await?;

        let text = resp.text().await?.to_string();

        let resp_json: serde_json::Value = serde_json::from_str(&text)?;

        Ok(resp_json["token"].to_string())
    }

    pub fn default_pool_opts() -> serde_json::Value {
        serde_json::json!({})
    }

    pub async fn get_pools(&self, slug: &str) -> anyhow::Result<serde_json::Value> {
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
                "Cookie",
                format!("DYNAMIC_JWT_TOKEN={};", self.token.as_ref().unwrap()),
            )
            .send()
            .await?;

        resp.json().await.map_err(anyhow::Error::from)
    }

    async fn create_pool_custom(
        &self,
        slug: &str,
        spot_price: Decimal,
        expiry: ExpiryTime,
    ) -> anyhow::Result<TransactionData> {
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
                "Cookie",
                format!("DYNAMIC_JWT_TOKEN={};", self.token.as_ref().unwrap()),
            )
            .send()
            .await?;

        let resp_json = resp.json().await?;
        Self::extract_transaction(&resp_json)
    }

    pub async fn create_escrow_pool(
        &self,
        slug: &str,
        spot_price: Decimal,
        expiry: ExpiryTime,
    ) -> anyhow::Result<TransactionData> {
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
                "Cookie",
                format!("DYNAMIC_JWT_TOKEN={};", self.token.as_ref().unwrap()),
            )
            .send()
            .await?;

        let resp_json = resp.json().await?;
        Self::extract_transaction(&resp_json)
    }

    pub async fn edit_pool_custom(
        &self,
        pool: &str,
        spot_price: Decimal,
        expiry: ExpiryTime,
    ) -> anyhow::Result<TransactionData> {
        let params = vec![
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
                "Cookie",
                format!("DYNAMIC_JWT_TOKEN={};", self.token.as_ref().unwrap()),
            )
            .send()
            .await?;

        let resp_json = resp.json().await?;
        Self::extract_transaction(&resp_json)
    }

    pub async fn edit_pool_escrow(
        &self,
        pool: &str,
        spot_price: Decimal,
        expiry: ExpiryTime,
    ) -> anyhow::Result<TransactionData> {
        let params = vec![
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
                "Cookie",
                format!("DYNAMIC_JWT_TOKEN={};", self.token.as_ref().unwrap()),
            )
            .send()
            .await?;

        let resp_json = resp.json().await?;
        Self::extract_transaction(&resp_json)
    }

    pub async fn cancel_offer(
        &self,
        pool_key: &str,
        amount: Decimal,
    ) -> anyhow::Result<TransactionData> {
        let url = "https://api-mainnet.magiceden.io/v2/instructions/mmm/sol-withdraw-buy";

        let resp = self
            .session
            .get(url)
            .query(&[("pool", pool_key), ("paymentAmount", &amount.to_string())])
            .header(
                "Cookie",
                format!("DYNAMIC_JWT_TOKEN={};", self.token.as_ref().unwrap()),
            )
            .send()
            .await?;

        let resp_json = resp.json().await?;
        Self::extract_transaction(&resp_json)
    }

    pub async fn cancel_offer_escrow(&self, pool_key: &str) -> anyhow::Result<TransactionData> {
        let url = "https://api-mainnet.magiceden.io/v2/instructions/mmm/sol-close-pool";

        let resp = self
            .session
            .get(url)
            .query(&[("pool", pool_key)])
            .header(
                "Cookie",
                format!("DYNAMIC_JWT_TOKEN={};", self.token.as_ref().unwrap()),
            )
            .send()
            .await?;

        let resp_json = resp.json().await?;
        Self::extract_transaction(&resp_json)
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

    pub async fn get_owned_pools(
        &self,
        owner: Option<String>,
    ) -> anyhow::Result<serde_json::Value> {
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
            .await?;

        resp.json().await.map_err(anyhow::Error::from)
    }

    pub async fn get_escrow_balance(&self, address: Option<String>) -> anyhow::Result<Decimal> {
        let address = address.unwrap_or_else(|| self.keypair.pubkey().to_string());
        let url = format!(
            "https://api-mainnet.magiceden.io/v2/wallets/{}/escrow_balance?auctionHouseAddress={}",
            address, AUCTION_HOUSE
        );

        let resp = self
            .session
            .get(&url)
            .header(
                "Cookie",
                format!(
                    "DYNAMIC_JWT_TOKEN={};",
                    self.token.as_ref().ok_or(anyhow!("No token present"))?
                ),
            )
            .send()
            .await?;

        // println!("{:#?}", resp);

        let text = resp.text().await?;
        let resp_json: serde_json::Value = serde_json::from_str(&text).map_err(|x| {
            println!("{}", text);
            x
        })?;

        let json_balance = resp_json["balance"].as_f64().unwrap();

        Ok(magic_round_sol(
            Decimal::from_f64(json_balance).ok_or(anyhow!("failed to parse balance from float"))?,
        ))
    }

    fn extract_transaction(resp: &serde_json::Value) -> anyhow::Result<TransactionData> {
        let transaction: Transaction = bincode::deserialize(
            &resp["txSigned"]["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_u64().unwrap() as u8)
                .collect::<Vec<_>>()
                .as_slice(),
        )
        .unwrap();
        let blockhash = solana_sdk::hash::Hash::from_str(
            &resp["blockhashData"]["blockhash"]
                .as_str()
                .ok_or(anyhow!("no blockhash present"))?,
        )?;
        // let blockheight = resp["blockhashData"]["lastValidBlockHeight"]
        //     .as_u64()
        //     .unwrap();

        let pool_key = resp
            .get("poolKey")
            .and_then(|x| x.as_str())
            .map(|x| x.to_owned());
        Ok((transaction, blockhash, pool_key))
    }
    pub async fn deposit_escrow(&self, amount: Decimal) -> anyhow::Result<TransactionData> {
        let url = format!(
            "https://api-mainnet.magiceden.io/v2/instructions/deposit?auctionHouseAddress={}&buyer={}&amount={}",
             AUCTION_HOUSE, self.keypair.pubkey().to_string(),amount.normalize().to_string()
        );

        let resp = self
            .session
            .get(&url)
            .header(
                "Cookie",
                format!("DYNAMIC_JWT_TOKEN={};", self.token.as_ref().unwrap()),
            )
            .send()
            .await?;

        let resp = resp.json().await?;

        Self::extract_transaction(&resp)
    }

    pub fn address(&self) -> String {
        self.keypair.pubkey().to_string()
    }
}
