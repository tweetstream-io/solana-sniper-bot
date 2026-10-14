//! The whole path of one event, in order: feed, dedupe, store and detect, rule match, filters,
//! build and sign, land, report. [`Decider`] is the synchronous part (no I/O) that the shared
//! fixtures test; [`start_bot`] wires it to the feeds and the executor.
//!
//! Reading order: `setup/env.rs`, `setup/config.rs`, `feed/client.rs`, `feed/dedupe.rs`, this file,
//! `detect/store.rs`, `detect/post.rs` and `detect/account.rs`, `rules/index.rs`, `rules/modes.rs`,
//! `rules/engine.rs`, `execute/trade.rs` (with `filters/gate.rs`, `execute/pump.rs`, and
//! `execute/trades.rs`), `land/land.rs`, `exits/exits.rs`, `report/latency.rs`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use solana_keypair::Keypair;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::detect::account::{callout_activity, follow_activity, fomo_activity, pin_activity, profile_activity};
use crate::detect::post::{SignalContext, detect_meta_signals, detect_post_signals, effective_platform, post_activity, x_actor};
use crate::detect::store::{Post, PostStore};
use crate::detect::types::{Activity, ActivityKind, CalloutOp, FollowOp, Signal};
use crate::execute::cache::ChainCache;
use crate::execute::trade::{ReleaseFire, Trader, TraderDeps};
use crate::execute::trades::{ExitReason, PendingExit, TradeBook};
use crate::exits::exits::ExitWatcher;
use crate::feed::client::{FeedOptions, Receipt, spawn_feed};
use crate::feed::dedupe::{EventDedupe, EventIds, dedupe_key};
use crate::land::land::{Lander, LanderOptions, SenderKind, SenderTarget, SignatureWatcher};
use crate::log::{log, short, wire_name};
use crate::network::Network;
use crate::network::http_client;
use crate::protocol::{Envelope, Payload, TweetPost, decode, event_ref, post_key, published_at_ms};
use crate::report::latency::{LatencyReport, Trace};
use crate::rules::engine::{EvalContext, RuleEngine};
use crate::rules::modes::{Fire, Skip};
use crate::setup::config::Config;

/// An exit the event asks for.
#[derive(Debug, Clone)]
pub struct PlannedExit {
    /// The mint to sell.
    pub mint: String,
    /// Why.
    pub code: ExitReason,
}

/// What the decider made of one event.
#[derive(Debug, Default)]
pub struct Outcome {
    /// A second copy of an event already handled.
    pub duplicate: bool,
    /// The payload did not match the protocol for its operation; nothing else is set.
    pub malformed: Option<String>,
    /// `"<t>/<op> <key>"`.
    pub event_ref: String,
    /// Addresses found.
    pub signals: Vec<Signal>,
    /// Rules that fire.
    pub fires: Vec<Fire>,
    /// Rules that declined.
    pub skips: Vec<Skip>,
    /// Exits to run.
    pub exits: Vec<PlannedExit>,
    /// Publish time of a post, for publish-to-receipt.
    pub published_at: Option<i64>,
    /// The first `tweet/content` for this post.
    pub first_content: bool,
}

/// Returns the stable signal JSON compared by `shared/fixtures` (documented in `shared/README.md`).
pub fn signals_json(outcome: &Outcome) -> Vec<Value> {
    let mut out = Vec::new();
    for s in &outcome.signals {
        out.push(json!({
            "event": s.event_ref,
            "source": s.source,
            "chain": s.chain,
            "mint": s.mint,
            "confidence": s.confidence,
            "actor": s.actor,
        }));
    }
    out
}

/// Returns the stable decision JSON compared by `shared/fixtures`: fires, then skips, then exits.
pub fn decisions_json(outcome: &Outcome, config: &Config) -> Vec<Value> {
    let event = &outcome.event_ref;
    let mut out = Vec::new();
    for fire in &outcome.fires {
        let rule = &config.rules[fire.rule];
        let source = fire.signal.as_ref().map(|s| s.source);
        out.push(json!({ "event": event, "rule": rule.name, "decision": "fire", "mint": fire.mint, "source": source }));
    }
    for skip in &outcome.skips {
        out.push(json!({ "event": event, "rule": config.rules[skip.rule].name, "decision": "skip", "code": skip.code }));
    }
    for exit in &outcome.exits {
        out.push(json!({ "event": event, "decision": "exit", "mint": exit.mint, "code": exit.code }));
    }
    out
}

/// Synchronous: from receipt to "these rules fire" with no await, so nothing else can interleave
/// and the decision costs microseconds.
pub struct Decider {
    config: Arc<Config>,
    book: Arc<TradeBook>,
    dedupe: EventDedupe,
    store: PostStore,
    engine: RuleEngine,
}

impl Decider {
    /// Creates a decider over the config and the shared trade book.
    pub fn new(config: Arc<Config>, book: Arc<TradeBook>) -> Decider {
        let engine = RuleEngine::new(Arc::clone(&config), &book);
        Decider { config, book, dedupe: EventDedupe::default(), store: PostStore::default(), engine }
    }

    /// Routes one event and returns the signals and decisions it produced. `wall_ms` is the
    /// receipt time used for post age and cooldowns.
    pub fn handle(&mut self, envelope: &Envelope, wall_ms: i64) -> Outcome {
        let pair = format!("{}/{}", envelope.t, envelope.op);
        let payload = match decode(envelope) {
            None => return Outcome { event_ref: pair, ..Outcome::default() },
            Some(Err(error)) => return Outcome { event_ref: pair, malformed: Some(error), ..Outcome::default() },
            Some(Ok(payload)) => payload,
        };
        let envelope_id = envelope.id.as_deref();
        let keys = event_ref(&pair, envelope_id, &payload);
        let ids = EventIds {
            envelope: envelope_id,
            event: keys.event_id.as_deref(),
            post: keys.post_key.as_deref(),
            payload: keys.payload_id.as_deref(),
        };
        let first = self.dedupe.first(dedupe_key(&pair, ids));
        let mut outcome = Outcome { event_ref: keys.event_ref, ..Outcome::default() };
        if !first {
            outcome.duplicate = true;
            return outcome;
        }
        let activity = self.apply(payload, &mut outcome);
        let Some(activity) = activity else {
            return outcome;
        };
        let age_ms = outcome.published_at.map(|published| wall_ms - published);
        let known_bio = activity
            .actor
            .namespace
            .profile_platform()
            .and_then(|platform| self.store.profile(platform, activity.actor.id.as_deref()))
            .and_then(|p| p.bio.text());
        let evaluation = self.engine.evaluate(&activity, EvalContext { now_ms: wall_ms, age_ms, known_bio });
        outcome.signals = activity.signals;
        outcome.fires = evaluation.fires;
        outcome.skips = evaluation.skips;
        outcome
    }

    /// Gives a fire back when it sent nothing: the same key may fire again.
    pub fn release(&mut self, fire: &Fire) {
        self.engine.release(fire);
    }

    /// Updates the store and returns what the account did, or `None` when the event carries no
    /// activity (a delete, or a post update before its author is known).
    fn apply(&mut self, payload: Payload, outcome: &mut Outcome) -> Option<Activity> {
        let event_ref = outcome.event_ref.clone();
        match payload {
            Payload::Content(d) => self.apply_post(d, true, &event_ref, outcome),
            Payload::Update(d) => self.apply_post(d, false, &event_ref, outcome),
            Payload::Meta(d) => {
                let key = post_key(&d.tweet_id, d.platform.as_deref(), None);
                let platform = platform_of(&key);
                self.store.apply_meta(&key, d);
                let activity = self.post_activity(&key, &platform, &event_ref)?;
                outcome.published_at = self.published_at(&key);
                Some(activity)
            }
            Payload::Pin(d) => {
                let key = post_key(&d.tweet_id, d.platform.as_deref(), d.author.as_ref());
                let platform = d.platform.as_deref().unwrap_or("twitter");
                Some(pin_activity(&d, &key, platform, &event_ref))
            }
            Payload::Delete(d) => {
                if self.config.exits.sell_on_delete {
                    let key = post_key(&d.tweet_id, d.platform.as_deref(), d.author.as_ref());
                    for mint in self.book.exit_targets_for_post(&key) {
                        outcome.exits.push(PlannedExit { mint, code: ExitReason::PostDeleted });
                    }
                }
                None
            }
            Payload::Follow(d) => Some(follow_activity(&d, FollowOp::Follow, "twitter", &event_ref)),
            Payload::Unfollow(d) => Some(follow_activity(&d, FollowOp::Unfollow, "twitter", &event_ref)),
            Payload::Profile(d) => {
                let platform = effective_platform(Some(&d.actor), "twitter");
                let activity = profile_activity(&d, self.store.profile(platform, d.actor.id.as_deref()), platform, &event_ref);
                self.store.update_profile(platform, d.actor.id.as_deref(), &d.changes);
                Some(activity)
            }
            Payload::Callout(d) => Some(callout_activity(&d, CalloutOp::Call, &event_ref)),
            Payload::CalloutUpdate(d) => Some(callout_activity(&d, CalloutOp::Update, &event_ref)),
            Payload::Thesis(d) => Some(fomo_activity(&d.coin, Some(&d), &event_ref)),
            Payload::Verified(d) => Some(fomo_activity(&d.coin, None, &event_ref)),
        }
    }

    /// `tweet/content` replaces the post's fields under any earlier update; `tweet/update` patches
    /// the fields it carries.
    fn apply_post(&mut self, d: TweetPost, content: bool, event_ref: &str, outcome: &mut Outcome) -> Option<Activity> {
        let key = post_key(&d.tweet_id, d.platform.as_deref(), d.author.as_ref());
        let platform = effective_platform(d.author.as_ref(), &platform_of(&key)).to_string();
        let had_content = self.store.get(&key).and_then(|e| e.post.as_ref()).is_some_and(|p| p.text.is_some());
        if content {
            self.store.apply_content(&key, &platform, d);
        } else {
            self.store.apply_update(&key, d);
        }
        let activity = self.post_activity(&key, &platform, event_ref)?;
        outcome.published_at = self.published_at(&key);
        outcome.first_content = content && !had_content;
        Some(activity)
    }

    /// Returns the activity of a stored post, once it has an author, with every signal of the
    /// merged post and its meta snapshot. Also sets the publish time.
    fn post_activity(&self, key: &str, platform: &str, event_ref: &str) -> Option<Activity> {
        let entry = self.store.get(key)?;
        let post: &Post = entry.post.as_ref()?;
        if post.author.id.is_none() && post.author.handle.is_none() {
            return None;
        }
        let platform = effective_platform(Some(&post.author), platform);
        let actor = x_actor(Some(&post.author), platform);
        let ctx = SignalContext { actor: &actor, event_ref, post_key: Some(key), market_cap_usd: None };
        let mut signals = detect_post_signals(post, &ctx);
        if let Some(meta) = &entry.meta {
            for signal in detect_meta_signals(meta, &ctx) {
                if !signals.iter().any(|s| s.mint == signal.mint) {
                    signals.push(signal);
                }
            }
        }
        let activity = post_activity(post, key, platform)?;
        Some(Activity { actor, event_ref: event_ref.to_string(), signals, kind: ActivityKind::Post(activity) })
    }

    fn published_at(&self, key: &str) -> Option<i64> {
        let post = self.store.get(key)?.post.as_ref()?;
        published_at_ms(&platform_of(key), &post.tweet_id, post.created_at)
    }
}

fn platform_of(key: &str) -> String {
    key.split(':').next().unwrap_or("twitter").to_string()
}

/// What the running bot needs from its caller.
pub struct BotOptions<N: Network> {
    /// The config.
    pub config: Arc<Config>,
    /// Outside calls.
    pub network: Arc<N>,
    /// The trade state for this mode.
    pub book: Arc<TradeBook>,
    /// One feed per API key, Ultra first.
    pub feeds: Vec<(String, String)>,
    /// TweetStream WebSocket URL.
    pub ws_url: String,
    /// Send nothing.
    pub dry_run: bool,
    /// The wallet; a throwaway key in dry run, so build and sign are still measured.
    pub wallet: Keypair,
    /// Every configured sender. Unused in dry run, but they shape the tips each transaction carries.
    pub senders: Vec<SenderTarget>,
    /// Solana PubSub WebSocket for confirmations.
    pub rpc_ws_url: String,
    /// Fatal feed errors (a rejected key) go here.
    pub fatal: mpsc::UnboundedSender<String>,
}

/// A running bot: stop it, then read its report.
pub struct RunningBot {
    /// Stage timings.
    pub report: Arc<LatencyReport>,
    book: Arc<TradeBook>,
    tasks: Vec<JoinHandle<()>>,
}

impl RunningBot {
    /// Stops the feeds, the reconciler, the exit watcher, and the background loops, and writes
    /// the trade state.
    pub fn stop(&self) {
        for task in &self.tasks {
            task.abort();
        }
        self.book.close();
    }
}

/// Starts the background caches (and waits for their first values), the trader and its
/// reconciler, the exit watcher, and one feed per key.
pub async fn start_bot<N: Network>(options: BotOptions<N>) -> RunningBot {
    let BotOptions { config, network, book, feeds, ws_url, dry_run, wallet, senders, rpc_ws_url, fatal } = options;
    let report = Arc::new(LatencyReport::default());
    let cache = Arc::new(ChainCache::default());
    let mut tasks = cache.start(Arc::clone(&network), Duration::from_millis(config.execution.blockhash_refresh_ms));
    cache.ready().await;
    let lander = if dry_run {
        None
    } else {
        let (watcher, task) = SignatureWatcher::start(rpc_ws_url);
        tasks.push(task);
        let status_rpc_url = senders.iter().find(|s| s.kind == SenderKind::Rpc).map(|s| s.url.clone());
        let height = Arc::clone(&cache);
        let options = LanderOptions {
            rebroadcast: Duration::from_millis(config.execution.rebroadcast_ms),
            confirm_timeout: Duration::from_millis(config.execution.confirm_timeout_ms),
            current_block_height: Arc::new(move || height.block_height()),
            status_rpc_url,
        };
        Some(Lander::new(http_client(), watcher, options))
    };
    let decider = Arc::new(Mutex::new(Decider::new(Arc::clone(&config), Arc::clone(&book))));
    let release: ReleaseFire = {
        let decider = Arc::clone(&decider);
        Arc::new(move |fire: &Fire| decider.lock().unwrap_or_else(|p| p.into_inner()).release(fire))
    };
    let trader = Arc::new(Trader::new(TraderDeps {
        config: Arc::clone(&config),
        network: Arc::clone(&network),
        book: Arc::clone(&book),
        cache: Arc::clone(&cache),
        report: Arc::clone(&report),
        wallet,
        dry_run,
        lander,
        senders,
        release_fire: release,
    }));
    tasks.push(trader.start());
    tasks.push(Arc::new(ExitWatcher::new(Arc::clone(&config), Arc::clone(&book), network, cache, Arc::clone(&trader))).start());
    let on_event = {
        let report = Arc::clone(&report);
        let config = Arc::clone(&config);
        move |envelope: &Envelope, receipt: &Receipt| on_event(envelope, receipt, &decider, &trader, &report, &config)
    };
    let on_event = Arc::new(on_event);
    for (name, api_key) in feeds {
        let options = FeedOptions { name, api_key, url: ws_url.clone(), heartbeat: Duration::from_secs(30) };
        tasks.push(spawn_feed(options, on_event.clone(), fatal.clone()));
    }
    RunningBot { report, book, tasks }
}

fn on_event<N: Network>(
    envelope: &Envelope,
    receipt: &Receipt,
    decider: &Arc<Mutex<Decider>>,
    trader: &Arc<Trader<N>>,
    report: &LatencyReport,
    config: &Config,
) {
    if envelope.t == "control" {
        if envelope.op == "auth_pong" {
            log("auth", &format!("{} authenticated", receipt.connection));
        }
        return;
    }
    let outcome = decider.lock().unwrap_or_else(|p| p.into_inner()).handle(envelope, receipt.wall_ms);
    let decided = Instant::now();
    if let Some(error) = &outcome.malformed {
        log("skip", &format!("{}: {} payload does not match the protocol: {error}", receipt.connection, outcome.event_ref));
        return;
    }
    if outcome.duplicate {
        return;
    }
    for fire in &outcome.fires {
        let rule = &config.rules[fire.rule];
        let mut trace = Trace::new(
            &fire.mint,
            &rule.name,
            &receipt.connection,
            &outcome.event_ref,
            outcome.published_at,
            receipt.wall_ms,
            receipt.mono,
        );
        trace.decided = Some(decided);
        // Reserved here, before the next event is handled, so one mint is bought once.
        let Some(reserved) = trader.reserve_buy(fire, &trace) else { continue };
        let (trader, fire) = (Arc::clone(trader), fire.clone());
        drop(tokio::spawn(async move { trader.buy(fire, reserved, trace).await }));
    }
    for exit in &outcome.exits {
        let (trader, mint, exit) = (Arc::clone(trader), exit.mint.clone(), PendingExit { code: exit.code, at: receipt.wall_ms });
        drop(tokio::spawn(async move { trader.exit(&mint, exit).await }));
    }
    report.record_first_copy(&receipt.connection);
    if outcome.first_content
        && let Some(published) = outcome.published_at
    {
        report.record_post(&receipt.connection, (receipt.wall_ms - published) as f64);
    }
    for s in &outcome.signals {
        let who = s.actor.handle.as_ref().map_or_else(|| s.actor.namespace.as_str().to_string(), |h| format!("@{h}"));
        log("signal", &format!("{} {} from {who} ({})", wire_name(&s.source), short(&s.mint), s.event_ref));
    }
    for s in &outcome.skips {
        log("skip", &format!("{}: {}, {}", config.rules[s.rule].name, wire_name(&s.code), s.detail));
    }
    for f in &outcome.fires {
        let rule = &config.rules[f.rule];
        log("fire", &format!("{}: buy {}, {}", rule.name, short(&f.mint), f.detail));
    }
    for e in &outcome.exits {
        log("exit", &format!("{}: the post it was bought on was deleted", short(&e.mint)));
    }
}
