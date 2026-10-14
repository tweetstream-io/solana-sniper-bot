//! Trades end to end through the trader: chain reads from `ScenarioNetwork`, sends to a local
//! stand-in RPC over HTTP, confirmations from a local stand-in PubSub WebSocket, state in a real
//! file.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_sniper_bot::execute::cache::ChainCache;
use solana_sniper_bot::execute::trade::{JupiterCode, SellResult, Trader, TraderDeps, compile_and_sign, jupiter_code};
use solana_sniper_bot::execute::trades::{ExitReason, Mode, PendingExit, TradeBook};
use solana_sniper_bot::exits::exits::ExitWatcher;
use solana_sniper_bot::land::land::{Lander, LanderOptions, SenderKind, SenderTarget, SignatureWatcher};
use solana_sniper_bot::network::{
    AccountData, Blockhash, EpochInfo, JupiterExecute, JupiterOrder, MarketSnapshot, NetError, NetResult, Network, OrderRequest,
    RugcheckSummary, SignatureStatus, http_client,
};
use solana_sniper_bot::report::latency::{LatencyReport, Trace};
use solana_sniper_bot::rules::modes::Fire;
use solana_sniper_bot::scenario::{ChainSpec, CurveSpec, FeeTierSpec, MintSpec, ScenarioNetwork};
use solana_sniper_bot::setup::config::{Config, parse_config};
use solana_transaction::versioned::VersionedTransaction;

const PUMP_MINT: &str = "FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA";
const PLAIN_MINT: &str = "H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29";
const AUTHORITY_MINT: &str = "7nHgAcf7tM2MuUF29mismBW2gKHBrXkGhJtU8LpqB6vN";
const TOKEN_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

fn curve() -> CurveSpec {
    CurveSpec {
        virtual_token_reserves: "1000000000000000".to_string(),
        virtual_quote_reserves: "32000000000".to_string(),
        real_token_reserves: "720000000000000".to_string(),
        real_quote_reserves: "2000000000".to_string(),
        token_total_supply: "1000000000000000".to_string(),
        complete: false,
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

fn chain() -> ChainSpec {
    let mut mints = HashMap::new();
    mints.insert(PUMP_MINT.to_string(), mint_spec("token-2022", false, Some(curve())));
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

/// What the test's Jupiter answers to `/execute`.
type Execute = Box<dyn Fn() -> NetResult<JupiterExecute> + Send + Sync>;

/// Jupiter answers from the test: an order for the wallet, and a chosen `/execute` result (or no
/// answer at all). Every other call goes to the scenario.
struct JupiterScenario {
    inner: Arc<ScenarioNetwork>,
    execute: Execute,
    hang: bool,
    market_maker_signs_first: bool,
    /// A signature that lands on chain at the moment the blockhash is checked.
    lands_at_validity_check: Mutex<Option<String>>,
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
    async fn is_blockhash_valid(&self, blockhash: &str) -> NetResult<bool> {
        if let Some(signature) = self.lands_at_validity_check.lock().unwrap().take() {
            self.inner.set_status(&signature, SignatureStatus { confirmed: true, err: None });
        }
        self.inner.is_blockhash_valid(blockhash).await
    }
    async fn transaction_debit(&self, signature: &str, owner: &str) -> NetResult<Option<i128>> {
        self.inner.transaction_debit(signature, owner).await
    }
    async fn jupiter_order(&self, request: OrderRequest) -> NetResult<JupiterOrder> {
        let maker = Keypair::new();
        let taker: Pubkey = request.taker.parse().unwrap();
        let (payer, other) = if self.market_maker_signs_first { (maker.pubkey(), taker) } else { (taker, maker.pubkey()) };
        let mut transfer = solana_system_interface::instruction::transfer(&other, &payer, 1);
        transfer.accounts[0].is_signer = true;
        let message = solana_message::v0::Message::try_compile(&payer, &[transfer], &[], Hash::new_from_array([3; 32])).unwrap();
        let signatures = vec![Default::default(); 2];
        let tx = VersionedTransaction { signatures, message: solana_message::VersionedMessage::V0(message) };
        let transaction = base64::engine::general_purpose::STANDARD.encode(wincode::serialize(&tx).unwrap());
        Ok(JupiterOrder {
            transaction: Some(transaction),
            request_id: "req".to_string(),
            last_valid_block_height: Some(1_150),
            error_message: None,
        })
    }
    async fn jupiter_execute(&self, _signed_tx: &str, _request_id: &str, _timeout: Duration) -> NetResult<JupiterExecute> {
        if self.hang {
            std::future::pending::<()>().await;
        }
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

/// One transaction an RPC stand-in received, with its pending entry on disk at that moment.
#[derive(Debug, Clone)]
struct Seen {
    transaction: VersionedTransaction,
    pending_at_send: Option<Value>,
}

type SeenList = Arc<Mutex<Vec<Seen>>>;
type OnSend = Arc<dyn Fn(&ScenarioNetwork, &str) + Send + Sync>;

fn decode(base64: &str) -> VersionedTransaction {
    wincode::deserialize(&base64::engine::general_purpose::STANDARD.decode(base64).unwrap()).unwrap()
}

/// An RPC stand-in that records each new transaction and its pending entry on disk at the moment
/// it arrived; `on_send` lets a test play the chain (debits, balances).
async fn rpc_stand_in(state_path: PathBuf, network: Arc<ScenarioNetwork>, on_send: OnSend) -> (String, SeenList) {
    let seen = SeenList::default();
    let list = Arc::clone(&seen);
    let on_request = Arc::new(move |body: &Value| {
        let transaction = decode(body["params"][0].as_str().unwrap());
        let signature = transaction.signatures[0].to_string();
        let state: Value = serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
        let pending = state["pending"].as_object().unwrap().values().find(|p| p["signature"] == signature.as_str()).cloned();
        let mut list = list.lock().unwrap();
        if !list.iter().any(|s| s.transaction.signatures[0].to_string() == signature) {
            list.push(Seen { transaction, pending_at_send: pending });
        }
        drop(list);
        on_send(&network, &signature);
    });
    (common::rpc_server(200, on_request).await, seen)
}

struct Setup<N> {
    state_path: PathBuf,
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
        self.book.close();
        let dir = self.state_path.parent().unwrap();
        if let Err(error) = fs::remove_dir_all(dir) {
            eprintln!("cannot remove {}: {error}", dir.display());
        }
    }
}

fn state_path() -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("sniper-trade-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.join("live.json")
}

/// A live state file holding one position in `mint`, bought for 1,000,000 lamports.
fn holding(mint: &str, token_amount: Option<&str>, pending_exits: Value) -> Value {
    let opened_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    json!({
        "version": 2, "mode": "live", "purchases": { mint: { "rule": "r", "at": opened_at } },
        "spend": { "totalLamports": "0", "recent": [] }, "pending": {},
        "positions": { mint: {
            "rule": "r", "trigger": { "eventRef": "e", "postKey": "twitter:1" }, "tokenProgram": TOKEN_2022,
            "openedAt": opened_at, "costLamports": "1000000", "tokenAmount": token_amount, "pendingExits": pending_exits
        } }
    })
}

struct Options {
    answer: common::Answer,
    fees: Value,
    on_send: OnSend,
    initial: Option<Value>,
}

fn options() -> Options {
    Options { answer: confirm(), fees: json!({}), on_send: no_chain(), initial: None }
}

async fn setup<N: Network>(network: Arc<N>, scenario: Arc<ScenarioNetwork>, options: Options) -> Setup<N> {
    let Options { answer, fees, on_send, initial } = options;
    let state_path = state_path();
    if let Some(initial) = initial {
        fs::write(&state_path, initial.to_string()).unwrap();
    }
    let (rpc_url, rpc) = rpc_stand_in(state_path.clone(), Arc::clone(&scenario), on_send).await;
    let (jito_url, jito) = rpc_stand_in(state_path.clone(), Arc::clone(&scenario), no_chain()).await;
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
        SenderTarget { name: "rpc".to_string(), url: rpc_url, kind: SenderKind::Rpc },
        SenderTarget { name: "jito".to_string(), url: jito_url, kind: SenderKind::Jito },
    ];
    let height = Arc::clone(&cache);
    let lander_options = LanderOptions {
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
        lander: Some(Lander::new(http_client(), watcher, lander_options)),
        senders,
        release_fire: Arc::new(move |fire: &Fire| release_list.lock().unwrap().push(fire.fire_key.clone())),
    }));
    Setup { state_path, scenario, book, trader, cache, config, rpc, jito, released, tasks }
}

async fn scenario_setup(options: Options) -> Setup<ScenarioNetwork> {
    let scenario = Arc::new(ScenarioNetwork::new(chain()));
    setup(Arc::clone(&scenario), scenario, options).await
}

fn confirm() -> common::Answer {
    Arc::new(|_: &str| Some(Value::Null))
}

fn silent() -> common::Answer {
    Arc::new(|_: &str| None)
}

fn no_chain() -> OnSend {
    Arc::new(|_: &ScenarioNetwork, _: &str| {})
}

fn fire(mint: &str) -> Fire {
    Fire { rule: 0, mint: mint.to_string(), signal: None, detail: "test".to_string(), fire_key: format!("r|{mint}") }
}

fn trace(mint: &str) -> Trace {
    let mut trace = Trace::new(mint, "r", "test", "tweet/content twitter:1", None, 0, Instant::now());
    trace.decided = Some(trace.received);
    trace
}

async fn buy<N: Network>(t: &Setup<N>, mint: &str) {
    if let Some(reserved) = t.trader.reserve_buy(&fire(mint), &trace(mint)) {
        t.trader.buy(fire(mint), reserved, trace(mint)).await;
    }
}

fn state<N>(t: &Setup<N>) -> Value {
    serde_json::from_str(&fs::read_to_string(&t.state_path).unwrap()).unwrap()
}

/// The Jito tip transfers in a transaction: system transfers to one of the tip accounts.
fn jito_tips(tx: &VersionedTransaction) -> usize {
    let accounts: Vec<Pubkey> = serde_json::from_str::<Value>(include_str!("../../shared/trading.json")).unwrap()["jitoTipAccounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().parse().unwrap())
        .collect();
    let keys = tx.message.static_account_keys();
    let system = Pubkey::default();
    tx.message
        .instructions()
        .iter()
        .filter(|ix| keys[usize::from(ix.program_id_index)] == system && accounts.contains(&keys[usize::from(ix.accounts[1])]))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_buy_is_on_disk_as_pending_with_its_signature_before_the_rpc_sees_it_and_lands_as_a_position() {
    let on_send: OnSend = Arc::new(|network: &ScenarioNetwork, signature: &str| {
        network.set_debit(signature, 12_345_678);
        network.set_balance(PUMP_MINT, 300_000_000_000);
    });
    let t = scenario_setup(Options { on_send, ..options() }).await;
    buy(&t, PUMP_MINT).await;
    let seen = t.rpc.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let pending = seen[0].pending_at_send.clone().expect("pending on disk before the send");
    assert_eq!(pending["blockhash"], seen[0].transaction.message.recent_blockhash().to_string().as_str());
    assert_eq!(pending["amountLamports"], "10000000");
    let state = state(&t);
    assert_eq!(state["pending"], json!({}));
    assert_eq!(state["spend"]["totalLamports"], "12345678", "spend settles to the actual debit");
    assert_eq!(state["positions"][PUMP_MINT]["costLamports"], "10000000");
    assert_eq!(t.book.position(PUMP_MINT).unwrap().token_amount, None, "balance not read yet");
    t.trader.reconcile().await;
    assert_eq!(t.book.position(PUMP_MINT).unwrap().token_amount, Some(300_000_000_000));
    assert!(t.released.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pump_reservation_is_the_documented_maximum_debit() {
    let t = scenario_setup(Options { answer: silent(), ..options() }).await;
    buy(&t, PUMP_MINT).await;
    let pending = state(&t)["pending"].as_object().unwrap().values().next().cloned().unwrap();
    // 10,000,000 + 5,000 signature + 100,000 priority (0.0001 SOL) + 2,074,080 token account
    // + 1,844,400 volume accumulator; the scenario curve is 115 bytes, so no top-up.
    assert_eq!(pending["reservedLamports"], "14023480");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_confirmation_leaves_the_buy_pending_and_the_reconciler_opens_the_position_once_confirmed() {
    let t = scenario_setup(Options { answer: silent(), ..options() }).await;
    buy(&t, PUMP_MINT).await;
    let pending = state(&t)["pending"].as_object().unwrap().values().next().cloned().unwrap();
    assert_eq!(state(&t)["spend"]["totalLamports"], pending["reservedLamports"]);
    assert!(t.book.position(PUMP_MINT).is_none());
    assert!(t.released.lock().unwrap().is_empty(), "a sent fire is never given back");
    t.scenario.set_status(pending["signature"].as_str().unwrap(), SignatureStatus { confirmed: true, err: None });
    t.scenario.set_balance(PUMP_MINT, 5);
    t.trader.reconcile().await;
    assert_eq!(t.book.position(PUMP_MINT).unwrap().token_amount, Some(5));
    assert_eq!(state(&t)["pending"], json!({}));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_buy_expires_only_when_its_signature_is_unknown_and_its_blockhash_is_invalid() {
    let t = scenario_setup(Options { answer: silent(), ..options() }).await;
    buy(&t, PUMP_MINT).await;
    t.scenario.set_block_height(10_000);
    t.trader.reconcile().await;
    assert_eq!(state(&t)["pending"].as_object().unwrap().len(), 1, "past its height, but the blockhash still answers valid");
    t.scenario.set_blockhash_valid(false);
    t.trader.reconcile().await;
    let state = state(&t);
    assert_eq!(state["pending"], json!({}));
    assert_eq!(state["spend"]["totalLamports"], "0", "refunded");
    assert_eq!(state["purchases"], json!({}));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_state_write_sends_nothing_and_gives_the_fire_back() {
    let t = scenario_setup(options()).await;
    let blocker = t.state_path.with_extension("json.tmp");
    fs::create_dir(&blocker).unwrap();
    buy(&t, PUMP_MINT).await;
    fs::remove_dir(&blocker).unwrap();
    assert!(t.rpc.lock().unwrap().is_empty());
    assert_eq!(*t.released.lock().unwrap(), vec![format!("r|{PUMP_MINT}")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_reconciler_does_not_see_a_pending_trade_until_its_write_completed() {
    let t = scenario_setup(Options { answer: silent(), ..options() }).await;
    let fifo = t.state_path.with_extension("json.tmp");
    assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
    let trader = Arc::clone(&t.trader);
    let reserved = trader.reserve_buy(&fire(PUMP_MINT), &trace(PUMP_MINT)).unwrap();
    let buying = tokio::spawn(async move { trader.buy(fire(PUMP_MINT), reserved, trace(PUMP_MINT)).await });
    // The write now waits on the FIFO for a reader: the trade is in memory, not yet on disk.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (busy, visible) = (t.book.is_busy(PUMP_MINT), t.book.pending().len());
    let reader = fifo.clone();
    let drained = tokio::task::spawn_blocking(move || fs::read(&reader).unwrap());
    buying.await.unwrap();
    drained.await.unwrap();
    fs::remove_file(&fifo).unwrap();
    assert!(busy, "the buy was in flight");
    assert_eq!(visible, 0, "the reconciler must not see a trade before its pending write completed");
    // fsync on a FIFO fails, so the write fails: nothing is sent and the fire comes back.
    assert!(t.rpc.lock().unwrap().is_empty());
    assert_eq!(t.released.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn anti_mev_sends_only_through_jito_with_its_tip_and_otherwise_only_to_rpcs_without_one() {
    let protected = scenario_setup(Options { fees: json!({ "antiMev": true }), ..options() }).await;
    buy(&protected, PUMP_MINT).await;
    let jito = protected.jito.lock().unwrap().clone();
    assert_eq!(jito.len(), 1);
    assert_eq!(jito_tips(&jito[0].transaction), 1, "the Jito transaction carries the tip");
    assert!(protected.rpc.lock().unwrap().is_empty());
    let open = scenario_setup(Options { fees: json!({ "antiMev": false }), ..options() }).await;
    buy(&open, PUMP_MINT).await;
    let rpc = open.rpc.lock().unwrap().clone();
    assert_eq!(rpc.len(), 1);
    assert_eq!(jito_tips(&rpc[0].transaction), 0, "no tip when Jito is not used");
    assert!(open.jito.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_token_gives_the_fire_back_and_spends_nothing() {
    let t = scenario_setup(options()).await;
    buy(&t, AUTHORITY_MINT).await;
    assert!(t.rpc.lock().unwrap().is_empty() && t.jito.lock().unwrap().is_empty(), "nothing sent");
    assert_eq!(t.released.lock().unwrap().len(), 1);
    assert_eq!(state(&t)["spend"]["totalLamports"], "0");
    assert!(t.trader.reserve_buy(&fire(AUTHORITY_MINT), &trace(AUTHORITY_MINT)).is_some(), "not bought, so it may fire again");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mint_already_bought_is_refused_at_once_and_its_fire_given_back() {
    let t = scenario_setup(Options { initial: Some(holding(PUMP_MINT, None, json!([]))), ..options() }).await;
    assert!(t.trader.reserve_buy(&fire(PUMP_MINT), &trace(PUMP_MINT)).is_none());
    assert_eq!(t.released.lock().unwrap().len(), 1);
}

fn jupiter_network(execute: Execute, hang: bool, market_maker_signs_first: bool) -> (Arc<JupiterScenario>, Arc<ScenarioNetwork>) {
    let scenario = Arc::new(ScenarioNetwork::new(chain()));
    let lands_at_validity_check = Mutex::new(None);
    let network = JupiterScenario { inner: Arc::clone(&scenario), execute, hang, market_maker_signs_first, lands_at_validity_check };
    (Arc::new(network), scenario)
}

async fn jupiter_setup(execute: Execute, hang: bool, market_maker_signs_first: bool, initial: Option<Value>) -> Setup<JupiterScenario> {
    let (network, scenario) = jupiter_network(execute, hang, market_maker_signs_first);
    setup(network, scenario, Options { initial, ..options() }).await
}

fn jupiter_answer(status: &str, code: i64, signature: Option<&str>) -> Execute {
    let (status, signature) = (status.to_string(), signature.map(str::to_string));
    Box::new(move || Ok(JupiterExecute { status: status.clone(), code, signature: signature.clone(), error: None }))
}

/// A position in a mint that is not on a curve, so its sell goes through Jupiter.
fn graduated() -> Option<Value> {
    Some(holding(PLAIN_MINT, Some("1000"), json!([])))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_coin_off_the_pump_fun_curve_is_not_bought_and_its_fire_comes_back() {
    let t = scenario_setup(options()).await;
    buy(&t, PLAIN_MINT).await;
    assert!(t.rpc.lock().unwrap().is_empty() && t.jito.lock().unwrap().is_empty(), "nothing sent");
    assert_eq!(state(&t)["pending"], json!({}));
    assert_eq!(t.released.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn jupiter_sell_answers_map_through_the_shared_codes_and_a_success_must_say_so_with_a_signature() {
    for case in serde_json::from_str::<Value>(include_str!("../../shared/trading.json")).unwrap()["jupiterExecuteCodes"]["rejected"]
        .as_array()
        .unwrap()
    {
        assert_eq!(jupiter_code(case.as_i64().unwrap()), JupiterCode::Rejected);
    }
    assert_eq!(jupiter_code(0), JupiterCode::Landed);
    assert_eq!(jupiter_code(-1000), JupiterCode::Unknown);
    assert_eq!(jupiter_code(-2001), JupiterCode::Unknown);

    let cases: Vec<(Execute, bool, usize, &str)> = vec![
        (Box::new(|| Err(NetError("timed out".to_string()))), true, 1, "a timeout stays pending"),
        (jupiter_answer("Failed", -1001, None), true, 1, "an unknown code stays pending"),
        (jupiter_answer("Failed", -1002, None), true, 0, "a rejection ends the sell, the position stays"),
        (jupiter_answer("Failed", 0, Some("5sig")), true, 1, "code 0 without Success stays pending"),
        (jupiter_answer("Success", 0, Some("5sig")), false, 0, "a success sells the position"),
    ];
    for (execute, held, pending, what) in cases {
        let t = jupiter_setup(execute, false, false, graduated()).await;
        assert_eq!(t.trader.sell(PLAIN_MINT, "test").await == SellResult::Sold, !held, "{what}");
        let state = state(&t);
        assert_eq!(state["pending"].as_object().unwrap().len(), pending, "{what}");
        assert_eq!(state["positions"].get(PLAIN_MINT).is_some(), held, "{what}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn jupiter_stores_the_signature_it_returns_and_a_sell_without_one_of_ours_waits_for_review() {
    let t = jupiter_setup(jupiter_answer("Failed", -1001, Some("5returned")), false, false, graduated()).await;
    t.trader.sell(PLAIN_MINT, "test").await;
    let pending = state(&t)["pending"].as_object().unwrap().values().next().cloned().unwrap();
    assert_eq!(pending["signature"], "5returned");

    let t = jupiter_setup(jupiter_answer("Failed", -1001, None), false, true, graduated()).await;
    t.trader.sell(PLAIN_MINT, "test").await;
    let pending = state(&t)["pending"].as_object().unwrap().values().next().cloned().unwrap();
    assert_eq!(pending["signature"], Value::Null, "the market maker signs first, so the transaction has none of ours yet");
    t.scenario.set_blockhash_valid(false);
    t.trader.reconcile().await;
    assert_eq!(state(&t)["pending"].as_object().unwrap().len(), 1, "it waits for manual review");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sell_cancelled_while_jupiter_answers_is_left_to_the_reconciler() {
    let t = jupiter_setup(jupiter_answer("Success", 0, None), true, false, graduated()).await;
    let trader = Arc::clone(&t.trader);
    let selling = tokio::spawn(async move { trader.sell(PLAIN_MINT, "test").await });
    let signature = loop {
        let pending = state(&t)["pending"].as_object().unwrap().values().next().cloned();
        if let Some(pending) = pending {
            break pending["signature"].as_str().unwrap().to_string();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    selling.abort();
    assert!(selling.await.unwrap_err().is_cancelled());
    t.scenario.set_status(&signature, SignatureStatus { confirmed: true, err: None });
    t.trader.reconcile().await;
    assert!(t.book.position(PLAIN_MINT).is_none(), "the cancelled sell is no longer marked as in progress");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trade_that_lands_while_its_expiry_is_checked_is_found_and_not_expired() {
    let (network, scenario) = jupiter_network(Box::new(|| Err(NetError("no answer".to_string()))), false, false);
    let t = setup(Arc::clone(&network), scenario, Options { answer: silent(), ..options() }).await;
    buy(&t, PUMP_MINT).await;
    let pending = state(&t)["pending"].as_object().unwrap().values().next().cloned().expect("no confirmation, so pending");
    t.scenario.set_blockhash_valid(false);
    // The blockhash is already invalid, and the transaction lands just as that is read.
    *network.lands_at_validity_check.lock().unwrap() = Some(pending["signature"].as_str().unwrap().to_string());
    t.trader.reconcile().await;
    assert!(t.book.position(PUMP_MINT).is_some(), "the landed buy opened its position");
    assert_eq!(state(&t)["pending"], json!({}));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delete_exit_stays_on_the_position_until_a_sell_confirms_and_the_reconciler_retries_it() {
    let failing_sell = Arc::new(|_: &str| Some(json!({ "InstructionError": [2, { "Custom": 6003 }] })));
    let held = holding(PUMP_MINT, Some("300000000000"), json!([]));
    let t = scenario_setup(Options { answer: failing_sell, initial: Some(held), ..options() }).await;
    t.trader.exit(PUMP_MINT, PendingExit { code: ExitReason::PostDeleted, at: 7 }).await;
    assert_eq!(t.rpc.lock().unwrap().len(), 1, "the sell went out and failed");
    assert_eq!(state(&t)["positions"][PUMP_MINT]["pendingExits"], json!([{ "code": "post_deleted", "at": 7 }]));
    t.trader.reconcile().await;
    assert_eq!(t.rpc.lock().unwrap().len(), 2, "the reconciler sells again");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_exit_watcher_sells_a_position_that_reached_take_profit_once() {
    let t = scenario_setup(Options { initial: Some(holding(PUMP_MINT, Some("300000000000"), json!([]))), ..options() }).await;
    let mut config = (*t.config).clone();
    config.exits.poll_ms = 100;
    config.exits.take_profit_pct = Some(100.0);
    let watcher = Arc::new(ExitWatcher::new(
        Arc::new(config),
        Arc::clone(&t.book),
        Arc::clone(&t.scenario),
        Arc::clone(&t.cache),
        Arc::clone(&t.trader),
    ));
    // 300 billion tokens are worth about 9.4 million lamports on this curve after fees; bought for 1 million.
    let task = watcher.start();
    tokio::time::sleep(Duration::from_millis(900)).await;
    task.abort();
    assert_eq!(t.rpc.lock().unwrap().len(), 1, "one sell, though the watcher polled several times");
    assert!(t.book.position(PUMP_MINT).is_none());
    assert_eq!(t.trader.sell(PUMP_MINT, "again").await, SellResult::NoPosition);
}

#[test]
fn a_signed_transfer_compiles_for_the_wallet() {
    let wallet = Keypair::new();
    let transfer = solana_system_interface::instruction::transfer(&wallet.pubkey(), &Pubkey::new_unique(), 1);
    let signed = compile_and_sign(&wallet, &[transfer], Hash::new_from_array([1; 32])).unwrap();
    assert_eq!(signed.transaction.signatures.len(), 1);
    assert!(signed.transaction.signatures[0].verify(wallet.pubkey().as_ref(), &signed.transaction.message.serialize()));
}
