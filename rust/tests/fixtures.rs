//! Runs every case in `shared/fixtures` through the decider and compares the signal and decision
//! JSON with the hand-written expectations. The TypeScript bot runs the same files.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use solana_sniper_bot::detect::types::Actor;
use solana_sniper_bot::execute::trades::{Mode, Position, PositionRoute, TradeBook, Trigger};
use solana_sniper_bot::pipeline::{Decider, decisions_json, signals_json};
use solana_sniper_bot::protocol::Envelope;
use solana_sniper_bot::setup::config::parse_config;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../shared/fixtures")
}

fn actor(value: &Value) -> Actor {
    serde_json::from_value(value.clone()).expect("position actor")
}

fn run_fixture(fixture: &Value) -> (Vec<Value>, Vec<Value>) {
    let config =
        fixture.get("config").cloned().unwrap_or_else(|| json!({ "rules": [{ "name": "none", "mode": "ca_scanner", "accounts": ["1"] }] }));
    let config = Arc::new(parse_config(config).expect("fixture config is valid"));
    let book = Arc::new(TradeBook::open(None, Mode::DryRun).unwrap());
    for p in fixture["positions"].as_array().into_iter().flatten() {
        let trigger = Trigger {
            rule: p["rule"].as_str().unwrap().to_string(),
            fire_key: String::new(),
            event_ref: String::new(),
            post_key: p["postKey"].as_str().map(str::to_string),
            actor: actor(&p["actor"]),
        };
        book.open_position(Position {
            mint: p["mint"].as_str().unwrap().to_string(),
            route: PositionRoute::Pump,
            token_program: String::new(),
            opened_at: 0,
            buy_trade_id: String::new(),
            cost_lamports: 0,
            token_amount: None,
            trigger,
            sell_trade_id: None,
        });
    }
    let mut decider = Decider::new(Arc::clone(&config), book);
    let (mut signals, mut decisions) = (Vec::new(), Vec::new());
    for event in fixture["events"].as_array().expect("events") {
        let frame = event["envelope"].to_string();
        let envelope: Envelope = serde_json::from_str(&frame).expect("fixture envelope parses");
        let outcome = decider.handle(&envelope, event["receivedAtMs"].as_i64().expect("receivedAtMs"));
        signals.extend(signals_json(&outcome));
        decisions.extend(decisions_json(&outcome, &config));
    }
    (signals, decisions)
}

#[test]
fn every_shared_fixture_matches() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(fixture_dir()).expect("shared/fixtures exists").map(|e| e.unwrap().path()).collect();
    files.retain(|p| p.extension().is_some_and(|e| e == "json"));
    files.sort();
    assert!(files.len() >= 15, "expected the shared fixtures, found {}", files.len());
    let mut failures = Vec::new();
    for file in &files {
        let fixture: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        let name = fixture["name"].as_str().unwrap_or("?").to_string();
        let (signals, decisions) = run_fixture(&fixture);
        if let Some(expected) = fixture.get("expectedSignals")
            && expected != &Value::Array(signals.clone())
        {
            failures.push(format!("{name} signals\n  expected {}\n  actual   {}", expected, Value::Array(signals)));
        }
        if let Some(expected) = fixture.get("expectedDecisions")
            && expected != &Value::Array(decisions.clone())
        {
            failures.push(format!("{name} decisions\n  expected {}\n  actual   {}", expected, Value::Array(decisions)));
        }
    }
    assert!(failures.is_empty(), "{} of {} fixtures differ:\n{}", failures.len(), files.len(), failures.join("\n"));
}

/// JSON equality with numbers compared by value, as JavaScript does: 20 and 20.0 are the same.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y)),
        (Value::Object(x), Value::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w))),
        _ => a == b,
    }
}

fn at<'a>(value: &'a Value, path: &str) -> &'a Value {
    let mut current = value;
    for key in path.split('.') {
        current = match key.parse::<usize>() {
            Ok(i) => &current[i],
            Err(_) => &current[key],
        };
    }
    current
}

#[test]
fn every_shared_config_fixture_matches() {
    let mut files: Vec<PathBuf> =
        std::fs::read_dir(fixture_dir().join("config")).expect("shared/fixtures/config exists").map(|e| e.unwrap().path()).collect();
    files.sort();
    assert!(!files.is_empty());
    let mut failures = Vec::new();
    for file in &files {
        let fixture: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        let name = fixture["name"].as_str().unwrap_or("?");
        let result = parse_config(fixture["config"].clone());
        let expected = &fixture["expected"];
        match (result, expected.get("values"), expected.get("error")) {
            (Ok(config), Some(values), _) => {
                let config = serde_json::to_value(&config).unwrap();
                for (path, value) in values.as_object().unwrap() {
                    if !same(at(&config, path), value) {
                        failures.push(format!("{name}: {path} is {}, expected {value}", at(&config, path)));
                    }
                }
            }
            (Err(error), _, Some(want)) => {
                let got = json!({ "rule": error.rule, "field": error.field });
                if &got != want {
                    failures.push(format!("{name}: error {got} ({}), expected {want}", error.message));
                }
            }
            (Ok(_), _, _) => failures.push(format!("{name}: expected an error")),
            (Err(error), _, _) => failures.push(format!("{name}: unexpected error {}", error.message)),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
