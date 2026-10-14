//! Copies of one event from two connections share a dedupe key; distinct events do not.

use serde_json::json;
use solana_sniper_bot::feed::dedupe::dedupe_key;
use solana_sniper_bot::protocol::Envelope;

fn key(pair: &str, d: &str) -> u64 {
    let (t, op) = pair.split_once('/').unwrap();
    let frame = format!(r#"{{"v":1,"t":"{t}","op":"{op}","d":{d}}}"#);
    let envelope: Envelope = serde_json::from_str(&frame).unwrap();
    dedupe_key(&format!("{}/{}", envelope.t, envelope.op), envelope.d)
}

#[test]
fn receipt_times_at_any_depth_do_not_make_a_copy_a_new_event() {
    let pin = |received: i64, observed: i64| {
        json!({ "eventId": "pin-1", "observedAt": observed, "tweet": { "tweetId": "1", "text": "hi", "receivedAt": received } }).to_string()
    };
    assert_eq!(key("tweet/pin", &pin(10, 20)), key("tweet/pin", &pin(11, 25)));
    let delete = |deleted: i64| json!({ "tweetId": "1", "deletedAt": deleted, "receivedAt": deleted + 1 }).to_string();
    assert_eq!(key("tweet/delete", &delete(5)), key("tweet/delete", &delete(9)));
}

#[test]
fn key_order_does_not_matter_but_any_other_field_does() {
    let a = r#"{"tweetId":"1","text":"a","kind":"post","author":{"id":"7","handle":"x"}}"#;
    let b = r#"{"kind":"post","author":{"handle":"x","id":"7"},"text":"a","tweetId":"1"}"#;
    assert_eq!(key("tweet/update", a), key("tweet/update", b));
    assert_ne!(
        key("tweet/update", a),
        key("tweet/update", r#"{"tweetId":"1","text":"ab","kind":"post","author":{"id":"7","handle":"x"}}"#)
    );
    assert_ne!(key("tweet/update", a), key("tweet/meta", a));
    assert_ne!(key("tweet/update", r#"{"n":1}"#), key("tweet/update", r#"{"n":2}"#));
}

#[test]
fn numbers_equal_once_parsed_as_doubles_are_one_event() {
    let price = |n: &str| key("tweet/meta", &format!(r#"{{"tweetId":"1","priceUsd":{n}}}"#));
    assert_eq!(price("1"), price("1.0"));
    assert_eq!(price("1"), price("1e0"));
    assert_eq!(price("0"), price("-0.0"));
    assert_eq!(price("9007199254740993"), price("9007199254740992"));
    assert_ne!(price("1"), price("1.5"));
}
