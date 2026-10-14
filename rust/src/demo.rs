//! `cargo run --release -- --demo`: the bot in dry run against the local demo feed
//! (`ts/scripts/demo-feed.ts`, started here with Node), with chain and market reads answered from
//! `shared/demo/scenario.json`. Needs no API key and makes no request off this machine.

use std::process::Stdio;
use std::sync::Arc;

use serde::Deserialize;
use solana_keypair::Keypair;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::execute::trades::{Mode, TradeBook};
use crate::log::log;
use crate::pipeline::{BotOptions, start_bot};
use crate::scenario::{ChainSpec, ScenarioNetwork};
use crate::setup::config::load_config;
use crate::setup::env::{repo_root, trial_link};

#[derive(Deserialize)]
struct Keys {
    standard: String,
    ultra: String,
}

#[derive(Deserialize)]
struct Scenario {
    keys: Keys,
    chain: ChainSpec,
}

/// The feed exits 1.5 s after its last step; the bot stops before that, so the feed closing is not
/// reported as a dropped connection.
const LAST_STEP_SETTLE: std::time::Duration = std::time::Duration::from_millis(1_000);

/// Returns true for the feed's line announcing its last step, `step N/N: ...`.
fn is_last_step(line: &str) -> bool {
    let Some(rest) = line.split("step ").nth(1) else {
        return false;
    };
    let counts = rest.split(':').next().unwrap_or("");
    matches!(counts.split_once('/'), Some((n, total)) if !n.is_empty() && n == total)
}

/// Runs the demo and returns the process exit code.
pub async fn run() -> i32 {
    match demo().await {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

async fn demo() -> Result<(), String> {
    let root = repo_root();
    let scenario_path = root.join("shared/demo/scenario.json");
    let text = std::fs::read_to_string(&scenario_path).map_err(|e| format!("{}: {e}", scenario_path.display()))?;
    let scenario: Scenario = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", scenario_path.display()))?;
    let config = load_config(&root.join("shared/demo/config.json")).map_err(|e| e.message)?;
    if !root.join("ts/node_modules/ws").exists() {
        return Err("The demo feed runs on Node.js 22.18 or later. Run npm install in ts/ once, then run the demo again.".to_string());
    }
    let mut feed = Command::new("node")
        .arg(root.join("ts/scripts/demo-feed.ts"))
        .args(["--port", "0", "--clients", "2", "--speed", "1", "--scenario"])
        .arg(&scenario_path)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Cannot start node for the demo feed ({e}). Install Node.js 22.18 or later."))?;
    let stdout = feed.stdout.take().ok_or("the demo feed has no stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    let mut port = None;
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(rest) = line.split("ws://127.0.0.1:").nth(1) {
            port = rest.split('/').next().map(str::to_string);
            break;
        }
    }
    let port = port.ok_or("the demo feed exited before it printed its port")?;
    log("demo", &format!("dry run against the local demo feed on port {port}; nothing leaves this machine"));
    let (last_step, mut script_done) = tokio::sync::oneshot::channel();
    let forward = tokio::spawn(async move {
        let mut last_step = Some(last_step);
        while let Ok(Some(line)) = lines.next_line().await {
            println!("{line}");
            if is_last_step(&line)
                && let Some(done) = last_step.take()
            {
                let _ = done.send(());
            }
        }
    });
    let (fatal, mut fatal_rx) = mpsc::unbounded_channel();
    let book = TradeBook::open(None, Mode::DryRun).map_err(|e| e.0)?;
    let bot = start_bot(BotOptions {
        config: Arc::new(config),
        network: Arc::new(ScenarioNetwork::new(scenario.chain)),
        book: Arc::new(book),
        feeds: vec![("ultra".to_string(), scenario.keys.ultra), ("standard".to_string(), scenario.keys.standard)],
        ws_url: format!("ws://127.0.0.1:{port}/ws"),
        dry_run: true,
        wallet: Keypair::new(),
        senders: Vec::new(),
        rpc_ws_url: String::new(),
        fatal,
    })
    .await;
    tokio::select! {
        _ = &mut script_done => tokio::time::sleep(LAST_STEP_SETTLE).await,
        status = feed.wait() => {
            status.map_err(|e| format!("demo feed: {e}"))?;
        }
        Some(reason) = fatal_rx.recv() => {
            bot.stop();
            return Err(reason);
        }
    }
    bot.stop();
    let _ = feed.wait().await;
    let _ = forward.await;
    for line in bot.report.summary() {
        log("summary", &line);
    }
    log("next", &format!("Run it on live X data with a TweetStream key: {}", trial_link("rust-cli-demo")));
    Ok(())
}
