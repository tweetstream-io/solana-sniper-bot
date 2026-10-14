//! `maxSnipes` follows the trade book: a buy that expired or failed on chain is not a purchase, so
//! the rule gets its snipe back, while a fire whose buy is not reserved yet still counts.

use std::sync::Arc;

use serde_json::json;
use solana_sniper_bot::execute::trades::{BuyRequest, Mode, Sent, TradeBook, TradeOutcome, TradeRoute, Trigger};
use solana_sniper_bot::pipeline::{Decider, Outcome};
use solana_sniper_bot::protocol::Envelope;
use solana_sniper_bot::setup::config::{Spend, parse_config};

const FIRST: &str = "FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA";
const SECOND: &str = "H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29";

fn decider(book: &Arc<TradeBook>) -> Decider {
    let config = parse_config(json!({
        "rules": [{ "name": "dev", "mode": "ca_scanner", "accounts": ["111"], "filters": { "maxSnipes": 1 } }]
    }))
    .unwrap();
    Decider::new(Arc::new(config), Arc::clone(book))
}

fn post(decider: &mut Decider, tweet_id: u64, mint: &str) -> Outcome {
    let d = json!({ "tweetId": tweet_id.to_string(), "text": format!("CA: {mint}"), "kind": "post", "author": { "id": "111", "handle": "dev" } });
    let frame = json!({ "v": 1, "t": "tweet", "op": "content", "ts": 1, "d": d }).to_string();
    let envelope: Envelope = serde_json::from_str(&frame).unwrap();
    // Snowflake ids from 2026, received right away, so the posts are never stale.
    decider.handle(&envelope, ((tweet_id >> 22) + 1_288_834_974_657) as i64)
}

/// What the trader does with a fire: reserve the buy, write it as pending, then record `outcome`.
fn buy(book: &TradeBook, mint: &str, outcome: TradeOutcome) {
    let trigger = Trigger { event_ref: "e".to_string(), post_key: None };
    let ticket = book.reserve_buy(BuyRequest { mint, rule: "dev", trigger, now: 1_000 }).unwrap();
    let sent = Sent {
        route: TradeRoute::Pump,
        signature: Some("sig".to_string()),
        blockhash: "hash".to_string(),
        last_valid_block_height: None,
        token_program: String::new(),
        reserved_lamports: 14_000_000,
        amount_lamports: 10_000_000,
    };
    let caps = Spend { max_per_trade_sol: 1.0, max_per_hour_sol: 1.0, max_total_sol: 1.0 };
    book.mark_pending(&ticket, sent, &caps, 1_000).unwrap();
    book.resolve(&ticket.id, &outcome, 2_000);
}

#[test]
fn a_buy_that_expired_or_failed_gives_the_rule_its_snipe_back() {
    let outcomes = [
        TradeOutcome::NotLanded { error: "expired".to_string() },
        TradeOutcome::Failed { error: "6002".to_string(), spent_lamports: None },
    ];
    for outcome in outcomes {
        let book = Arc::new(TradeBook::open(None, Mode::DryRun).unwrap());
        let mut decider = decider(&book);
        assert_eq!(post(&mut decider, 2_026_541_002_650_550_272, FIRST).fires.len(), 1);
        buy(&book, FIRST, outcome.clone());
        assert_eq!(book.buys_by_rule("dev"), 0, "{outcome:?} is not a buy");
        let next = post(&mut decider, 2_026_541_006_844_854_272, SECOND);
        assert_eq!(next.fires.len(), 1, "after {outcome:?} the rule may buy again: {:?}", next.skips);
    }
}

#[test]
fn a_fire_counts_until_its_buy_is_reserved_and_a_landed_buy_keeps_counting() {
    let book = Arc::new(TradeBook::open(None, Mode::DryRun).unwrap());
    let mut decider = decider(&book);
    assert_eq!(post(&mut decider, 2_026_541_002_650_550_272, FIRST).fires.len(), 1);
    assert_eq!(post(&mut decider, 2_026_541_006_844_854_272, SECOND).fires.len(), 0, "the first fire holds the only snipe");
    buy(&book, FIRST, TradeOutcome::Landed { token_amount: None, spent_lamports: None });
    let third = post(&mut decider, 2_026_541_011_039_158_272, SECOND);
    assert!(third.fires.is_empty(), "the landed buy uses the snipe: {:?}", third.skips);
}

#[test]
fn a_released_fire_gives_its_claim_back() {
    let book = Arc::new(TradeBook::open(None, Mode::DryRun).unwrap());
    let mut decider = decider(&book);
    let first = post(&mut decider, 2_026_541_002_650_550_272, FIRST);
    decider.release(&first.fires[0]);
    let next = post(&mut decider, 2_026_541_006_844_854_272, SECOND);
    assert_eq!(next.fires.len(), 1, "the refused or filtered fire bought nothing: {:?}", next.skips);
}
