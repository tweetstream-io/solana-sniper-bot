//! The trade book against real files: the lock, loading, one buy per mint, spend caps at the
//! pending write, outcomes, and exits.

use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};
use solana_sniper_bot::execute::trades::{
    BuyRequest, ExitReason, ExitTarget, Mode, PendingExit, ReserveError, Sent, Side, Ticket, TradeBook, TradeOutcome, TradeRoute, Trigger,
};
use solana_sniper_bot::setup::config::Spend;

const MINT: &str = "FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA";
const OTHER: &str = "H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// A fresh directory, removed when dropped.
struct StateDir(PathBuf);

impl StateDir {
    fn new() -> StateDir {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("sniper-trades-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        StateDir(dir)
    }

    fn live(&self) -> PathBuf {
        self.0.join("live.json")
    }

    fn read(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.live()).unwrap()).unwrap()
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("cannot remove {}: {error}", self.0.display());
        }
    }
}

fn caps(per_trade: f64, per_hour: f64, total: f64) -> Spend {
    Spend { max_per_trade_sol: per_trade, max_per_hour_sol: per_hour, max_total_sol: total }
}

fn wide() -> Spend {
    caps(10.0, 10.0, 10.0)
}

fn reserve(book: &TradeBook, mint: &str, rule: &str) -> Result<Ticket, ReserveError> {
    let trigger = Trigger { event_ref: "tweet/content twitter:1".to_string(), post_key: Some("twitter:1".to_string()) };
    book.reserve_buy(BuyRequest { mint, rule, fire_key: mint, trigger, now: 1_000 })
}

fn sent(reserved: u64, amount: u64) -> Sent {
    Sent {
        route: TradeRoute::Pump,
        signature: Some("sig".to_string()),
        blockhash: "hash".to_string(),
        last_valid_block_height: Some(1_150),
        token_program: TOKEN_PROGRAM.to_string(),
        reserved_lamports: reserved,
        amount_lamports: amount,
    }
}

fn landed(spent: Option<u64>) -> TradeOutcome {
    TradeOutcome::Landed { token_amount: Some(500), spent_lamports: spent }
}

/// Reserves, writes pending, and lands a buy of `mint` for 14,000,000 lamports reserved.
fn bought(book: &TradeBook, mint: &str) -> Ticket {
    let ticket = reserve(book, mint, "dev").unwrap();
    book.mark_pending(&ticket, sent(14_000_000, 10_000_000), &wide(), 1_000).unwrap();
    book.resolve(&ticket.id, &landed(Some(12_000_000)), 2_000);
    ticket
}

#[test]
fn a_second_process_cannot_open_a_state_file_in_use_and_a_closed_book_releases_it() {
    let dir = StateDir::new();
    let first = TradeBook::open(Some(&dir.live()), Mode::Live).unwrap();
    let error = TradeBook::open(Some(&dir.live()), Mode::Live).err().expect("the lock is held").0;
    assert!(error.contains(&format!("process {}, which is running", std::process::id())), "{error}");
    first.close();
    assert!(!dir.0.join("live.lock").exists());
    TradeBook::open(Some(&dir.live()), Mode::Live).expect("free after close");
}

#[test]
fn a_lock_left_behind_is_never_taken_over_and_the_message_names_its_owner() {
    let dir = StateDir::new();
    let lock = dir.0.join("live.lock");
    let cases =
        [(r#"{"pid": 2147483646, "startedAt": 1}"#, "process 2147483646, which is not running"), ("not json", "an unknown process")];
    for (leftover, owner) in cases {
        fs::write(&lock, leftover).unwrap();
        let error = TradeBook::open(Some(&dir.live()), Mode::Live).err().expect("refused").0;
        assert!(error.contains(owner) && error.contains("then delete"), "{error}");
        assert_eq!(fs::read_to_string(&lock).unwrap(), leftover, "the lock is left as it was");
    }
    fs::remove_file(&lock).unwrap();
    let book = TradeBook::open(Some(&dir.live()), Mode::Live).unwrap();
    let held: Value = serde_json::from_str(&fs::read_to_string(&lock).unwrap()).unwrap();
    assert_eq!(held["pid"], std::process::id());
    assert!(held["startedAt"].as_i64().unwrap() > 0);
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 2, "no temp file is left next to the lock and the state");
    book.close();
}

#[test]
fn a_version_1_file_or_the_other_modes_file_stops_startup() {
    let dir = StateDir::new();
    fs::write(dir.live(), json!({ "version": 1, "mode": "live", "snipes": {} }).to_string()).unwrap();
    let error = TradeBook::open(Some(&dir.live()), Mode::Live).err().expect("version 1 is refused").0;
    assert!(error.contains("older version") && error.contains("Move or delete"), "{error}");
    fs::remove_file(dir.live()).unwrap();
    TradeBook::open(Some(&dir.live()), Mode::DryRun).unwrap().close();
    let error = TradeBook::open(Some(&dir.live()), Mode::Live).err().expect("other mode is refused").0;
    assert!(error.contains("dry_run state"), "{error}");
}

#[test]
fn a_mint_is_bought_once_ever_and_refused_while_its_buy_is_reserved_or_pending() {
    let book = TradeBook::open(None, Mode::Live).unwrap();
    let ticket = reserve(&book, MINT, "dev").unwrap();
    assert_eq!(reserve(&book, MINT, "dev"), Err(ReserveError::Busy(Side::Buy)), "reserved");
    book.mark_pending(&ticket, sent(14_000_000, 10_000_000), &wide(), 1_000).unwrap();
    assert_eq!(reserve(&book, MINT, "other"), Err(ReserveError::Busy(Side::Buy)), "pending");
    book.resolve(&ticket.id, &landed(None), 2_000);
    assert_eq!(reserve(&book, MINT, "dev"), Err(ReserveError::AlreadyBought), "held");
    let sell = book.reserve_sell(MINT, 3_000).unwrap();
    book.mark_pending(&sell, sent(0, 0), &wide(), 3_000).unwrap();
    book.resolve(&sell.id, &landed(None), 4_000);
    assert!(book.position(MINT).is_none());
    assert_eq!(reserve(&book, MINT, "dev"), Err(ReserveError::AlreadyBought), "sold, and still bought once");
}

#[test]
fn a_buy_that_never_landed_can_be_bought_again_and_is_refunded() {
    let dir = StateDir::new();
    let book = TradeBook::open(Some(&dir.live()), Mode::Live).unwrap();
    let ticket = reserve(&book, MINT, "dev").unwrap();
    book.mark_pending(&ticket, sent(14_000_000, 10_000_000), &wide(), 1_000).unwrap();
    assert_eq!(dir.read()["spend"]["totalLamports"], "14000000");
    book.resolve(&ticket.id, &TradeOutcome::NotLanded { error: "expired".to_string() }, 2_000);
    let state = dir.read();
    assert_eq!(state["spend"], json!({ "totalLamports": "0", "recent": [] }));
    assert_eq!(state["pending"], json!({}));
    assert_eq!(state["purchases"], json!({}));
    assert!(reserve(&book, MINT, "dev").is_ok());
}

#[test]
fn an_unknown_outcome_keeps_the_trade_pending_with_its_spend() {
    let dir = StateDir::new();
    let book = TradeBook::open(Some(&dir.live()), Mode::Live).unwrap();
    let ticket = reserve(&book, MINT, "dev").unwrap();
    book.mark_pending(&ticket, sent(14_000_000, 10_000_000), &wide(), 1_000).unwrap();
    book.resolve(&ticket.id, &TradeOutcome::Unknown { error: "no answer".to_string() }, 2_000);
    let state = dir.read();
    assert_eq!(state["pending"][&ticket.id]["signature"], "sig");
    assert_eq!(state["spend"]["totalLamports"], "14000000");
    assert_eq!(book.pending().len(), 1);
}

#[test]
fn a_landed_buy_is_one_write_that_removes_pending_records_the_purchase_and_opens_the_position_at_its_amount() {
    let dir = StateDir::new();
    let book = TradeBook::open(Some(&dir.live()), Mode::Live).unwrap();
    let ticket = bought(&book, MINT);
    let state = dir.read();
    assert_eq!(state["pending"], json!({}));
    assert_eq!(state["purchases"][MINT], json!({ "rule": "dev", "at": 2_000 }));
    assert_eq!(state["spend"]["totalLamports"], "12000000", "spend settles to the actual debit");
    assert_eq!(state["spend"]["recent"][0], json!({ "tradeId": ticket.id, "at": 1_000, "lamports": "12000000" }));
    let position = &state["positions"][MINT];
    assert_eq!(position["costLamports"], "10000000", "the cost is the amount spent on the token");
    assert_eq!(position["tokenAmount"], "500");
    assert_eq!(position["tokenProgram"], TOKEN_PROGRAM);
    assert_eq!(position["trigger"], json!({ "eventRef": "tweet/content twitter:1", "postKey": "twitter:1" }));
}

#[test]
fn a_buy_that_landed_with_an_error_is_not_a_purchase_and_keeps_its_reservation_when_the_debit_is_unknown() {
    let book = TradeBook::open(None, Mode::Live).unwrap();
    let dir = StateDir::new();
    let book_on_disk = TradeBook::open(Some(&dir.live()), Mode::Live).unwrap();
    for (book, spent, total) in [(&book, None, None), (&book_on_disk, Some(9_000), Some("9000"))] {
        let ticket = reserve(book, MINT, "dev").unwrap();
        book.mark_pending(&ticket, sent(14_000_000, 10_000_000), &wide(), 1_000).unwrap();
        book.resolve(&ticket.id, &TradeOutcome::Failed { error: "6002".to_string(), spent_lamports: spent }, 2_000);
        assert!(book.position(MINT).is_none());
        assert!(reserve(book, MINT, "dev").is_ok(), "not a purchase");
        if let Some(total) = total {
            assert_eq!(dir.read()["spend"]["totalLamports"], total);
        }
    }
}

#[test]
fn the_spend_caps_are_checked_at_the_pending_write_and_a_refused_buy_writes_nothing() {
    let dir = StateDir::new();
    let book = TradeBook::open(Some(&dir.live()), Mode::Live).unwrap();
    let ticket = reserve(&book, MINT, "dev").unwrap();
    let error = book.mark_pending(&ticket, sent(20_000_000, 10_000_000), &caps(0.015, 1.0, 1.0), 1_000).unwrap_err();
    assert!(error.contains("maxPerTradeSol"), "{error}");
    assert_eq!(dir.read()["pending"], json!({}));
    let again = reserve(&book, MINT, "dev").expect("the refused ticket is gone");
    book.cancel(&again);

    let first = reserve(&book, OTHER, "dev").unwrap();
    book.mark_pending(&first, sent(14_000_000, 10_000_000), &caps(1.0, 0.02, 1.0), 1_000).unwrap();
    let second = reserve(&book, MINT, "dev").unwrap();
    let error = book.mark_pending(&second, sent(14_000_000, 10_000_000), &caps(1.0, 0.02, 1.0), 2_000).unwrap_err();
    assert!(error.contains("maxPerHourSol"), "{error}");
    let third = reserve(&book, MINT, "dev").unwrap();
    book.mark_pending(&third, sent(14_000_000, 10_000_000), &caps(1.0, 0.02, 1.0), 1_000 + 3_600_000).expect("an hour later");
    let fourth = reserve(&book, "7nHgAcf7tM2MuUF29mismBW2gKHBrXkGhJtU8LpqB6vN", "dev").unwrap();
    let error = book.mark_pending(&fourth, sent(14_000_000, 10_000_000), &caps(1.0, 1.0, 0.03), 1_000 + 7_200_000).unwrap_err();
    assert!(error.contains("maxTotalSol"), "{error}");
}

#[test]
fn a_failed_pending_write_sends_nothing_and_leaves_no_reservation() {
    let dir = StateDir::new();
    let book = TradeBook::open(Some(&dir.live()), Mode::Live).unwrap();
    let ticket = reserve(&book, MINT, "dev").unwrap();
    fs::create_dir(dir.0.join("live.json.tmp")).unwrap();
    let error = book.mark_pending(&ticket, sent(14_000_000, 10_000_000), &wide(), 1_000).unwrap_err();
    assert!(error.contains("state write failed"), "{error}");
    fs::remove_dir(dir.0.join("live.json.tmp")).unwrap();
    assert!(book.pending().is_empty());
    book.flush();
    assert_eq!(dir.read()["spend"]["totalLamports"], "0");
    assert!(reserve(&book, MINT, "dev").is_ok());
}

#[test]
fn max_snipes_counts_purchases_pending_buys_and_buys_in_progress_by_rule() {
    let book = TradeBook::open(None, Mode::Live).unwrap();
    bought(&book, MINT);
    let pending = reserve(&book, OTHER, "dev").unwrap();
    book.mark_pending(&pending, sent(14_000_000, 10_000_000), &wide(), 1_000).unwrap();
    let _in_progress = reserve(&book, "7nHgAcf7tM2MuUF29mismBW2gKHBrXkGhJtU8LpqB6vN", "dev").unwrap();
    let _other_rule = reserve(&book, "GQeXtxWoxbo1C8f4sUU14WMPbRNkgFgnCKvmfooptudo", "other").unwrap();
    assert_eq!(book.buys_by_rule("dev"), 3);
}

#[test]
fn one_sell_at_a_time_and_a_failed_sell_keeps_the_position_and_its_pending_exits() {
    let dir = StateDir::new();
    let book = TradeBook::open(Some(&dir.live()), Mode::Live).unwrap();
    bought(&book, MINT);
    assert_eq!(book.add_exit(MINT, PendingExit { code: ExitReason::PostDeleted, at: 5 }), ExitTarget::Position);
    assert_eq!(book.add_exit(OTHER, PendingExit { code: ExitReason::PostDeleted, at: 5 }), ExitTarget::None, "nothing held");
    assert_eq!(book.positions_to_exit().len(), 1);
    let sell = book.reserve_sell(MINT, 10).unwrap();
    assert_eq!(book.reserve_sell(MINT, 11), Err(ReserveError::Busy(Side::Sell)));
    assert!(book.positions_to_exit().is_empty(), "a sell is in progress");
    book.mark_pending(&sell, sent(105_000, 0), &wide(), 10).unwrap();
    book.resolve(&sell.id, &TradeOutcome::Failed { error: "slippage".to_string(), spent_lamports: Some(5_000) }, 20);
    let state = dir.read();
    assert_eq!(state["positions"][MINT]["pendingExits"], json!([{ "code": "post_deleted", "at": 5 }]));
    assert_eq!(state["spend"]["totalLamports"], "12105000", "a failed sell keeps its reservation");
    assert_eq!(book.positions_to_exit().len(), 1, "tried again");
    assert_eq!(book.reserve_sell(OTHER, 30), Err(ReserveError::NoPosition));
}

#[test]
fn a_sell_is_never_refused_by_the_spend_caps_and_an_expired_sell_is_refunded() {
    let dir = StateDir::new();
    let book = TradeBook::open(Some(&dir.live()), Mode::Live).unwrap();
    bought(&book, MINT);
    let sell = book.reserve_sell(MINT, 10).unwrap();
    book.mark_pending(&sell, sent(105_000, 0), &caps(0.0, 0.0, 0.0), 10).expect("an exit always goes out");
    assert_eq!(dir.read()["spend"]["totalLamports"], "12105000");
    book.resolve(&sell.id, &TradeOutcome::NotLanded { error: "expired".to_string() }, 20);
    assert_eq!(dir.read()["spend"]["totalLamports"], "12000000");
    let again = book.reserve_sell(MINT, 30).unwrap();
    book.mark_pending(&again, sent(105_000, 0), &wide(), 30).unwrap();
    book.resolve(&again.id, &landed(None), 40);
    let state = dir.read();
    assert_eq!(state["positions"], json!({}));
    assert_eq!(state["spend"]["totalLamports"], "12105000", "a landed sell keeps its reservation");
}

#[test]
fn a_delete_during_a_pending_buy_waits_on_it_and_the_position_takes_it_over() {
    let dir = StateDir::new();
    let book = TradeBook::open(Some(&dir.live()), Mode::Live).unwrap();
    let ticket = reserve(&book, MINT, "dev").unwrap();
    book.mark_pending(&ticket, sent(14_000_000, 10_000_000), &wide(), 1_000).unwrap();
    assert_eq!(book.exit_targets_for_post("twitter:1"), vec![MINT.to_string()], "the pending buy was bought on the post");
    assert_eq!(book.add_exit(MINT, PendingExit { code: ExitReason::PostDeleted, at: 1_500 }), ExitTarget::PendingBuy);
    assert_eq!(dir.read()["pending"][&ticket.id]["pendingExits"], json!([{ "code": "post_deleted", "at": 1_500 }]));
    book.resolve(&ticket.id, &landed(None), 2_000);
    assert_eq!(dir.read()["positions"][MINT]["pendingExits"], json!([{ "code": "post_deleted", "at": 1_500 }]));
    assert_eq!(book.positions_to_exit().len(), 1);
}

#[test]
fn a_restart_keeps_pending_trades_for_the_reconciler_and_drops_unsent_reservations() {
    let dir = StateDir::new();
    let book = TradeBook::open(Some(&dir.live()), Mode::Live).unwrap();
    let pending = reserve(&book, MINT, "dev").unwrap();
    book.mark_pending(&pending, sent(14_000_000, 10_000_000), &wide(), 1_000).unwrap();
    reserve(&book, OTHER, "dev").unwrap();
    book.close();
    drop(book);
    let book = TradeBook::open(Some(&dir.live()), Mode::Live).unwrap();
    let list = book.pending();
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].id.as_str(), list[0].mint.as_str(), list[0].amount_lamports), (pending.id.as_str(), MINT, 10_000_000));
    assert!(reserve(&book, OTHER, "dev").is_ok(), "an unsent reservation was never on disk");
}

#[test]
fn settling_a_buy_updates_the_total_even_after_its_hourly_entry_was_pruned() {
    let dir = StateDir::new();
    let book = TradeBook::open(Some(&dir.live()), Mode::Live).unwrap();
    let old = reserve(&book, MINT, "dev").unwrap();
    book.mark_pending(&old, sent(14_000_000, 10_000_000), &wide(), 1_000).unwrap();
    let later = reserve(&book, OTHER, "dev").unwrap();
    book.mark_pending(&later, sent(14_000_000, 10_000_000), &wide(), 1_000 + 3_600_001).unwrap();
    assert_eq!(dir.read()["spend"]["recent"].as_array().unwrap().len(), 1, "the first entry left the hourly window");
    book.resolve(&old.id, &TradeOutcome::NotLanded { error: "expired".to_string() }, 1_000 + 3_600_002);
    assert_eq!(dir.read()["spend"]["totalLamports"], "14000000", "the expired buy's reservation came back");
}
