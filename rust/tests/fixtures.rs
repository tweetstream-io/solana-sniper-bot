//! Runs every case in `shared/fixtures` through the decider and compares the signal and decision
//! JSON with the hand-written expectations. The TypeScript bot runs the same files.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use solana_sniper_bot::execute::trades::{Mode, TradeBook};
use solana_sniper_bot::pipeline::{Decider, decisions_json, signals_json};
use solana_sniper_bot::protocol::Envelope;
use solana_sniper_bot::setup::config::parse_config;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../shared/fixtures")
}

/// A dry-run state file holding the fixture's positions, opened the way the bot opens one.
fn book_with(positions: &[Value], dir: &std::path::Path) -> TradeBook {
    let mut held = serde_json::Map::new();
    for p in positions {
        let mut trigger = json!({ "eventRef": "" });
        if let Some(post) = p.get("postKey") {
            trigger["postKey"] = post.clone();
        }
        let position = json!({
            "rule": p["rule"], "trigger": trigger, "tokenProgram": "", "openedAt": 0,
            "costLamports": "0", "tokenAmount": null, "pendingExits": []
        });
        held.insert(p["mint"].as_str().expect("position mint").to_string(), position);
    }
    let state = json!({
        "version": 2, "mode": "dry_run", "purchases": {}, "spend": { "totalLamports": "0", "recent": [] },
        "positions": held, "pending": {}
    });
    let path = dir.join("dry-run.json");
    std::fs::write(&path, state.to_string()).expect("write the fixture state");
    TradeBook::open(Some(&path), Mode::DryRun).expect("fixture state loads")
}

fn run_fixture(fixture: &Value) -> (Vec<Value>, Vec<Value>) {
    let config =
        fixture.get("config").cloned().unwrap_or_else(|| json!({ "rules": [{ "name": "none", "mode": "ca_scanner", "accounts": ["1"] }] }));
    let config = Arc::new(parse_config(config).expect("fixture config is valid"));
    let dir = std::env::temp_dir().join(format!("sniper-fixture-{}-{}", std::process::id(), fixture["name"].as_str().unwrap_or("x")));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let positions = fixture["positions"].as_array().cloned().unwrap_or_default();
    let book = Arc::new(book_with(&positions, &dir));
    let mut decider = Decider::new(Arc::clone(&config), Arc::clone(&book));
    let (mut signals, mut decisions) = (Vec::new(), Vec::new());
    for event in fixture["events"].as_array().expect("events") {
        let frame = event["envelope"].to_string();
        let envelope: Envelope = serde_json::from_str(&frame).expect("fixture envelope parses");
        let outcome = decider.handle(&envelope, event["receivedAtMs"].as_i64().expect("receivedAtMs"));
        signals.extend(signals_json(&outcome));
        decisions.extend(decisions_json(&outcome, &config));
    }
    book.close();
    drop(book);
    if let Err(error) = std::fs::remove_dir_all(&dir) {
        eprintln!("cannot remove {}: {error}", dir.display());
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

#[test]
fn every_shared_ledger_fixture_loads_and_writes_back_unchanged() {
    let mut files: Vec<PathBuf> =
        std::fs::read_dir(fixture_dir().join("ledger")).expect("shared/fixtures/ledger exists").map(|e| e.unwrap().path()).collect();
    files.sort();
    assert!(!files.is_empty());
    let dir = std::env::temp_dir().join(format!("sniper-ledger-fixtures-{}", std::process::id()));
    let mut failures = Vec::new();
    for file in &files {
        let original: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        let (name, mode) = match original["mode"].as_str() {
            Some("live") => ("live.json", Mode::Live),
            _ => ("dry-run.json", Mode::DryRun),
        };
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, original.to_string()).unwrap();
        match TradeBook::open(Some(&path), mode) {
            Ok(book) => {
                book.close();
                let written: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
                if written != original {
                    failures.push(format!("{}: wrote back\n  {written}\n  expected\n  {original}", file.display()));
                }
            }
            Err(error) => failures.push(format!("{}: {error}", file.display())),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The shared fixture cannot tell which update fired (both share an event name), so this checks
/// that the update without `kind` produces nothing on its own.
#[test]
fn a_post_assembled_from_updates_waits_for_its_kind() {
    let config = Arc::new(parse_config(json!({ "rules": [{ "name": "scan", "mode": "ca_scanner", "accounts": ["111"] }] })).unwrap());
    let mut decider = Decider::new(Arc::clone(&config), Arc::new(TradeBook::open(None, Mode::DryRun).unwrap()));
    let first = json!({ "v": 1, "t": "tweet", "op": "update", "ts": 1, "id": "u1", "d": {
        "tweetId": "2026541013136310272", "text": "GQeXtxWoxbo1C8f4sUU14WMPbRNkgFgnCKvmfooptudo", "author": { "id": "111", "handle": "dev_one" }
    } })
    .to_string();
    let outcome = decider.handle(&serde_json::from_str(&first).unwrap(), 1_772_000_003_500);
    assert!(outcome.signals.is_empty() && outcome.fires.is_empty(), "no kind yet: nothing, got {outcome:?}");
    let second =
        json!({ "v": 1, "t": "tweet", "op": "update", "ts": 2, "id": "u2", "d": { "tweetId": "2026541013136310272", "kind": "post" } })
            .to_string();
    let outcome = decider.handle(&serde_json::from_str(&second).unwrap(), 1_772_000_003_600);
    assert_eq!(outcome.fires.len(), 1, "the kind arrived: it fires");
}
