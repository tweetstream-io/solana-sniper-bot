//! The trade state machine through its public methods: reservations, transitions, what survives a
//! restart, and what the state file must look like.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::Value;
use solana_sniper_bot::detect::types::{Actor, Namespace};
use solana_sniper_bot::execute::trades::{
    BuyRequest, ExitTarget, Mode, Outcome, Position, PositionRoute, Sent, Trade, TradeBook, TradeRoute, TradeState, Trigger,
};
use solana_sniper_bot::setup::config::Spend;

const MINT: &str = "FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA";
const OTHER: &str = "H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29";
const THIRD: &str = "7nHgAcf7tM2MuUF29mismBW2gKHBrXkGhJtU8LpqB6vN";

fn caps() -> Spend {
    Spend { max_per_trade_sol: 0.1, max_per_hour_sol: 0.25, max_total_sol: 0.25 }
}

fn trigger() -> Trigger {
    let actor = Actor { namespace: Namespace::X, id: Some("111".to_string()), handle: None };
    Trigger {
        rule: "r".to_string(),
        fire_key: "r|x".to_string(),
        event_ref: "tweet/content twitter:1".to_string(),
        post_key: Some("twitter:1".to_string()),
        actor,
    }
}

/// A state file in a fresh directory, removed when dropped.
struct StateDir(PathBuf);

impl StateDir {
    fn new() -> StateDir {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("sniper-trades-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        StateDir(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join("live.json")
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sent(id: &str) -> Sent {
    Sent {
        signature: format!("sig-{id}"),
        last_valid_block_height: 1_000,
        tx: "AAAA".to_string(),
        route: TradeRoute::Pump,
        senders: vec!["rpc".to_string()],
    }
}

fn request<'a>(mint: &'a str, max_debit: i128, skip_if_already_bought: bool, now: i64, caps: &'a Spend) -> BuyRequest<'a> {
    BuyRequest {
        mint,
        trigger: trigger(),
        route: TradeRoute::Pump,
        max_debit,
        fee_budget: 10_000_000,
        caps,
        skip_if_already_bought,
        skip_if_already_holding: true,
        now,
    }
}

fn buy(book: &TradeBook, now: i64) -> Trade {
    let caps = caps();
    book.reserve_buy(request(MINT, 60_000_000, true, now, &caps)).expect("the buy is reserved")
}

fn spent_total(path: &Path) -> String {
    let state: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    state["spend"]["totalLamports"].as_str().unwrap().to_string()
}

fn landed(token_amount: Option<u64>, spent_lamports: Option<i128>) -> Outcome {
    Outcome::Landed { token_amount, spent_lamports, signature: None }
}

fn position(buy_trade_id: &str, trigger: Trigger) -> Position {
    Position {
        mint: MINT.to_string(),
        route: PositionRoute::Pump,
        token_program: String::new(),
        opened_at: 0,
        buy_trade_id: buy_trade_id.to_string(),
        cost_lamports: 1,
        token_amount: Some("5".to_string()),
        trigger,
        sell_trade_id: None,
    }
}

#[test]
fn an_unknown_outcome_keeps_the_spend_and_blocks_a_repeat_buy_and_expiry_later_refunds_it() {
    let dir = StateDir::new();
    let path = dir.file();
    let book = TradeBook::open(Some(&path), Mode::Live).unwrap();
    let t = buy(&book, 0);
    book.mark_sending(&t.id, sent(&t.id), 1).unwrap();
    book.resolve(&t.id, Outcome::Unknown { error: "timeout".to_string() }, 2).unwrap();
    assert_eq!(spent_total(&path), "60000000");
    let caps = caps();
    let repeat = book.reserve_buy(request(MINT, 1, false, 3, &caps)).unwrap_err();
    assert!(repeat.contains("in flight"), "{repeat}");
    book.resolve(&t.id, Outcome::Expired, 4).unwrap();
    assert_eq!(book.trade(&t.id).unwrap().state, TradeState::Reconciled);
    assert_eq!(spent_total(&path), "0");
    buy(&book, 5);
}

#[test]
fn sending_is_on_disk_with_the_signature_and_signed_bytes_before_mark_sending_returns() {
    let dir = StateDir::new();
    let path = dir.file();
    let book = TradeBook::open(Some(&path), Mode::Live).unwrap();
    let t = buy(&book, 0);
    book.mark_sending(&t.id, sent(&t.id), 1).unwrap();
    let on_disk: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let trade = &on_disk["trades"][&t.id];
    assert_eq!(trade["state"], "sending");
    assert_eq!(trade["signature"], format!("sig-{}", t.id));
    assert_eq!(trade["tx"], "AAAA");
    let reopened = TradeBook::open(Some(&path), Mode::Live).unwrap();
    let ids: Vec<String> = reopened.unresolved().into_iter().map(|t| t.id).collect();
    assert_eq!(ids, vec![t.id], "a restart finds the trade to reconcile");
}

#[test]
fn a_reserved_trade_found_after_a_restart_was_never_sent_so_it_fails_and_gives_the_spend_back() {
    let dir = StateDir::new();
    let path = dir.file();
    let book = TradeBook::open(Some(&path), Mode::Live).unwrap();
    let caps = caps();
    let reserved = book.reserve_buy(BuyRequest { fee_budget: 1, ..request(OTHER, 20_000_000, true, 0, &caps) }).unwrap();
    let sending = buy(&book, 1);
    book.mark_sending(&sending.id, sent(&sending.id), 2).unwrap();
    let reopened = TradeBook::open(Some(&path), Mode::Live).unwrap();
    let closed: Vec<String> = reopened.recover_after_restart().into_iter().map(|t| t.id).collect();
    assert_eq!(closed, vec![reserved.id.clone()]);
    assert_eq!(reopened.trade(&reserved.id).unwrap().state, TradeState::Failed);
    assert_eq!(reopened.trade(&sending.id).unwrap().state, TradeState::Sending);
    assert_eq!(spent_total(&path), "60000000", "only the sending trade's reservation remains");
}

#[test]
fn a_failed_state_write_refuses_the_send_and_leaves_the_trade_reserved() {
    let dir = StateDir::new();
    let path = dir.file();
    let dir = path.parent().unwrap().to_path_buf();
    let book = TradeBook::open(Some(&path), Mode::Live).unwrap();
    let t = buy(&book, 0);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    let refused = book.mark_sending(&t.id, sent(&t.id), 1);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(refused.is_err());
    assert_eq!(book.trade(&t.id).unwrap().state, TradeState::Reserved);
    book.fail_before_send(&t.id, "state write failed");
    assert_eq!(book.trade(&t.id).unwrap().state, TradeState::Failed);
    assert_eq!(spent_total(&path), "0");
}

#[test]
fn the_spend_reservation_is_the_largest_possible_debit_settled_to_the_actual_one() {
    let book = TradeBook::open(None, Mode::Live).unwrap();
    let caps = caps();
    let over = book.reserve_buy(BuyRequest { fee_budget: 1, ..request(MINT, 100_000_001, true, 0, &caps) }).unwrap_err();
    assert!(over.contains("maxPerTradeSol"), "{over}");
    let t = book.reserve_buy(request(MINT, 100_000_000, true, 0, &caps)).unwrap();
    book.mark_sending(&t.id, sent(&t.id), 1).unwrap();
    book.resolve(&t.id, landed(Some(5), Some(52_000_000)), 2).unwrap();
    assert_eq!(book.trade(&t.id).unwrap().spent_lamports, Some(52_000_000));
    // 0.25 SOL total cap: 0.052 spent, so a 0.1 SOL reservation still fits and a third does not.
    assert!(book.reserve_buy(BuyRequest { fee_budget: 1, ..request(OTHER, 100_000_000, true, 3, &caps) }).is_ok());
    let third = book.reserve_buy(BuyRequest { fee_budget: 1, ..request(THIRD, 100_000_000, true, 4, &caps) }).unwrap_err();
    assert!(third.contains("maxPerHourSol") || third.contains("maxTotalSol"), "{third}");
}

#[test]
fn a_failed_transaction_keeps_only_its_fee_budget_as_spent() {
    let book = TradeBook::open(None, Mode::Live).unwrap();
    let t = buy(&book, 0);
    book.mark_sending(&t.id, sent(&t.id), 1).unwrap();
    book.resolve(&t.id, Outcome::Failed { error: "6002".to_string(), spent_lamports: None }, 2).unwrap();
    assert_eq!(book.trade(&t.id).unwrap().spent_lamports, Some(10_000_000));
    buy(&book, 3);
}

#[test]
fn an_exited_mint_stays_bought_and_skip_if_already_holding_blocks_while_the_position_is_open() {
    let book = TradeBook::open(None, Mode::Live).unwrap();
    let caps = caps();
    let t = buy(&book, 0);
    book.mark_sending(&t.id, sent(&t.id), 1).unwrap();
    book.resolve(&t.id, landed(Some(5), None), 2).unwrap();
    book.open_position(position(&t.id, trigger()));
    let holding = book.reserve_buy(BuyRequest { fee_budget: 0, ..request(MINT, 1, false, 3, &caps) }).unwrap_err();
    assert!(holding.contains("still holds"), "{holding}");
    let sell = book.reserve_sell(MINT, trigger(), TradeRoute::Pump, 4).unwrap();
    book.mark_sending(&sell.id, sent(&sell.id), 5).unwrap();
    book.resolve(&sell.id, landed(None, None), 6).unwrap();
    assert!(book.position(MINT).is_none());
    let bought = book.reserve_buy(BuyRequest { fee_budget: 0, ..request(MINT, 1, true, 7, &caps) }).unwrap_err();
    assert!(bought.contains("already bought"), "{bought}");
}

#[test]
fn one_sell_at_a_time_so_an_unknown_sell_blocks_the_next_until_it_resolves() {
    let book = TradeBook::open(None, Mode::Live).unwrap();
    book.open_position(position("b", trigger()));
    let first = book.reserve_sell(MINT, trigger(), TradeRoute::Pump, 1).unwrap();
    book.mark_sending(&first.id, sent(&first.id), 2).unwrap();
    book.resolve(&first.id, Outcome::Unknown { error: "timeout".to_string() }, 3).unwrap();
    let busy = book.reserve_sell(MINT, trigger(), TradeRoute::Pump, 4).unwrap_err();
    assert!(busy.contains("still unknown"), "{busy}");
    book.resolve(&first.id, Outcome::Expired, 5).unwrap();
    assert!(book.reserve_sell(MINT, trigger(), TradeRoute::Pump, 6).is_ok());
}

#[test]
fn an_exit_asked_for_during_a_pending_buy_is_returned_when_the_buy_lands() {
    let book = TradeBook::open(None, Mode::Live).unwrap();
    let t = buy(&book, 0);
    book.mark_sending(&t.id, sent(&t.id), 1).unwrap();
    assert_eq!(book.add_exit_intent(MINT, "post_deleted", "deleted", 2), ExitTarget::Pending);
    let intents = book.resolve(&t.id, landed(None, None), 3).unwrap();
    let codes: Vec<&str> = intents.iter().map(|i| i.code.as_str()).collect();
    assert_eq!(codes, vec!["post_deleted"]);
}

#[test]
fn exit_targets_match_the_triggering_account_and_not_just_the_rule() {
    let book = TradeBook::open(None, Mode::Live).unwrap();
    let caller = |handle: &str| Actor { namespace: Namespace::PumpFun, id: None, handle: Some(handle.to_string()) };
    book.open_position(position("b", Trigger { actor: caller("CallerOne"), ..trigger() }));
    assert_eq!(book.exit_targets_for_actor("r", &caller("callerone")), vec![MINT.to_string()]);
    assert!(book.exit_targets_for_actor("r", &caller("OtherCaller")).is_empty());
    assert_eq!(book.exit_targets_for_post("twitter:1"), vec![MINT.to_string()]);
}

#[test]
fn a_state_file_of_another_version_mode_or_shape_stops_startup_and_only_a_missing_file_is_created() {
    let dir = StateDir::new();
    let path = dir.file();
    drop(TradeBook::open(Some(&path), Mode::Live).unwrap());
    let valid: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let mut other_version = valid.clone();
    other_version["version"] = Value::from(999);
    fs::write(&path, other_version.to_string()).unwrap();
    assert!(TradeBook::open(Some(&path), Mode::Live).err().unwrap().0.contains("ledger.schema.json"));
    fs::write(&path, valid.to_string()).unwrap();
    assert!(TradeBook::open(Some(&path), Mode::DryRun).err().unwrap().0.contains("live state"));
    fs::write(&path, "{not json").unwrap();
    assert!(TradeBook::open(Some(&path), Mode::Live).err().unwrap().0.contains("buy the same mints again"));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let unreadable = TradeBook::open(Some(&path), Mode::Live).err().unwrap().0;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(unreadable.contains("cannot read"), "{unreadable}");
}

/// Written by hand from `shared/ledger.schema.json`, in the shape the TypeScript bot writes: a buy
/// still sending, its purchase and spend, and an open position from an earlier buy.
const V1_FILE: &str = r#"{
  "version": 1,
  "mode": "live",
  "snipes": { "dev-ca": 2 },
  "spend": { "totalLamports": "15928480", "recent": [{ "tradeId": "mg1abc-0", "at": 1772000001000, "lamports": "15928480" }] },
  "purchases": { "H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29": { "firstAt": 1771990000000, "tradeId": "mg0aaa-3" } },
  "trades": {
    "mg1abc-0": {
      "id": "mg1abc-0", "side": "buy", "mint": "FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA", "route": "pump", "state": "sending",
      "createdAt": 1772000001000,
      "trigger": { "rule": "dev-ca", "fireKey": "dev-ca|FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA", "eventRef": "tweet/content twitter:2026541002650550272",
        "postKey": "twitter:2026541002650550272", "actor": { "namespace": "x", "id": "111", "handle": "dev_one" } },
      "reservedLamports": "15928480", "feeBudgetLamports": "5928480", "signature": "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQUW",
      "lastValidBlockHeight": 300000150, "tx": "AAAA", "senders": ["mainnet.helius-rpc.com", "jito"], "sentAt": 1772000001010, "exitIntents": []
    }
  },
  "positions": {
    "H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29": {
      "mint": "H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29", "route": "pump", "tokenProgram": "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
      "openedAt": 1771990000500, "buyTradeId": "mg0aaa-3", "costLamports": "10000000", "tokenAmount": null,
      "trigger": { "rule": "caller", "fireKey": "caller|H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29", "eventRef": "account/callout c-1",
        "actor": { "namespace": "pump_fun", "handle": "CallerOne" } }
    }
  }
}"#;

#[test]
fn a_version_1_file_in_the_shared_format_loads_and_round_trips() {
    let dir = StateDir::new();
    let path = dir.file();
    fs::write(&path, V1_FILE).unwrap();
    let book = TradeBook::open(Some(&path), Mode::Live).unwrap();
    let unresolved = book.unresolved();
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0].last_valid_block_height, Some(300_000_150));
    assert_eq!(unresolved[0].reserved_lamports, 15_928_480);
    let held = book.position("H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29").unwrap();
    assert_eq!(held.token_amount, None);
    assert_eq!(
        book.exit_targets_for_actor("caller", &Actor { namespace: Namespace::PumpFun, id: None, handle: Some("callerone".to_string()) })
            .len(),
        1
    );
    let caps = Spend { max_per_trade_sol: 1.0, max_per_hour_sol: 1.0, max_total_sol: 1.0 };
    let repeat = book.reserve_buy(request("H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29", 1, true, 1772000002000, &caps)).unwrap_err();
    assert!(repeat.contains("already bought (trade mg0aaa-3)"), "{repeat}");
    assert!(!book.reserve_snipe("dev-ca", 2), "snipes used are read from the file");
    book.flush();
    let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let original: Value = serde_json::from_str(V1_FILE).unwrap();
    assert_eq!(written, original, "a load and a write change nothing");
}
