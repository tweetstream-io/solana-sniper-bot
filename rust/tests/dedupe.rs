//! Copies of one event from two connections share a dedupe key; distinct events do not.

use serde_json::json;
use serde_json::value::RawValue;
use solana_sniper_bot::feed::dedupe::dedupe_key;

fn key(pair: &str, d: serde_json::Value) -> u64 {
    let raw = RawValue::from_string(d.to_string()).unwrap();
    dedupe_key(pair, Some(&raw))
}

#[test]
fn receipt_times_at_any_depth_do_not_make_a_copy_a_new_event() {
    let pin = |received: i64, observed: i64| json!({ "eventId": "pin-1", "observedAt": observed, "tweet": { "tweetId": "1", "text": "hi", "receivedAt": received } });
    assert_eq!(key("tweet/pin", pin(10, 20)), key("tweet/pin", pin(11, 25)));
    let delete = |deleted: i64| json!({ "tweetId": "1", "deletedAt": deleted, "receivedAt": deleted + 1 });
    assert_eq!(key("tweet/delete", delete(5)), key("tweet/delete", delete(9)));
}

#[test]
fn key_order_does_not_matter_but_any_other_field_does() {
    let a: serde_json::Value = serde_json::from_str(r#"{"tweetId":"1","text":"a","kind":"post"}"#).unwrap();
    let b: serde_json::Value = serde_json::from_str(r#"{"kind":"post","text":"a","tweetId":"1"}"#).unwrap();
    assert_eq!(key("tweet/update", a.clone()), key("tweet/update", b));
    assert_ne!(key("tweet/update", a.clone()), key("tweet/update", json!({ "tweetId": "1", "text": "ab", "kind": "post" })));
    assert_ne!(key("tweet/update", a.clone()), key("tweet/meta", a));
    assert_ne!(key("tweet/update", json!({ "n": 1 })), key("tweet/update", json!({ "n": 2 })));
}
