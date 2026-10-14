//! Config errors a user reads: each says what to change.

use serde_json::json;
use solana_sniper_bot::setup::config::parse_config;

#[test]
fn a_denylisted_mint_is_named_as_such_and_a_bad_one_as_undecodable() {
    let config = |mint: &str| json!({ "rules": [{ "name": "r", "mode": "new_tweet", "accounts": ["1"], "token": { "mint": mint } }] });
    let denied = parse_config(config("So11111111111111111111111111111111111111112")).unwrap_err();
    assert!(denied.message.contains("wrapped SOL mint") && !denied.message.contains("32-byte"), "{}", denied.message);
    assert_eq!((denied.rule.as_deref(), denied.field.as_deref()), (Some("r"), Some("token.mint")));
    let short = parse_config(config("4uQeVj5tqViQh7yWWGStvkEG1Zmhx6uasJtWCJziofL")).unwrap_err();
    assert!(short.message.contains("does not decode to a 32-byte Solana address"), "{}", short.message);
}
