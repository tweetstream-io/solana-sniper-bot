//! Entry point for `cargo run --release`: reads `.env` and `config.json`, prints setup help when
//! something is missing, then starts the pipeline in `pipeline.rs`. Dry run unless `DRY_RUN=false`.
//! `--demo` runs the local demo instead.

use std::process::ExitCode;
use std::sync::Arc;

use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_sniper_bot::demo;
use solana_sniper_bot::execute::trades::{Mode, TradeBook};
use solana_sniper_bot::land::land::{SenderKind, SenderTarget};
use solana_sniper_bot::log::log;
use solana_sniper_bot::network::{LiveNetwork, http_client};
use solana_sniper_bot::pipeline::{BotOptions, start_bot};
use solana_sniper_bot::setup::config::{handle_warnings, load_config};
use solana_sniper_bot::setup::env::{Env, read_env, setup_help, ultra_trial_link};
use tokio::sync::mpsc;

const USAGE: &str = "usage: solana-sniper-bot [--demo]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let demo_mode = match args.as_slice() {
        [] => false,
        [flag] if flag == "--demo" => true,
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    // Multi-thread: the two feed sockets, the refresh loops and in-flight trades never queue behind each other; the decider runs inline on the task that read the frame.
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime starts");
    let code = if demo_mode { runtime.block_on(demo::run()) } else { runtime.block_on(run()) };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn wallet(env: &Env) -> Result<Keypair, String> {
    let Some(secret) = &env.wallet_secret else {
        if !env.dry_run {
            return Err(
                "DRY_RUN=false needs WALLET_SECRET_KEY in .env: the base58 secret key of a wallet that holds only what you can lose."
                    .to_string(),
            );
        }
        return Ok(Keypair::new());
    };
    let bytes = bs58::decode(secret)
        .into_vec()
        .map_err(|_| "WALLET_SECRET_KEY is not base58. Paste the 64-byte secret key in base58, as wallets export it.".to_string())?;
    if bytes.len() != 64 {
        return Err(format!("WALLET_SECRET_KEY decodes to {} bytes; a Solana secret key is 64.", bytes.len()));
    }
    Keypair::try_from(bytes.as_slice()).map_err(|_| "WALLET_SECRET_KEY is not a valid Ed25519 key pair.".to_string())
}

/// Every RPC URL, plus Jito when `JITO_URL` is set.
fn sender_targets(env: &Env) -> Vec<SenderTarget> {
    let mut targets = Vec::new();
    for url in &env.rpc_urls {
        let name = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| url.clone());
        targets.push(SenderTarget { name, url: url.clone(), kind: SenderKind::Rpc });
    }
    if let Some(jito) = &env.jito_url {
        targets.push(SenderTarget { name: "jito".to_string(), url: format!("{jito}/api/v1/transactions"), kind: SenderKind::Jito });
    }
    targets
}

async fn run() -> i32 {
    let env = match read_env() {
        Ok(env) => env,
        Err(error) => return fail(&error.0),
    };
    if env.api_key.is_none() && env.ultra_api_key.is_none() {
        return fail(&setup_help());
    }
    let config = match load_config(&env.config_path) {
        Ok(config) => config,
        Err(error) => return fail(&error.message),
    };
    for warning in handle_warnings(&config) {
        log("warning", &warning);
    }
    let wallet = match wallet(&env) {
        Ok(wallet) => wallet,
        Err(message) => return fail(&message),
    };
    let senders = sender_targets(&env);
    if env.dry_run {
        log("mode", "DRY RUN: nothing is sent. Set DRY_RUN=false in .env to trade.");
    } else {
        let spend = &config.spend;
        println!(
            "
##########################################################################
  LIVE MODE: this bot will spend real SOL from {}
  Caps: {} SOL per trade, {} per hour, {} in total.
  Stop with Ctrl+C. Tokens bought this way can go to zero.
##########################################################################
",
            wallet.pubkey(),
            spend.max_per_trade_sol,
            spend.max_per_hour_sol,
            spend.max_total_sol
        );
    }
    let (file, mode) = if env.dry_run { ("dry-run.json", Mode::DryRun) } else { ("live.json", Mode::Live) };
    let book = match TradeBook::open(Some(&env.state_dir.join(file)), mode) {
        Ok(book) => book,
        Err(error) => return fail(&error.0),
    };
    let mut feeds = Vec::new();
    if let Some(key) = &env.ultra_api_key {
        feeds.push(("ultra".to_string(), key.clone()));
    }
    if let Some(key) = &env.api_key {
        feeds.push(("standard".to_string(), key.clone()));
    }
    let network = LiveNetwork::new(http_client(), env.rpc_urls[0].clone(), env.jupiter_api_key.clone());
    let (fatal, mut fatal_rx) = mpsc::unbounded_channel();
    let bot = start_bot(BotOptions {
        config: Arc::new(config),
        network: Arc::new(network),
        book: Arc::new(book),
        feeds,
        ws_url: env.ws_url.clone(),
        dry_run: env.dry_run,
        wallet,
        senders,
        rpc_ws_url: env.rpc_ws_url.clone(),
        fatal,
    })
    .await;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        Some(reason) = fatal_rx.recv() => {
            bot.stop();
            return fail(&reason);
        }
    }
    bot.stop();
    for line in bot.report.summary() {
        log("summary", &line);
    }
    if env.ultra_api_key.is_none() {
        log("next", &format!("Ultra Speed gives your snipe accounts first-send priority: {}", ultra_trial_link("rust-cli-latency-report")));
    }
    0
}

fn fail(message: &str) -> i32 {
    eprintln!("{message}");
    1
}
