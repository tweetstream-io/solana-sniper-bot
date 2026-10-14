//! Trades end to end through the trader: chain reads from `ScenarioNetwork`, sends to a local
//! stand-in RPC over HTTP, confirmations from a local stand-in PubSub WebSocket, state in a real
//! file.

mod common;

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_sniper_bot::detect::types::{Actor, Namespace};
use solana_sniper_bot::execute::cache::ChainCache;
use solana_sniper_bot::execute::pump::bonding_curve_address;
use solana_sniper_bot::execute::trade::{SellResult, Trader, TraderDeps, compile_and_sign};
use solana_sniper_bot::execute::trades::{Mode, Position, PositionRoute, TradeBook, Trigger};
use solana_sniper_bot::exits::exits::ExitWatcher;
use solana_sniper_bot::land::land::{Lander, LanderOptions, SenderKind, SenderTarget, SignatureWatcher};
use solana_sniper_bot::network::{
    AccountData, Blockhash, EpochInfo, JupiterExecute, JupiterOrder, MarketSnapshot, NetError, NetResult, Network, OrderRequest,
    RugcheckSummary, SignatureStatus, http_client,
};
use solana_sniper_bot::report::latency::{LatencyReport, Trace};
use solana_sniper_bot::rules::modes::Fire;
use solana_sniper_bot::scenario::{ChainSpec, CurveSpec, FeeTierSpec, MintSpec, ScenarioNetwork, chain_accounts};
use solana_sniper_bot::setup::config::{Config, parse_config};
use solana_transaction::versioned::VersionedTransaction;

const PUMP_MINT: &str = "FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA";
const PLAIN_MINT: &str = "H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29";
const AUTHORITY_MINT: &str = "7nHgAcf7tM2MuUF29mismBW2gKHBrXkGhJtU8LpqB6vN";

fn curve(complete: bool) -> CurveSpec {
    CurveSpec {
        virtual_token_reserves: "1000000000000000".to_string(),
        virtual_quote_reserves: "32000000000".to_string(),
        real_token_reserves: "720000000000000".to_string(),
        real_quote_reserves: "2000000000".to_string(),
        token_total_supply: "1000000000000000".to_string(),
        complete,
    }
}

fn mint_spec(token_program: &str, mint_authority: bool, curve: Option<CurveSpec>) -> MintSpec {
    MintSpec {
        token_program: token_program.to_string(),
        mint_authority,
        freeze_authority: false,
        supply: "1000000000000000".to_string(),
        decimals: 6,
        curve,
    }
}

fn chain(pump_curve_complete: bool) -> ChainSpec {
    let mut mints = HashMap::new();
    mints.insert(PUMP_MINT.to_string(), mint_spec("token-2022", false, Some(curve(pump_curve_complete))));
    mints.insert(PLAIN_MINT.to_string(), mint_spec("spl", false, None));
    mints.insert(AUTHORITY_MINT.to_string(), mint_spec("token-2022", true, None));
    let tiers = vec![FeeTierSpec { market_cap_lamports_threshold: "0".to_string(), protocol_fee_bps: 95, creator_fee_bps: 30 }];
    ChainSpec {
        sol_usd: 150.0,
        priority_fee_micro_lamports: 10_000,
        tip_floor_sol: 0.00002,
        block_height: 1_000,
        epoch: 850,
        fee_tiers: tiers,
        mints,
    }
}

/// Jupiter answers from the test: an order for the wallet, and a chosen `/execute` result. Every
/// other call goes to the scenario.
/// What the test's Jupiter answers to `/execute`.
type Execute = Box<dyn Fn() -> NetResult<JupiterExecute> + Send + Sync>;

struct JupiterScenario {
    inner: Arc<ScenarioNetwork>,
    execute: Execute,
}

impl Network for JupiterScenario {
    async fn get_accounts(&self, addresses: &[String]) -> NetResult<Vec<Option<AccountData>>> {
        self.inner.get_accounts(addresses).await
    }
    async fn latest_blockhash(&self) -> NetResult<Blockhash> {
        self.inner.latest_blockhash().await
    }
    async fn epoch_info(&self) -> NetResult<EpochInfo> {
        self.inner.epoch_info().await
    }
    async fn signature_status(&self, signature: &str) -> NetResult<Option<SignatureStatus>> {
        self.inner.signature_status(signature).await
    }
    async fn transaction_debit(&self, signature: &str, owner: &str) -> NetResult<Option<i128>> {
        self.inner.transaction_debit(signature, owner).await
    }
    async fn pumpportal_tx(&self, body: Value) -> NetResult<Vec<u8>> {
        self.inner.pumpportal_tx(body).await
    }
    async fn jupiter_order(&self, request: OrderRequest) -> NetResult<JupiterOrder> {
        let payer = Keypair::new();
        let taker: Pubkey = request.taker.parse().unwrap();
        let transfer = solana_system_interface::instruction::transfer(&taker, &payer.pubkey(), 1);
        let message = solana_message::v0::Message::try_compile(&taker, &[transfer], &[], Hash::new_from_array([3; 32])).unwrap();
        let tx = VersionedTransaction { signatures: vec![Default::default()], message: solana_message::VersionedMessage::V0(message) };
        let transaction = base64::engine::general_purpose::STANDARD.encode(wincode::serialize(&tx).unwrap());
        Ok(JupiterOrder {
            transaction: Some(transaction),
            request_id: "req".to_string(),
            last_valid_block_height: Some(1_150),
            error_message: None,
        })
    }
    async fn jupiter_execute(&self, _signed_tx: &str, _request_id: &str, _timeout: Duration) -> NetResult<JupiterExecute> {
        (self.execute)()
    }
    async fn recent_priority_fees(&self) -> NetResult<Vec<u64>> {
        self.inner.recent_priority_fees().await
    }
    async fn jito_tip_floor_sol(&self) -> NetResult<Option<f64>> {
        self.inner.jito_tip_floor_sol().await
    }
    async fn sol_usd_price(&self) -> NetResult<Option<f64>> {
        self.inner.sol_usd_price().await
    }
    async fn market_snapshot(&self, mint: &str) -> NetResult<MarketSnapshot> {
        self.inner.market_snapshot(mint).await
    }
    async fn rugcheck(&self, mint: &str) -> NetResult<Option<RugcheckSummary>> {
        self.inner.rugcheck(mint).await
    }
    async fn quote_sell_lamports(&self, mint: &str, tokens: u64) -> NetResult<Option<u64>> {
        self.inner.quote_sell_lamports(mint, tokens).await
    }
    async fn token_balance(&self, owner: &str, mint: &str) -> NetResult<u64> {
        self.inner.token_balance(owner, mint).await
    }
}

/// One transaction an RPC stand-in received, with the trade's state on disk at that moment.
#[derive(Debug, Clone)]
struct Seen {
    base64: String,
    state_at_send: Option<String>,
    tx_at_send: Option<String>,
}

type SeenList = Arc<Mutex<Vec<Seen>>>;
type OnSend = Arc<dyn Fn(&ScenarioNetwork, &str) + Send + Sync>;

fn signature_of(base64: &str) -> String {
    let bytes = base64::engine::general_purpose::STANDARD.decode(base64).unwrap();
    let tx: VersionedTransaction = wincode::deserialize(&bytes).unwrap();
    tx.signatures[0].to_string()
}

/// An RPC stand-in that records each new transaction and the trade's state on disk at the moment
/// it arrived; `on_send` lets a test play the chain (debits, balances, curve changes).
async fn rpc_stand_in(state_path: PathBuf, network: Arc<ScenarioNetwork>, on_send: OnSend) -> (String, SeenList) {
    let seen = SeenList::default();
    let list = Arc::clone(&seen);
    let on_request = Arc::new(move |body: &Value| {
        let base64 = body["params"][0].as_str().unwrap().to_string();
        let signature = signature_of(&base64);
        let state: Value = serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
        let trade = state["trades"].as_object().unwrap().values().find(|t| t["signature"] == signature.as_str()).cloned();
        let mut list = list.lock().unwrap();
        if !list.iter().any(|s| signature_of(&s.base64) == signature) {
            let state_at_send = trade.as_ref().and_then(|t| t["state"].as_str().map(str::to_string));
            let tx_at_send = trade.as_ref().and_then(|t| t["tx"].as_str().map(str::to_string));
            list.push(Seen { base64, state_at_send, tx_at_send });
        }
        drop(list);
        on_send(&network, &signature);
    });
    (common::rpc_server(200, on_request).await, seen)
}

struct Setup<N> {
    state_path: PathBuf,
    state_dir: PathBuf,
    scenario: Arc<ScenarioNetwork>,
    book: Arc<TradeBook>,
    trader: Arc<Trader<N>>,
    cache: Arc<ChainCache>,
    config: Arc<Config>,
    rpc: SeenList,
    jito: SeenList,
    released: Arc<Mutex<Vec<String>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl<N> Drop for Setup<N> {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let _ = fs::set_permissions(&self.state_dir, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.state_dir);
    }
}

fn state_path() -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("sniper-trade-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.join("live.json")
}

async fn setup<N: Network>(
    network: Arc<N>,
    scenario: Arc<ScenarioNetwork>,
    answer: common::Answer,
    fees: Value,
    on_send: OnSend,
) -> Setup<N> {
    let state_path = state_path();
    let (rpc_url, rpc) = rpc_stand_in(state_path.clone(), Arc::clone(&scenario), on_send).await;
    let (jito_url, jito) = rpc_stand_in(state_path.clone(), Arc::clone(&scenario), Arc::new(|_: &ScenarioNetwork, _: &str| {})).await;
    let pubsub = common::pubsub(answer).await;
    let mut rule_fees = json!({ "antiMev": false, "priorityFeeSol": 0.0001, "tipSol": 0.0001 });
    for (key, value) in fees.as_object().unwrap() {
        rule_fees[key] = value.clone();
    }
    let config = parse_config(json!({
        "rules": [{ "name": "r", "mode": "ca_scanner", "accounts": ["1"], "token": { "amountSol": 0.01 }, "fees": rule_fees }],
        "execution": { "confirmTimeoutMs": 1_000, "rebroadcastMs": 10_000 }
    }))
    .unwrap();
    let config = Arc::new(config);
    let book = Arc::new(TradeBook::open(Some(&state_path), Mode::Live).unwrap());
    let cache = Arc::new(ChainCache::default());
    let mut tasks = cache.start(Arc::clone(&network), Duration::from_secs(60));
    cache.ready().await;
    while cache.fee_tiers().is_none() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (watcher, task) = SignatureWatcher::start(pubsub);
    tasks.push(task);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let senders = vec![
        SenderTarget { name: "rpc".to_string(), url: rpc_url, kind: SenderKind::Rpc, needs_tip: false },
        SenderTarget { name: "jito".to_string(), url: jito_url, kind: SenderKind::Jito, needs_tip: false },
    ];
    let height = Arc::clone(&cache);
    let options = LanderOptions {
        rebroadcast: Duration::from_secs(10),
        confirm_timeout: Duration::from_secs(1),
        current_block_height: Arc::new(move || height.block_height()),
        status_rpc_url: None,
    };
    let released = Arc::new(Mutex::new(Vec::new()));
    let release_list = Arc::clone(&released);
    let trader = Arc::new(Trader::new(TraderDeps {
        config: Arc::clone(&config),
        network,
        book: Arc::clone(&book),
        cache: Arc::clone(&cache),
        report: Arc::new(LatencyReport::default()),
        wallet: Keypair::new(),
        dry_run: false,
        lander: Some(Lander::new(http_client(), watcher, options)),
        senders,
        release_fire: Arc::new(move |fire: &Fire| release_list.lock().unwrap().push(fire.fire_key.clone())),
    }));
    let state_dir = state_path.parent().unwrap().to_path_buf();
    Setup { state_path, state_dir, scenario, book, trader, cache, config, rpc, jito, released, tasks }
}

async fn scenario_setup(answer: common::Answer, fees: Value, on_send: OnSend) -> Setup<ScenarioNetwork> {
    let scenario = Arc::new(ScenarioNetwork::new(chain(false)));
    setup(Arc::clone(&scenario), scenario, answer, fees, on_send).await
}

fn confirm() -> common::Answer {
    Arc::new(|_: &str| Some(Value::Null))
}

fn no_chain() -> OnSend {
    Arc::new(|_: &ScenarioNetwork, _: &str| {})
}

fn fire(mint: &str) -> Fire {
    Fire {
        actor: Actor { namespace: Namespace::X, id: Some("1".to_string()), handle: None },
        rule: 0,
        mint: mint.to_string(),
        signal: None,
        detail: "test".to_string(),
        fire_key: format!("r|{mint}"),
    }
}

fn trace(mint: &str) -> Trace {
    let mut trace = Trace::new(mint, "r", "test", "tweet/content twitter:1", None, 0, Instant::now());
    trace.decided = Some(trace.received);
    trace
}

fn state<N>(t: &Setup<N>) -> Value {
    serde_json::from_str(&fs::read_to_string(&t.state_path).unwrap()).unwrap()
}

fn trades<N>(t: &Setup<N>) -> Vec<Value> {
    let mut list: Vec<Value> = state(t)["trades"].as_object().unwrap().values().cloned().collect();
    list.sort_by_key(|v| v["id"].as_str().unwrap().to_string());
    list
}

#[tokio::test(flavor = "multi_thread")]
async fn a_buy_is_on_disk_as_sending_with_the_exact_bytes_the_rpc_receives_before_the_rpc_sees_it() {
    let on_send: OnSend = Arc::new(|network: &ScenarioNetwork, signature: &str| {
        network.set_debit(signature, 12_345_678);
        network.set_balance(PUMP_MINT, 300_000_000_000);
    });
    let t = scenario_setup(confirm(), json!({}), on_send).await;
    t.trader.buy(fire(PUMP_MINT), trace(PUMP_MINT)).await;
    let seen = t.rpc.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].state_at_send.as_deref(), Some("sending"));
    assert_eq!(seen[0].tx_at_send.as_deref(), Some(seen[0].base64.as_str()));
    let trade = &trades(&t)[0];
    assert_eq!(trade["state"], "confirmed");
    assert_eq!(trade["spentLamports"], "12345678", "spend settles to the actual debit");
    assert_eq!(t.book.position(PUMP_MINT).unwrap().token_amount, None, "balance not read yet");
    t.trader.reconcile().await;
    assert_eq!(t.book.position(PUMP_MINT).unwrap().token_amount.as_deref(), Some("300000000000"));
    assert!(t.released.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn no_confirmation_leaves_the_buy_unknown_with_the_spend_held_and_the_reconciler_opens_the_position_later() {
    let t = scenario_setup(Arc::new(|_: &str| None), json!({}), no_chain()).await;
    t.trader.buy(fire(PUMP_MINT), trace(PUMP_MINT)).await;
    let trade = trades(&t)[0].clone();
    assert_eq!(trade["state"], "unknown");
    assert_eq!(state(&t)["spend"]["totalLamports"], trade["reservedLamports"]);
    assert!(t.book.position(PUMP_MINT).is_none());
    assert!(t.released.lock().unwrap().is_empty(), "a sent fire is never given back");
    t.scenario.set_status(trade["signature"].as_str().unwrap(), SignatureStatus { confirmed: true, err: None });
    t.scenario.set_balance(PUMP_MINT, 5);
    t.trader.reconcile().await;
    assert_eq!(t.book.position(PUMP_MINT).unwrap().token_amount.as_deref(), Some("5"));
    assert_eq!(trades(&t)[0]["state"], "reconciled");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exit_asked_for_while_the_buy_is_pending_sells_once_the_buy_lands() {
    let on_send: OnSend = Arc::new(|network: &ScenarioNetwork, _: &str| network.set_balance(PUMP_MINT, 300_000_000_000));
    let t = scenario_setup(confirm(), json!({}), on_send).await;
    let trader = Arc::clone(&t.trader);
    let buying = tokio::spawn(async move { trader.buy(fire(PUMP_MINT), trace(PUMP_MINT)).await });
    while t.rpc.lock().unwrap().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    t.trader.exit(PUMP_MINT, "post_deleted", "deleted").await;
    buying.await.unwrap();
    assert_eq!(t.rpc.lock().unwrap().len(), 2, "the buy, then the sell");
    assert!(t.book.position(PUMP_MINT).is_none(), "the sell landed and closed the position");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_state_write_sends_nothing_and_gives_the_fire_back_once() {
    let t = scenario_setup(confirm(), json!({}), no_chain()).await;
    let dir = t.state_path.parent().unwrap().to_path_buf();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    t.trader.buy(fire(PUMP_MINT), trace(PUMP_MINT)).await;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(t.rpc.lock().unwrap().is_empty());
    assert_eq!(t.released.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn anti_mev_sends_only_through_jito_with_its_tip_and_otherwise_only_to_rpcs() {
    let protected = scenario_setup(confirm(), json!({ "antiMev": true }), no_chain()).await;
    protected.trader.buy(fire(PUMP_MINT), trace(PUMP_MINT)).await;
    assert_eq!(protected.jito.lock().unwrap().len(), 1);
    assert!(protected.rpc.lock().unwrap().is_empty());
    let open = scenario_setup(confirm(), json!({ "antiMev": false }), no_chain()).await;
    open.trader.buy(fire(PUMP_MINT), trace(PUMP_MINT)).await;
    assert_eq!(open.rpc.lock().unwrap().len(), 1);
    assert!(open.jito.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_token_gives_the_fire_back_once_and_spends_nothing() {
    let t = scenario_setup(confirm(), json!({}), no_chain()).await;
    t.trader.buy(fire(AUTHORITY_MINT), trace(AUTHORITY_MINT)).await;
    assert_eq!(t.released.lock().unwrap().len(), 1);
    assert_eq!(state(&t)["spend"]["totalLamports"], "0");
    assert_eq!(t.trader.sell(PLAIN_MINT, "rule").await, SellResult::NoPosition);
}

async fn jupiter_setup(execute: Execute, answer: common::Answer, on_send: OnSend) -> Setup<JupiterScenario> {
    let scenario = Arc::new(ScenarioNetwork::new(chain(false)));
    let network = Arc::new(JupiterScenario { inner: Arc::clone(&scenario), execute });
    setup(network, scenario, answer, json!({}), on_send).await
}

#[tokio::test(flavor = "multi_thread")]
async fn jupiter_execute_that_times_out_or_answers_minus_1001_leaves_the_buy_unknown_and_minus_1002_is_a_definite_failure() {
    let answer = |code: i64| JupiterExecute { status: "Failed".to_string(), code, signature: None, error: None, total_output_amount: None };
    let cases: Vec<(Execute, &str)> = vec![
        (Box::new(|| Err(NetError("timed out".to_string()))), "unknown"),
        (Box::new(move || Ok(answer(-1001))), "unknown"),
        (Box::new(move || Ok(answer(-1002))), "failed"),
    ];
    for (execute, expected) in cases {
        let t = jupiter_setup(execute, confirm(), no_chain()).await;
        t.trader.buy(fire(PLAIN_MINT), trace(PLAIN_MINT)).await;
        let trade = trades(&t)[0].clone();
        assert_eq!(trade["state"], expected);
        let total = if expected == "unknown" { trade["reservedLamports"].clone() } else { Value::from("0") };
        assert_eq!(state(&t)["spend"]["totalLamports"], total);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_curve_that_graduates_mid_buy_retries_through_jupiter_within_the_same_fire() {
    let success =
        Box::new(|| Ok(JupiterExecute { status: "Success".to_string(), code: 0, signature: None, error: None, total_output_amount: None }));
    let on_send: OnSend = Arc::new(|network: &ScenarioNetwork, _: &str| {
        let graduated = chain_accounts(&chain(true));
        let curve_key = bonding_curve_address(&PUMP_MINT.parse().unwrap()).to_string();
        network.set_account(&curve_key, graduated[&curve_key].clone());
    });
    let t = jupiter_setup(success, Arc::new(|_: &str| Some(json!({ "InstructionError": [2, { "Custom": 6005 }] }))), on_send).await;
    t.trader.buy(fire(PUMP_MINT), trace(PUMP_MINT)).await;
    let mut routes: Vec<(String, String)> = Vec::new();
    let mut by_created: Vec<Value> = state(&t)["trades"].as_object().unwrap().values().cloned().collect();
    by_created.sort_by_key(|v| (v["createdAt"].as_i64().unwrap(), v["id"].as_str().unwrap().len(), v["id"].as_str().unwrap().to_string()));
    for trade in by_created {
        routes.push((trade["route"].as_str().unwrap().to_string(), trade["state"].as_str().unwrap().to_string()));
    }
    let expected = vec![("pump".to_string(), "failed".to_string()), ("jupiter".to_string(), "confirmed".to_string())];
    assert_eq!(routes, expected);
    assert!(t.released.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_exit_watcher_sells_a_position_that_reached_take_profit_once() {
    let t = scenario_setup(confirm(), json!({}), no_chain()).await;
    let mut config = (*t.config).clone();
    config.exits.poll_ms = 100;
    let watcher = Arc::new(ExitWatcher::new(
        Arc::new(config),
        Arc::clone(&t.book),
        Arc::clone(&t.scenario),
        Arc::clone(&t.cache),
        Arc::clone(&t.trader),
    ));
    // 300 billion tokens are worth about 9.4 million lamports on this curve after fees; bought for 1 million.
    let trigger = Trigger {
        rule: "r".to_string(),
        fire_key: "r|x".to_string(),
        event_ref: "e".to_string(),
        post_key: None,
        actor: Actor { namespace: Namespace::X, id: Some("1".to_string()), handle: None },
    };
    let opened_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    t.book.open_position(Position {
        mint: PUMP_MINT.to_string(),
        route: PositionRoute::Pump,
        token_program: "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".to_string(),
        opened_at,
        buy_trade_id: "b".to_string(),
        cost_lamports: 1_000_000,
        token_amount: Some("300000000000".to_string()),
        trigger,
        sell_trade_id: None,
    });
    let task = watcher.start();
    tokio::time::sleep(Duration::from_millis(900)).await;
    task.abort();
    assert_eq!(t.rpc.lock().unwrap().len(), 1, "one sell, though the watcher polled several times");
    assert!(t.book.position(PUMP_MINT).is_none());
}

#[test]
fn a_signed_transfer_compiles_for_the_wallet() {
    let wallet = Keypair::new();
    let transfer = solana_system_interface::instruction::transfer(&wallet.pubkey(), &Pubkey::new_unique(), 1);
    let signed = compile_and_sign(&wallet, &[transfer], Hash::new_from_array([1; 32])).unwrap();
    assert_eq!(signed.transaction.signatures.len(), 1);
    assert!(signed.transaction.signatures[0].verify(wallet.pubkey().as_ref(), &signed.transaction.message.serialize()));
}
