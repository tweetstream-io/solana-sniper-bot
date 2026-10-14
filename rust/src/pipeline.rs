//! The whole path of one event, in order: feed, dedupe, store and detect, rule match, filters,
//! build and sign, land, report. [`Decider`] is the synchronous part (no I/O) that the shared
//! fixtures test; [`start_bot`] wires it to the feeds and the executor.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use solana_keypair::Keypair;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::detect::account::{callout_activity, follow_activity, fomo_activity, pin_activity, profile_activity};
use crate::detect::post::{SignalContext, detect_meta_signals, detect_post_signals, post_activity, x_actor};
use crate::detect::store::{Post, PostStore};
use crate::detect::types::{Activity, ActivityKind, CalloutOp, FollowOp, Signal};
use crate::execute::cache::ChainCache;
use crate::execute::trade::{ReleaseFire, SellResult, Trader, TraderDeps};
use crate::execute::trades::TradeBook;
use crate::exits::exits::ExitWatcher;
use crate::feed::client::{FeedOptions, Receipt, spawn_feed};
use crate::feed::dedupe::{EventDedupe, EventIds, dedupe_key};
use crate::land::land::{Lander, LanderOptions, SenderKind, SenderTarget, SignatureWatcher};
use crate::log::{log, short};
use crate::network::Network;
use crate::network::http_client;
use crate::protocol::{
    AccountCallout, AccountFollow, AccountProfileUpdate, AccountThesis, AccountTokenVerified, Envelope, TweetDelete, TweetMeta, TweetPin,
    TweetPost, post_key, published_at_ms,
};
use crate::report::latency::{LatencyReport, Trace};
use crate::rules::engine::{EvalContext, RuleEngine};
use crate::rules::modes::{ExitCode, Fire, Skip};
use crate::setup::config::{Action, Config};

/// An exit the event asks for.
#[derive(Debug, Clone)]
pub struct PlannedExit {
    /// The mint to sell.
    pub mint: String,
    /// Why.
    pub code: ExitCode,
    /// Index of the rule that asked, or `None` for a deleted post.
    pub rule: Option<usize>,
    /// Free text for logs.
    pub detail: String,
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
        out.push(json!({ "event": event, "rule": rule.name, "decision": "fire", "mint": fire.mint, "action": rule.token.action.as_str(), "source": source }));
    }
    for skip in &outcome.skips {
        out.push(json!({ "event": event, "rule": config.rules[skip.rule].name, "decision": "skip", "code": skip.code }));
    }
    for exit in &outcome.exits {
        let mut value = json!({ "event": event, "decision": "exit", "mint": exit.mint, "code": exit.code });
        if let Some(rule) = exit.rule {
            value["rule"] = json!(config.rules[rule].name);
        }
        out.push(value);
    }
    out
}

fn parse<T: DeserializeOwned>(envelope: &Envelope) -> Result<T, String> {
    let text = envelope.d.map_or("{}", |d| d.get());
    serde_json::from_str(text).map_err(|e| e.to_string())
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

/// A routed payload, with the identities dedupe needs.
enum Payload {
    Post(TweetPost),
    Meta(TweetMeta),
    Pin(TweetPin),
    Delete(TweetDelete),
    Follow(AccountFollow, FollowOp),
    Profile(AccountProfileUpdate),
    Callout(AccountCallout, CalloutOp),
    Thesis(AccountThesis),
    Verified(AccountTokenVerified),
}

impl Decider {
    /// Creates a decider over the config and the shared trade book.
    pub fn new(config: Arc<Config>, book: Arc<TradeBook>) -> Decider {
        let engine = RuleEngine::new(Arc::clone(&config), Arc::clone(&book));
        Decider { config, book, dedupe: EventDedupe::default(), store: PostStore::default(), engine }
    }

    /// Routes one event and returns the signals and decisions it produced. `wall_ms` is the
    /// receipt time used for post age and cooldowns.
    pub fn handle(&mut self, envelope: &Envelope, wall_ms: i64) -> Outcome {
        let (t, op) = (envelope.t.as_ref(), envelope.op.as_ref());
        let routed = match (t, op) {
            ("tweet", "content") => parse(envelope).and_then(|d| content_problem(&d).map_or(Ok(Payload::Post(d)), Err)),
            ("tweet", "update") => parse(envelope).map(Payload::Post),
            ("tweet", "meta") => parse(envelope).map(Payload::Meta),
            ("tweet", "pin") => parse(envelope).map(Payload::Pin),
            ("tweet", "delete") => parse(envelope).map(Payload::Delete),
            ("account", "follow") => parse(envelope).map(|d| Payload::Follow(d, FollowOp::Follow)),
            ("account", "unfollow") => parse(envelope).map(|d| Payload::Follow(d, FollowOp::Unfollow)),
            ("account", "profile_update") => parse(envelope).map(Payload::Profile),
            ("account", "callout") => parse(envelope).map(|d| Payload::Callout(d, CalloutOp::Call)),
            ("account", "callout_update") => parse(envelope).map(|d| Payload::Callout(d, CalloutOp::Update)),
            ("account", "thesis") => parse(envelope).map(Payload::Thesis),
            ("account", "token_verified") => parse(envelope).map(Payload::Verified),
            _ => return Outcome { event_ref: format!("{t}/{op}"), ..Outcome::default() },
        };
        let payload = match routed.and_then(|payload| problem(&payload).map_or(Ok(payload), Err)) {
            Ok(payload) => payload,
            Err(error) => return Outcome { event_ref: format!("{t}/{op}"), malformed: Some(error), ..Outcome::default() },
        };
        let pair = format!("{t}/{op}");
        let envelope_id = envelope.id.as_deref();
        let (event_ref, ids) = identify(&pair, envelope_id, &payload);
        let mut outcome = Outcome { event_ref, ..Outcome::default() };
        if !self.dedupe.first(dedupe_key(&pair, ids.as_ids(envelope_id))) {
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
        for exit in evaluation.exits {
            if self.book.exit_targets_for_actor(&self.config.rules[exit.rule].name, &exit.actor).contains(&exit.mint) {
                outcome.exits.push(PlannedExit { mint: exit.mint, code: exit.code, rule: Some(exit.rule), detail: exit.detail });
            }
        }
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
            Payload::Post(d) => {
                let content = event_ref.starts_with("tweet/content ");
                let key = post_key(&d.tweet_id, d.platform.as_deref(), d.author.as_ref());
                let platform = platform_of(&key);
                let had_content = self.store.get(&key).and_then(|e| e.post.as_ref()).is_some_and(|p| p.text.is_some());
                if content {
                    self.store.apply_content(&key, &platform, d);
                } else {
                    self.store.apply_update(&key, d);
                }
                let activity = self.post_activity(&key, &platform, &event_ref)?;
                outcome.published_at = self.published_at(&key);
                outcome.first_content = content && !had_content;
                Some(activity)
            }
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
                Some(pin_activity(&d, &key, &platform_of(&key), &event_ref))
            }
            Payload::Delete(d) => {
                if self.config.exits.sell_on_delete {
                    let key = post_key(&d.tweet_id, d.platform.as_deref(), d.author.as_ref());
                    for mint in self.book.exit_targets_for_post(&key) {
                        let detail = "the post that triggered the buy was deleted".to_string();
                        outcome.exits.push(PlannedExit { mint, code: ExitCode::PostDeleted, rule: None, detail });
                    }
                }
                None
            }
            Payload::Follow(d, op) => Some(follow_activity(&d, op, account_platform(d.actor.platform.as_deref()), &event_ref)),
            Payload::Profile(d) => {
                let platform = account_platform(d.actor.platform.as_deref());
                let activity = profile_activity(&d, self.store.profile(platform, d.actor.id.as_deref()), platform, &event_ref);
                self.store.update_profile(platform, d.actor.id.as_deref(), &d.changes);
                Some(activity)
            }
            Payload::Callout(d, op) => Some(callout_activity(&d, op, &event_ref)),
            Payload::Thesis(d) => Some(fomo_activity(&d.coin, Some(&d), &event_ref)),
            Payload::Verified(d) => Some(fomo_activity(&d.coin, None, &event_ref)),
        }
    }

    /// Returns the activity of a stored post, once it has an author, with every signal of the
    /// merged post and its meta snapshot. Also sets the publish time.
    fn post_activity(&self, key: &str, platform: &str, event_ref: &str) -> Option<Activity> {
        let entry = self.store.get(key)?;
        let post: &Post = entry.post.as_ref()?;
        if post.author.id.is_none() && post.author.handle.is_none() {
            return None;
        }
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
        let activity = post_activity(post, key, platform);
        Some(Activity { actor, event_ref: event_ref.to_string(), signals, kind: ActivityKind::Post(activity) })
    }

    fn published_at(&self, key: &str) -> Option<i64> {
        let post = self.store.get(key)?.post.as_ref()?;
        published_at_ms(&platform_of(key), &post.tweet_id, post.created_at)
    }
}

/// Checks the fields the protocol marks required for each operation, so a malformed frame is
/// skipped with a reason instead of acting on half an event.
fn problem(payload: &Payload) -> Option<String> {
    let text = |value: Option<&String>| value.is_some_and(|v| !v.is_empty());
    let need = |ok: bool, what: &str| if ok { None } else { Some(format!("missing or invalid {what}")) };
    match payload {
        Payload::Post(d) => need(!d.tweet_id.is_empty(), "tweetId"),
        Payload::Meta(d) => need(!d.tweet_id.is_empty(), "tweetId"),
        Payload::Delete(d) => need(!d.tweet_id.is_empty(), "tweetId"),
        Payload::Pin(d) => need(!d.tweet_id.is_empty(), "tweetId")
            .or_else(|| need(d.author.is_some(), "author"))
            .or_else(|| need(text(d.event_id.as_ref()), "eventId")),
        Payload::Follow(d, _) => {
            need(text(d.target.handle.as_ref()), "target.handle").or_else(|| need(text(d.event_id.as_ref()), "eventId"))
        }
        Payload::Profile(d) => need(text(d.event_id.as_ref()), "eventId"),
        Payload::Callout(d, _) => need(!d.caller.username.is_empty(), "caller.username")
            .or_else(|| need(!d.token.address.is_empty(), "token.address"))
            .or_else(|| need(text(d.callout_id.as_ref()), "calloutId")),
        Payload::Thesis(d) => need(!d.coin.address.is_empty(), "coin.address")
            .or_else(|| need(!d.author.id.is_empty() && !d.author.username.is_empty(), "author"))
            .or_else(|| need(text(d.thesis.id.as_ref()), "thesis")),
        Payload::Verified(d) => need(!d.coin.address.is_empty(), "coin.address"),
    }
}

/// `tweet/content` must carry the whole post: text, author, and a known kind.
fn content_problem(d: &TweetPost) -> Option<String> {
    let need = |ok: bool, what: &str| if ok { None } else { Some(format!("missing or invalid {what}")) };
    need(!d.tweet_id.is_empty(), "tweetId")
        .or_else(|| need(d.text.is_some(), "text"))
        .or_else(|| need(d.author.is_some(), "author"))
        .or_else(|| need(d.kind.is_some_and(|k| k != crate::protocol::PostKind::Other), "kind"))
}

fn platform_of(key: &str) -> String {
    key.split(':').next().unwrap_or("twitter").to_string()
}

fn account_platform(platform: Option<&str>) -> &str {
    platform.unwrap_or("twitter")
}

/// Identities of a routed payload, kept as owned strings until dedupe borrows them.
struct Ids {
    event: Option<String>,
    post: Option<String>,
    payload: Option<String>,
}

impl Ids {
    fn as_ids<'a>(&'a self, envelope: Option<&'a str>) -> EventIds<'a> {
        EventIds { envelope, event: self.event.as_deref(), post: self.post.as_deref(), payload: self.payload.as_deref() }
    }
}

/// Returns the event reference and the identities of a payload. Posts are named by post key;
/// other events by `d.eventId`, else the envelope id.
fn identify(pair: &str, envelope_id: Option<&str>, payload: &Payload) -> (String, Ids) {
    let (event, post, own) = match payload {
        Payload::Post(d) => (None, Some(post_key(&d.tweet_id, d.platform.as_deref(), d.author.as_ref())), None),
        Payload::Meta(d) => (None, Some(post_key(&d.tweet_id, d.platform.as_deref(), None)), None),
        Payload::Pin(d) => (d.event_id.clone(), Some(post_key(&d.tweet_id, d.platform.as_deref(), d.author.as_ref())), None),
        Payload::Delete(d) => (d.event_id.clone(), Some(post_key(&d.tweet_id, d.platform.as_deref(), d.author.as_ref())), None),
        Payload::Follow(d, _) => (d.event_id.clone(), None, None),
        Payload::Profile(d) => (d.event_id.clone(), None, None),
        Payload::Callout(d, _) => {
            let call = d
                .callout_id
                .as_ref()
                .map(|id| format!("{id}:{}:{}", d.kind.as_deref().unwrap_or("callout"), d.reply_id.as_deref().unwrap_or("")));
            (d.event_id.clone(), None, call)
        }
        Payload::Thesis(d) => (d.event_id.clone(), None, d.thesis.id.clone()),
        Payload::Verified(d) => (d.event_id.clone(), None, Some(d.coin.address.clone())),
    };
    let name = match &post {
        Some(key) => key.clone(),
        None => event.clone().or(envelope_id.map(str::to_string)).or(own.clone()).unwrap_or_default(),
    };
    (format!("{pair} {name}"), Ids { event, post, payload: own })
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
        self.book.flush();
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
        let (trader, fire, decider) = (Arc::clone(trader), fire.clone(), Arc::clone(decider));
        match rule.token.action {
            Action::Buy => drop(tokio::spawn(async move { trader.buy(fire, trace).await })),
            Action::Sell => {
                let reason = format!("rule {}", rule.name);
                drop(tokio::spawn(async move {
                    if trader.sell(&fire.mint, &reason).await == SellResult::NoPosition {
                        decider.lock().unwrap_or_else(|p| p.into_inner()).release(&fire);
                    }
                }));
            }
        }
    }
    for exit in &outcome.exits {
        let (trader, exit) = (Arc::clone(trader), exit.clone());
        drop(tokio::spawn(async move { trader.exit(&exit.mint, exit.code.as_str(), &exit.detail).await }));
    }
    report.record_first_copy(&receipt.connection);
    if outcome.first_content
        && let Some(published) = outcome.published_at
    {
        report.record_post(&receipt.connection, (receipt.wall_ms - published) as f64);
    }
    for s in &outcome.signals {
        let who = s.actor.handle.as_ref().map_or_else(|| s.actor.namespace.as_str().to_string(), |h| format!("@{h}"));
        log("signal", &format!("{} {} from {who} ({})", s.source.as_str(), short(&s.mint), s.event_ref));
    }
    for s in &outcome.skips {
        log("skip", &format!("{}: {}, {}", config.rules[s.rule].name, s.code.as_str(), s.detail));
    }
    for f in &outcome.fires {
        let rule = &config.rules[f.rule];
        log("fire", &format!("{}: {} {}, {}", rule.name, rule.token.action.as_str(), short(&f.mint), f.detail));
    }
    for e in &outcome.exits {
        log("exit", &format!("{}: {}, {}", short(&e.mint), e.code.as_str(), e.detail));
    }
}
