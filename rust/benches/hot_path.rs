//! `cargo bench`: times the hot path on this machine with no network, over the same events as
//! `ts/scripts/bench.ts`. Feeds N posts, each with a new mint, through the real decider and the
//! trader in dry run, and prints the stage timings. The mint read waits 1 ms to stand in for a
//! nearby RPC; address derivation runs during that wait.
//!
//! The trader logs every buy, so the measuring run is a child process whose log lines are dropped;
//! only its result lines are printed.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_sniper_bot::execute::cache::ChainCache;
use solana_sniper_bot::execute::trade::{Trader, TraderDeps};
use solana_sniper_bot::execute::trades::{Mode, TradeBook};
use solana_sniper_bot::pipeline::Decider;
use solana_sniper_bot::protocol::Envelope;
use solana_sniper_bot::report::latency::{LatencyReport, Trace, percentile};
use solana_sniper_bot::rules::modes::Fire;
use solana_sniper_bot::scenario::{ChainSpec, CurveSpec, FeeTierSpec, MintSpec, ScenarioNetwork};
use solana_sniper_bot::setup::config::parse_config;

const TWITTER_EPOCH_MS: u64 = 1_288_834_974_657;
const RESULT: &str = "RESULT ";
const CHILD: &str = "SNIPER_BENCH_CHILD";

fn main() {
    let n: usize = std::env::args().skip(1).find_map(|a| a.parse().ok()).unwrap_or(2000);
    if std::env::var_os(CHILD).is_none() {
        let mut child =
            Command::new(std::env::current_exe().unwrap()).arg(n.to_string()).env(CHILD, "1").stdout(Stdio::piped()).spawn().unwrap();
        for line in BufReader::new(child.stdout.take().unwrap()).lines() {
            let line = line.unwrap();
            if let Some(result) = line.strip_prefix(RESULT) {
                println!("{result}");
            }
        }
        child.wait().unwrap();
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    runtime.block_on(run(n));
}

async fn run(n: usize) {
    let author = json!({ "id": "1000000000000000001", "handle": "@bench_dev" });
    let mints: Vec<String> = (0..n).map(|_| Keypair::new().pubkey().to_string()).collect();
    let curve = CurveSpec {
        virtual_token_reserves: "1000000000000000".to_string(),
        virtual_quote_reserves: "32000000000".to_string(),
        real_token_reserves: "720000000000000".to_string(),
        real_quote_reserves: "2000000000".to_string(),
        token_total_supply: "1000000000000000".to_string(),
        complete: false,
    };
    let mut specs = HashMap::new();
    for mint in &mints {
        let spec = MintSpec {
            token_program: "token-2022".to_string(),
            mint_authority: false,
            freeze_authority: false,
            supply: "1000000000000000".to_string(),
            decimals: 6,
            curve: Some(curve.clone()),
        };
        specs.insert(mint.clone(), spec);
    }
    let tiers = vec![FeeTierSpec { market_cap_lamports_threshold: "0".to_string(), protocol_fee_bps: 95, creator_fee_bps: 30 }];
    let chain = ChainSpec {
        sol_usd: 150.0,
        priority_fee_micro_lamports: 40_000,
        tip_floor_sol: 0.00002,
        block_height: 300_000_000,
        epoch: 850,
        fee_tiers: tiers,
        mints: specs,
    };
    let network = Arc::new(ScenarioNetwork::new(chain).with_read_delay(Duration::from_millis(1)));
    let config = parse_config(json!({
        "rules": [{ "name": "bench", "mode": "ca_scanner", "accounts": ["1000000000000000001"], "token": { "amountSol": 0.001 }, "filters": { "maxSnipes": 0 } }],
        "spend": { "maxPerTradeSol": 1, "maxPerHourSol": 1e6, "maxTotalSol": 1e6 }
    }))
    .unwrap();
    let config = Arc::new(config);
    let book = Arc::new(TradeBook::open(None, Mode::DryRun).unwrap());
    let cache = Arc::new(ChainCache::default());
    let tasks = cache.start(Arc::clone(&network), Duration::from_secs(60));
    cache.ready().await;
    while cache.fee_tiers().is_none() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let report = Arc::new(LatencyReport::default());
    let decider = Arc::new(Mutex::new(Decider::new(Arc::clone(&config), Arc::clone(&book))));
    let release = Arc::clone(&decider);
    let trader = Trader::new(TraderDeps {
        config: Arc::clone(&config),
        network,
        book,
        cache,
        report: Arc::clone(&report),
        wallet: Keypair::new(),
        dry_run: true,
        lander: None,
        senders: Vec::new(),
        release_fire: Arc::new(move |fire: &Fire| release.lock().unwrap().release(fire)),
    });

    let mut decide_us = Vec::new();
    for (i, mint) in mints.iter().enumerate() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let tweet_id = ((now - TWITTER_EPOCH_MS) << 22) + i as u64;
        let d =
            json!({ "tweetId": tweet_id.to_string(), "text": format!("CA: {mint}"), "createdAt": now, "kind": "post", "author": author });
        let frame = json!({ "v": 1, "t": "tweet", "op": "content", "ts": now, "d": d }).to_string();
        let received = Instant::now();
        let envelope: Envelope = serde_json::from_str(&frame).unwrap();
        let outcome = decider.lock().unwrap().handle(&envelope, now as i64);
        let decided = Instant::now();
        decide_us.push(decided.duration_since(received).as_secs_f64() * 1e6);
        for fire in outcome.fires {
            let mut trace = Trace::new(mint, "bench", "bench", &outcome.event_ref, None, now as i64, received);
            trace.decided = Some(decided);
            if let Some(reserved) = trader.reserve_buy(&fire, &trace) {
                trader.buy(fire, reserved, trace).await;
            }
        }
    }
    for task in tasks {
        task.abort();
    }
    decide_us.sort_by(f64::total_cmp);
    let p = |q| percentile(&decide_us, q).unwrap_or(0.0);
    println!("{RESULT}{n} posts, each with a new mint, on {} {}, release build", std::env::consts::OS, std::env::consts::ARCH);
    println!("{RESULT}receipt->decision (JSON parse + dedupe + detect + rules): p50 {:.1} us, p95 {:.1} us", p(0.5), p(0.95));
    for line in report.summary() {
        if line.starts_with("safety read") || line.starts_with("build+sign") || line.starts_with("decision->signed") {
            println!("{RESULT}{line}");
        }
    }
}
