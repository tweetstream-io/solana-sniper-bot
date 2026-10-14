//! Every read the bot makes from outside the process: Solana RPC, Jupiter quotes and prices, Jito
//! tip levels, DexScreener pools, Rugcheck. [`LiveNetwork`] calls the real services; the demo and
//! the benchmark use `scenario::ScenarioNetwork`, so a dry run can work with no network at all.

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// One account as returned by the RPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountData {
    /// Owning program, base58.
    pub owner: String,
    /// Raw account bytes.
    pub data: Vec<u8>,
}

/// Market data for a token that is not on the pump.fun curve.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MarketSnapshot {
    /// Market cap of the deepest pool.
    pub market_cap_usd: Option<f64>,
    /// Liquidity of the deepest pool.
    pub liquidity_usd: Option<f64>,
    /// Earliest pool creation.
    pub created_at_ms: Option<f64>,
    /// Venue of the deepest pool, as DexScreener names it (pumpfun, raydium, ...).
    pub platform: Option<String>,
}

/// The parts of a Rugcheck report the gate reads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RugcheckSummary {
    /// Normalized risk score.
    pub score: Option<f64>,
    /// Names of danger-level risks.
    pub dangers: Vec<String>,
}

/// A read that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetError(pub String);

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NetError {}

impl From<reqwest::Error> for NetError {
    fn from(error: reqwest::Error) -> Self {
        NetError(error.to_string())
    }
}

/// Result of a network read.
pub type NetResult<T> = Result<T, NetError>;

/// A recent blockhash and the last block height it is valid for.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Blockhash {
    /// Base58 blockhash.
    pub blockhash: String,
    /// Last block height at which a transaction with this blockhash can land.
    pub last_valid_block_height: u64,
}

/// The current epoch and block height.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpochInfo {
    /// Current epoch.
    pub epoch: u64,
    /// Current block height.
    pub block_height: u64,
}

/// What the RPC knows about a sent transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct SignatureStatus {
    /// Confirmed or finalized.
    pub confirmed: bool,
    /// The transaction's error, when it failed.
    pub err: Option<Value>,
}

/// `GET /swap/v2/order` with a taker (<https://developers.jup.ag/docs/swap/order-and-execute>).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JupiterOrder {
    /// Unsigned transaction, base64, or `None` when there is no route.
    pub transaction: Option<String>,
    /// Id to pass to `/execute`.
    pub request_id: String,
    /// The order's hard expiry.
    pub last_valid_block_height: Option<u64>,
    /// Why there is no transaction.
    pub error_message: Option<String>,
}

/// `POST /swap/v2/execute`. Code 0 is success.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JupiterExecute {
    /// `Success` or `Failed`.
    pub status: String,
    /// Result code.
    pub code: i64,
    /// The signature that landed.
    pub signature: Option<String>,
    /// Error text.
    pub error: Option<String>,
    /// Tokens received.
    pub total_output_amount: Option<String>,
}

/// What a Jupiter order is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRequest {
    /// Mint spent.
    pub input_mint: String,
    /// Mint received.
    pub output_mint: String,
    /// Raw amount of `input_mint`.
    pub amount: u64,
    /// The wallet that signs.
    pub taker: String,
}

/// Every outside call except sending. The live implementation calls the services; the scenario
/// implementation answers from a file so the demo, the benchmark, and the trade tests run the real
/// pipeline without a network.
pub trait Network: Send + Sync + 'static {
    /// Reads several accounts in one round trip at `processed`, so a mint created in the last slot
    /// is there.
    fn get_accounts(&self, addresses: &[String]) -> impl Future<Output = NetResult<Vec<Option<AccountData>>>> + Send;
    /// Returns a recent blockhash.
    fn latest_blockhash(&self) -> impl Future<Output = NetResult<Blockhash>> + Send;
    /// Returns the current epoch and block height.
    fn epoch_info(&self) -> impl Future<Output = NetResult<EpochInfo>> + Send;
    /// Returns the status of a sent transaction, or `None` when the RPC has not seen it.
    fn signature_status(&self, signature: &str) -> impl Future<Output = NetResult<Option<SignatureStatus>>> + Send;
    /// Returns how many lamports the transaction took from `owner` (fees, tips, rent, principal), or
    /// `None` when the transaction is not found.
    fn transaction_debit(&self, signature: &str, owner: &str) -> impl Future<Output = NetResult<Option<i128>>> + Send;
    /// Returns an unsigned transaction from the PumpPortal local trading API.
    fn pumpportal_tx(&self, body: Value) -> impl Future<Output = NetResult<Vec<u8>>> + Send;
    /// Asks Jupiter for an order the taker signs.
    fn jupiter_order(&self, request: OrderRequest) -> impl Future<Output = NetResult<JupiterOrder>> + Send;
    /// Hands a signed order to Jupiter, which lands it. An error means the outcome is unknown.
    fn jupiter_execute(
        &self,
        signed_tx: &str,
        request_id: &str,
        timeout: Duration,
    ) -> impl Future<Output = NetResult<JupiterExecute>> + Send;
    /// Returns what selling `tokens` of `mint` for SOL would return now, from a Jupiter quote, or
    /// `None` when there is no route.
    fn quote_sell_lamports(&self, mint: &str, tokens: u64) -> impl Future<Output = NetResult<Option<u64>>> + Send;
    /// Returns the wallet's raw balance of a token.
    fn token_balance(&self, owner: &str, mint: &str) -> impl Future<Output = NetResult<u64>> + Send;
    /// Returns recent priority fees in micro-lamports per compute unit.
    fn recent_priority_fees(&self) -> impl Future<Output = NetResult<Vec<u64>>> + Send;
    /// Returns the recent landed Jito tip level in SOL.
    fn jito_tip_floor_sol(&self) -> impl Future<Output = NetResult<Option<f64>>> + Send;
    /// Returns the SOL price in USD.
    fn sol_usd_price(&self) -> impl Future<Output = NetResult<Option<f64>>> + Send;
    /// Returns market data for a token.
    fn market_snapshot(&self, mint: &str) -> impl Future<Output = NetResult<MarketSnapshot>> + Send;
    /// Returns the Rugcheck summary, or `None` when there is no report.
    fn rugcheck(&self, mint: &str) -> impl Future<Output = NetResult<Option<RugcheckSummary>>> + Send;
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Returns the shared HTTP client: keep-alive pools, TCP_NODELAY, and HTTP/2 when the server
/// offers it through ALPN, so after the first request a call costs one round trip.
///
/// # Panics
///
/// Panics if the TLS backend cannot be initialized.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .http2_keep_alive_interval(Duration::from_secs(15))
        .http2_keep_alive_while_idle(true)
        .build()
        .expect("TLS backend initializes")
}

/// Makes one JSON-RPC call. Used for reads here and for `sendTransaction` in `land/land.rs`.
///
/// # Errors
///
/// Returns [`NetError`] on a transport error, a non-2xx status, or a JSON-RPC error.
pub async fn rpc_call<T: DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> NetResult<T> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    let response = client.post(url).json(&body).timeout(timeout).send().await?;
    if !response.status().is_success() {
        return Err(NetError(format!("{method}: HTTP {}", response.status().as_u16())));
    }
    #[derive(Deserialize)]
    struct RpcError {
        message: Option<String>,
    }
    #[derive(Deserialize)]
    struct Reply {
        #[serde(default)]
        result: Value,
        error: Option<RpcError>,
    }
    let reply: Reply = response.json().await?;
    if let Some(error) = reply.error {
        return Err(NetError(format!("{method}: {}", error.message.unwrap_or_else(|| "error".to_string()))));
    }
    serde_json::from_value(reply.result).map_err(|e| NetError(format!("{method}: {e}")))
}

/// Wrapped SOL mint.
pub const WSOL: &str = "So11111111111111111111111111111111111111112";
const JUPITER: &str = "https://api.jup.ag";
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const SIDE_TIMEOUT: Duration = Duration::from_secs(5);

/// Reads from the real services.
#[derive(Debug, Clone)]
pub struct LiveNetwork {
    client: reqwest::Client,
    rpc_url: String,
    jupiter_api_key: Option<String>,
}

impl LiveNetwork {
    /// Creates a reader on one RPC URL.
    pub fn new(client: reqwest::Client, rpc_url: String, jupiter_api_key: Option<String>) -> LiveNetwork {
        LiveNetwork { client, rpc_url, jupiter_api_key }
    }

    /// The keyless tier allows 0.5 requests per second; a free key raises that.
    fn jupiter(&self, url: String) -> reqwest::RequestBuilder {
        let request = self.client.get(url).timeout(SIDE_TIMEOUT);
        match &self.jupiter_api_key {
            Some(key) => request.header("x-api-key", key),
            None => request,
        }
    }
}

impl Network for LiveNetwork {
    async fn get_accounts(&self, addresses: &[String]) -> NetResult<Vec<Option<AccountData>>> {
        #[derive(Deserialize)]
        struct Raw {
            owner: String,
            data: (String, String),
        }
        #[derive(Deserialize)]
        struct Reply {
            value: Vec<Option<Raw>>,
        }
        let params = json!([addresses, { "encoding": "base64", "commitment": "processed" }]);
        let reply: Reply = rpc_call(&self.client, &self.rpc_url, "getMultipleAccounts", params, READ_TIMEOUT).await?;
        let mut out = Vec::new();
        for raw in reply.value {
            let account = match raw {
                Some(Raw { owner, data: (data, _encoding) }) => {
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .map_err(|e| NetError(format!("getMultipleAccounts: {e}")))?;
                    Some(AccountData { owner, data })
                }
                None => None,
            };
            out.push(account);
        }
        Ok(out)
    }

    async fn latest_blockhash(&self) -> NetResult<Blockhash> {
        #[derive(Deserialize)]
        struct Reply {
            value: Blockhash,
        }
        let params = json!([{ "commitment": "confirmed" }]);
        let reply: Reply = rpc_call(&self.client, &self.rpc_url, "getLatestBlockhash", params, READ_TIMEOUT).await?;
        Ok(reply.value)
    }

    async fn epoch_info(&self) -> NetResult<EpochInfo> {
        rpc_call(&self.client, &self.rpc_url, "getEpochInfo", json!([{ "commitment": "confirmed" }]), READ_TIMEOUT).await
    }

    async fn signature_status(&self, signature: &str) -> NetResult<Option<SignatureStatus>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Entry {
            confirmation_status: Option<String>,
            err: Option<Value>,
        }
        #[derive(Deserialize)]
        struct Reply {
            value: Vec<Option<Entry>>,
        }
        let params = json!([[signature], { "searchTransactionHistory": true }]);
        let reply: Reply = rpc_call(&self.client, &self.rpc_url, "getSignatureStatuses", params, READ_TIMEOUT).await?;
        let Some(Some(entry)) = reply.value.into_iter().next() else {
            return Ok(None);
        };
        let confirmed = matches!(entry.confirmation_status.as_deref(), Some("confirmed" | "finalized"));
        Ok(Some(SignatureStatus { confirmed, err: entry.err.filter(|e| !e.is_null()) }))
    }

    async fn transaction_debit(&self, signature: &str, owner: &str) -> NetResult<Option<i128>> {
        let params = json!([signature, { "commitment": "confirmed", "maxSupportedTransactionVersion": 0, "encoding": "jsonParsed" }]);
        let tx: Value = rpc_call(&self.client, &self.rpc_url, "getTransaction", params, READ_TIMEOUT).await?;
        let keys = tx["transaction"]["message"]["accountKeys"].as_array().cloned().unwrap_or_default();
        let mut index = None;
        for (i, key) in keys.iter().enumerate() {
            let key = key.as_str().or_else(|| key["pubkey"].as_str());
            if key == Some(owner) {
                index = Some(i);
                break;
            }
        }
        let Some(i) = index else {
            return Ok(None);
        };
        let (Some(pre), Some(post)) = (tx["meta"]["preBalances"][i].as_i64(), tx["meta"]["postBalances"][i].as_i64()) else {
            return Ok(None);
        };
        Ok(Some(i128::from(pre) - i128::from(post)))
    }

    /// PumpPortal local trading API (<https://pumpportal.fun/local-trading-api/trading-api/>).
    async fn pumpportal_tx(&self, body: Value) -> NetResult<Vec<u8>> {
        let response = self.client.post("https://pumpportal.fun/api/trade-local").json(&body).timeout(SIDE_TIMEOUT).send().await?;
        if response.status().as_u16() != 200 {
            return Err(NetError(format!("PumpPortal HTTP {}", response.status().as_u16())));
        }
        Ok(response.bytes().await?.to_vec())
    }

    async fn jupiter_order(&self, request: OrderRequest) -> NetResult<JupiterOrder> {
        let OrderRequest { input_mint, output_mint, amount, taker } = request;
        let url = format!("{JUPITER}/swap/v2/order?inputMint={input_mint}&outputMint={output_mint}&amount={amount}&taker={taker}");
        let response = self.jupiter(url).send().await?;
        if !response.status().is_success() {
            return Err(NetError(format!("Jupiter /order HTTP {}", response.status().as_u16())));
        }
        Ok(response.json().await?)
    }

    async fn jupiter_execute(&self, signed_tx: &str, request_id: &str, timeout: Duration) -> NetResult<JupiterExecute> {
        let body = json!({ "signedTransaction": signed_tx, "requestId": request_id });
        let mut request = self.client.post(format!("{JUPITER}/swap/v2/execute")).json(&body).timeout(timeout);
        if let Some(key) = &self.jupiter_api_key {
            request = request.header("x-api-key", key);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(NetError(format!("Jupiter /execute HTTP {}", response.status().as_u16())));
        }
        Ok(response.json().await?)
    }

    /// `GET /swap/v2/order` without a taker returns a quote and no transaction.
    async fn quote_sell_lamports(&self, mint: &str, tokens: u64) -> NetResult<Option<u64>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Quote {
            out_amount: Option<String>,
        }
        let response = self.jupiter(format!("{JUPITER}/swap/v2/order?inputMint={mint}&outputMint={WSOL}&amount={tokens}")).send().await?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let quote: Quote = response.json().await?;
        Ok(quote.out_amount.and_then(|a| a.parse().ok()))
    }

    async fn token_balance(&self, owner: &str, mint: &str) -> NetResult<u64> {
        let params = json!([owner, { "mint": mint }, { "encoding": "jsonParsed", "commitment": "confirmed" }]);
        let reply: Value = rpc_call(&self.client, &self.rpc_url, "getTokenAccountsByOwner", params, READ_TIMEOUT).await?;
        let mut total = 0u64;
        for account in reply["value"].as_array().into_iter().flatten() {
            let amount = account["account"]["data"]["parsed"]["info"]["tokenAmount"]["amount"].as_str().and_then(|a| a.parse::<u64>().ok());
            total = total.saturating_add(amount.unwrap_or(0));
        }
        Ok(total)
    }

    /// No account filter: a filter only counts transactions that lock every listed account as
    /// writable, and the accounts every pump.fun trade shares are read-only.
    async fn recent_priority_fees(&self) -> NetResult<Vec<u64>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Row {
            prioritization_fee: u64,
        }
        let rows: Vec<Row> = rpc_call(&self.client, &self.rpc_url, "getRecentPrioritizationFees", json!([]), READ_TIMEOUT).await?;
        let mut fees = Vec::new();
        for row in rows {
            fees.push(row.prioritization_fee);
        }
        Ok(fees)
    }

    /// <https://docs.jito.wtf/lowlatencytxnsend/> "Get Tip Information". Values are in SOL.
    async fn jito_tip_floor_sol(&self) -> NetResult<Option<f64>> {
        #[derive(Deserialize)]
        struct Row {
            landed_tips_75th_percentile: Option<f64>,
        }
        let response = self.client.get("https://bundles.jito.wtf/api/v1/bundles/tip_floor").timeout(SIDE_TIMEOUT).send().await?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let rows: Vec<Row> = response.json().await?;
        Ok(rows.first().and_then(|r| r.landed_tips_75th_percentile))
    }

    /// Jupiter Price API V3 (<https://dev.jup.ag/docs/price/v3>). An absent mint means no reliable
    /// price.
    async fn sol_usd_price(&self) -> NetResult<Option<f64>> {
        let response = self.jupiter(format!("{JUPITER}/price/v3?ids={WSOL}")).send().await?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let body: Value = response.json().await?;
        Ok(body[WSOL]["usdPrice"].as_f64())
    }

    /// DexScreener "Get the pools of a given token address"
    /// (<https://docs.dexscreener.com/api/reference>).
    async fn market_snapshot(&self, mint: &str) -> NetResult<MarketSnapshot> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Liquidity {
            usd: Option<f64>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Pair {
            dex_id: Option<String>,
            liquidity: Option<Liquidity>,
            market_cap: Option<f64>,
            pair_created_at: Option<f64>,
        }
        let url = format!("https://api.dexscreener.com/token-pairs/v1/solana/{mint}");
        let response = self.client.get(url).send().await?;
        if !response.status().is_success() {
            return Ok(MarketSnapshot::default());
        }
        let pairs: Vec<Pair> = response.json().await.unwrap_or_default();
        let depth = |p: &Pair| p.liquidity.as_ref().and_then(|l| l.usd).unwrap_or(0.0);
        let mut deepest: Option<&Pair> = None;
        let mut created: Option<f64> = None;
        for pair in &pairs {
            if deepest.is_none_or(|d| depth(pair) > depth(d)) {
                deepest = Some(pair);
            }
            if let Some(at) = pair.pair_created_at {
                created = Some(created.map_or(at, |c| c.min(at)));
            }
        }
        let Some(deepest) = deepest else {
            return Ok(MarketSnapshot::default());
        };
        Ok(MarketSnapshot {
            market_cap_usd: deepest.market_cap,
            liquidity_usd: deepest.liquidity.as_ref().and_then(|l| l.usd),
            created_at_ms: created,
            platform: deepest.dex_id.clone(),
        })
    }

    /// Rugcheck token report summary (<https://api.rugcheck.xyz/swagger/index.html>). No key needed.
    async fn rugcheck(&self, mint: &str) -> NetResult<Option<RugcheckSummary>> {
        #[derive(Deserialize)]
        struct Risk {
            name: Option<String>,
            level: Option<String>,
        }
        #[derive(Deserialize)]
        struct Raw {
            score_normalised: Option<f64>,
            risks: Option<Vec<Risk>>,
        }
        let url = format!("https://api.rugcheck.xyz/v1/tokens/{mint}/report/summary");
        let response = self.client.get(url).send().await?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let raw: Raw = response.json().await?;
        let mut dangers = Vec::new();
        for risk in raw.risks.into_iter().flatten() {
            if risk.level.as_deref() == Some("danger") {
                dangers.push(risk.name.unwrap_or_else(|| "unnamed risk".to_string()));
            }
        }
        Ok(Some(RugcheckSummary { score: raw.score_normalised, dangers }))
    }
}
