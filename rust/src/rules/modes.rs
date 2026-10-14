//! The rule modes: for each, which activities and account keys it listens to ([`listens`]) and a
//! pure matcher ([`match_rule`]) that answers fire (with a mint), skip (with a stable code), or
//! nothing. Next: `rules/engine.rs` applies the limits that need memory.
//!
//! The matching conventions live here: X accounts match by numeric id or by handle without case,
//! only for `x` actors; pump.fun callers and FOMO users match without case; FOMO coins match
//! exactly; keywords match as whole words without case.

use regex::Regex;
use serde::Serialize;

use crate::detect::types::{
    Activity, ActivityKind, Actor, CalloutActivity, CalloutOp, Chain, FollowActivity, FomoActivity, FomoOp, Namespace, PostActivity,
    ProfileActivity, Signal,
};
use crate::log::wire_name;
use crate::protocol::PostKind;
use crate::setup::config::{KeywordFilter, Mode, NewTweetEvent, Rule};

/// X accounts are keyed `id:<id>` or `h:<handle>`, pump.fun callers `pf:<name>`, FOMO authors
/// `fu:<id or name>`, FOMO coins `fc:<address>`.
fn x_spec_key(spec: &str) -> String {
    let value = spec.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return format!("id:{value}");
    }
    format!("h:{}", value.strip_prefix('@').unwrap_or(value).to_lowercase())
}

/// Returns the keys an actor is indexed under. Only `x` actors have X keys, so a post author on
/// another platform never matches an X rule.
pub fn actor_keys(actor: &Actor) -> Vec<String> {
    let mut keys = Vec::new();
    let prefix = match actor.namespace {
        Namespace::X => {
            if let Some(id) = &actor.id {
                keys.push(format!("id:{id}"));
            }
            if let Some(handle) = &actor.handle {
                keys.push(format!("h:{}", handle.to_lowercase()));
            }
            return keys;
        }
        Namespace::PumpFun => "pf:",
        Namespace::Fomo => "fu:",
        Namespace::Platform(_) | Namespace::None => return keys,
    };
    for value in [&actor.id, &actor.handle].into_iter().flatten() {
        keys.push(format!("{prefix}{}", value.to_lowercase()));
    }
    keys
}

/// Returns true when an account spec (numeric id or handle) names this X actor.
pub fn matches_account(spec: &str, actor: &Actor) -> bool {
    let key = x_spec_key(spec);
    actor.namespace == Namespace::X && actor_keys(actor).contains(&key)
}

/// Returns true when a FOMO user spec names this author, by id or username, without case.
pub fn matches_fomo_user(spec: &str, actor: &Actor) -> bool {
    let spec = spec.to_lowercase();
    [&actor.id, &actor.handle].into_iter().flatten().any(|v| v.to_lowercase() == spec)
}

/// Builds the whole-word, case-insensitive pattern for one keyword: "cat" matches "Cat!" but not
/// "catalog".
pub fn keyword_pattern(word: &str) -> Regex {
    Regex::new(&format!(r"(?i)(^|[^\p{{L}}\p{{N}}_]){}($|[^\p{{L}}\p{{N}}_])", regex::escape(word)))
        .expect("escaped keyword is a valid pattern")
}

/// Returns the first keyword of the filter that appears in the texts up to its scan depth.
/// `texts[0]` is the actor's own text; `texts[n]` is n levels of referenced posts.
pub fn find_word<'a>(texts: &[Vec<String>], filter: &'a KeywordFilter) -> Option<&'a str> {
    let scanned = &texts[..texts.len().min(filter.scan_depth + 1)];
    for (word, pattern) in filter.words.iter().zip(&filter.patterns) {
        for level in scanned {
            if level.iter().any(|text| pattern.is_match(text)) {
                return Some(word);
            }
        }
    }
    None
}

/// The activity types a rule can listen to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActivityType {
    Post,
    Follow,
    Profile,
    Pin,
    Callout,
    Fomo,
}

/// Returns the type of an activity.
pub fn activity_type(kind: &ActivityKind) -> ActivityType {
    match kind {
        ActivityKind::Post(_) => ActivityType::Post,
        ActivityKind::Follow(_) => ActivityType::Follow,
        ActivityKind::Profile(_) => ActivityType::Profile,
        ActivityKind::Pin => ActivityType::Pin,
        ActivityKind::Callout(_) => ActivityType::Callout,
        ActivityKind::Fomo(_) => ActivityType::Fomo,
    }
}

/// Stable skip codes, shared with the TypeScript bot through `shared/fixtures`. Free text goes in
/// the detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipCode {
    /// The post is older than `maxSignalAgeMs`.
    StalePost,
    /// None of the include keywords appears.
    KeywordIncludeMissing,
    /// An exclude keyword appears.
    KeywordExcludeFound,
    /// The event carries no usable address.
    NoContractAddress,
    /// Only an EVM address; execution is Solana-only.
    EvmNotSupported,
    /// The signal is below `minConfidence`.
    LowConfidence,
    /// The mint is not in the account's last known bio.
    MintNotInBio,
    /// The follow payload has no target bio.
    BioNotInPayload,
    /// A `callout_update` never buys.
    UpdateNotNew,
    /// The token already ran past `maxMultiple`.
    MultipleTooHigh,
    /// The event does not show the caller holding the token.
    CallerNotHolding,
    /// The rule fired less than `cooldownSec` ago.
    Cooldown,
    /// The rule used all of `maxSnipes`.
    MaxSnipes,
}

/// A rule that fired on a mint.
#[derive(Debug, Clone)]
pub struct Fire {
    /// Index of the rule in the config.
    pub rule: usize,
    /// The mint to trade.
    pub mint: String,
    /// The signal that supplied the mint; `None` when the rule names the mint.
    pub signal: Option<Signal>,
    /// Free text for logs.
    pub detail: String,
    /// A rule fires once per key: per post, per account event, or per mint.
    pub fire_key: String,
}

/// A rule looked at the event and decided not to trade.
#[derive(Debug, Clone)]
pub struct Skip {
    /// Index of the rule in the config.
    pub rule: usize,
    /// Stable code.
    pub code: SkipCode,
    /// Free text for logs.
    pub detail: String,
}

/// What one rule made of one activity.
#[derive(Debug, Clone)]
#[expect(clippy::large_enum_variant, reason = "verdicts live briefly in a Vec; boxing a fire would allocate on the hot path")]
pub enum RuleVerdict {
    /// Trade.
    Fire(Fire),
    /// Do not trade, with a reason.
    Skip(Skip),
}

fn skip(rule: usize, code: SkipCode, detail: String) -> RuleVerdict {
    RuleVerdict::Skip(Skip { rule, code, detail })
}

fn keyword_skip(index: usize, rule: &Rule, texts: &[Vec<String>]) -> Option<RuleVerdict> {
    let include = &rule.include_keywords;
    if !include.words.is_empty() && find_word(texts, include).is_none() {
        let detail = format!("none of [{}] within scan depth {}", include.words.join(", "), include.scan_depth);
        return Some(skip(index, SkipCode::KeywordIncludeMissing, detail));
    }
    let exclude = &rule.exclude_keywords;
    let found = find_word(texts, exclude)?;
    Some(skip(index, SkipCode::KeywordExcludeFound, format!("\"{found}\" within scan depth {}", exclude.scan_depth)))
}

/// The configured mint, or with `useDetectedCa` the first Solana address the event carried.
fn pick_mint(index: usize, rule: &Rule, signals: &[Signal], detail: String, fire_key: String) -> RuleVerdict {
    if !rule.token.mint.is_empty() {
        return RuleVerdict::Fire(Fire { rule: index, mint: rule.token.mint.clone(), signal: None, detail, fire_key });
    }
    if let Some(signal) = signals.iter().find(|s| s.chain == Chain::Solana) {
        let detail = format!("{detail}, CA from {}", wire_name(&signal.source));
        return RuleVerdict::Fire(Fire { rule: index, mint: signal.mint.clone(), signal: Some(signal.clone()), detail, fire_key });
    }
    match signals.iter().find(|s| s.chain == Chain::Evm) {
        Some(evm) => {
            skip(index, SkipCode::EvmNotSupported, format!("{detail}: only an EVM address {}; execution is Solana-only", evm.mint))
        }
        None => skip(index, SkipCode::NoContractAddress, format!("{detail}: no contract address in the event yet")),
    }
}

/// The token the event names, limited to `token.mint` when the rule sets one.
fn event_token(index: usize, rule: &Rule, signals: &[Signal], detail: String) -> Option<RuleVerdict> {
    let Some(signal) = signals.first() else {
        return Some(skip(index, SkipCode::NoContractAddress, format!("{detail}: the token address is not a valid Solana or EVM address")));
    };
    if !rule.token.mint.is_empty() && signal.mint != rule.token.mint {
        return None;
    }
    if signal.chain != Chain::Solana {
        return Some(skip(index, SkipCode::EvmNotSupported, format!("{detail}: EVM token {}; execution is Solana-only", signal.mint)));
    }
    let fire_key = format!("{}|{}", rule.name, signal.mint);
    Some(RuleVerdict::Fire(Fire { rule: index, mint: signal.mint.clone(), signal: Some(signal.clone()), detail, fire_key }))
}

/// Mode `ca_scanner`: trade contract addresses the watched accounts post.
fn match_ca_scanner(index: usize, rule: &Rule, activity: &Activity, known_bio: Option<&str>) -> Vec<RuleVerdict> {
    let mut out = Vec::new();
    for s in &activity.signals {
        if !rule.sources.contains(&s.source) {
            continue;
        }
        if !rule.token.mint.is_empty() && s.mint != rule.token.mint {
            continue;
        }
        if s.chain != Chain::Solana {
            out.push(skip(index, SkipCode::EvmNotSupported, format!("EVM address {} in {}", s.mint, wire_name(&s.source))));
        } else if s.confidence < rule.min_confidence {
            let detail = format!(
                "{} signal is {}, rule needs {}",
                wire_name(&s.source),
                confidence_name(s.confidence),
                confidence_name(rule.min_confidence)
            );
            out.push(skip(index, SkipCode::LowConfidence, detail));
        } else if rule.require_mint_in_bio && !known_bio.unwrap_or("").contains(&s.mint) {
            out.push(skip(index, SkipCode::MintNotInBio, format!("{} is not in the account's last known bio", s.mint)));
        } else {
            let fire_key = format!("{}|{}", rule.name, s.mint);
            out.push(RuleVerdict::Fire(Fire {
                rule: index,
                mint: s.mint.clone(),
                signal: Some(s.clone()),
                detail: format!("CA {}", s.detail),
                fire_key,
            }));
        }
    }
    out
}

fn confidence_name(c: crate::detect::types::Confidence) -> &'static str {
    match c {
        crate::detect::types::Confidence::Low => "low",
        crate::detect::types::Confidence::Medium => "medium",
        crate::detect::types::Confidence::High => "high",
    }
}

/// Mode `engagement`: the reactor retweets, quotes, or replies to the target post or a target
/// account.
fn match_engagement(index: usize, rule: &Rule, a: &PostActivity, signals: &[Signal]) -> Option<RuleVerdict> {
    let action = match a.kind {
        PostKind::Quote | PostKind::Reply | PostKind::Retweet => a.kind,
        PostKind::Post | PostKind::Other => return None,
    };
    if !rule.actions.contains(&action) {
        return None;
    }
    let target = a.target.as_ref()?;
    let on_target = if rule.target_tweet_id.is_empty() {
        rule.reacted_to.iter().any(|spec| matches_account(spec, &target.account))
    } else {
        target.tweet_id == rule.target_tweet_id
    };
    if !on_target {
        return None;
    }
    if let Some(blocked) = keyword_skip(index, rule, &a.texts) {
        return Some(blocked);
    }
    let what = if rule.target_tweet_id.is_empty() {
        format!("@{}", target.account.handle.as_deref().or(target.account.id.as_deref()).unwrap_or(""))
    } else {
        format!("post {}", rule.target_tweet_id)
    };
    Some(pick_mint(index, rule, signals, format!("{} of {what}", wire_name(&a.kind)), format!("{}|{}", rule.name, a.post_key)))
}

/// Mode `new_tweet` on posts: a watched account posts, retweets, quotes, or replies.
fn match_new_tweet_post(index: usize, rule: &Rule, a: &PostActivity, signals: &[Signal]) -> Option<RuleVerdict> {
    if !rule.events.contains(&NewTweetEvent::from(a.kind)) {
        return None;
    }
    if let Some(blocked) = keyword_skip(index, rule, &a.texts) {
        return Some(blocked);
    }
    Some(pick_mint(index, rule, signals, format!("new {}", wire_name(&a.kind)), format!("{}|{}", rule.name, a.post_key)))
}

/// Mode `new_tweet` on bios: a watched account changes its bio.
fn match_new_tweet_bio(index: usize, rule: &Rule, a: &ProfileActivity, activity: &Activity) -> Option<RuleVerdict> {
    let bio = a.bio.as_ref()?;
    if !rule.events.contains(&NewTweetEvent::Bio) {
        return None;
    }
    if let Some(blocked) = keyword_skip(index, rule, &[vec![bio.clone()]]) {
        return Some(blocked);
    }
    Some(pick_mint(index, rule, &activity.signals, "bio changed".to_string(), format!("{}|{}", rule.name, activity.event_ref)))
}

/// Mode `follow`: a watched account follows or unfollows someone.
fn match_follow(index: usize, rule: &Rule, a: &FollowActivity, activity: &Activity) -> Option<RuleVerdict> {
    if !rule.follow_events.contains(&a.op) {
        return None;
    }
    if !rule.targets.is_empty() && !rule.targets.iter().any(|spec| matches_account(spec, &a.target)) {
        return None;
    }
    let op = match a.op {
        crate::detect::types::FollowOp::Follow => "follow",
        crate::detect::types::FollowOp::Unfollow => "unfollow",
    };
    let detail = format!("{op} of @{}", a.target.handle.as_deref().unwrap_or(""));
    if rule.token.mint.is_empty() && rule.use_detected_ca && !a.bio_in_payload {
        let detail = format!("{detail}: no target bio in the event, and this example does not look profiles up");
        return Some(skip(index, SkipCode::BioNotInPayload, detail));
    }
    Some(pick_mint(index, rule, &activity.signals, detail, format!("{}|{}", rule.name, activity.event_ref)))
}

/// Mode `pfp_update`: a watched account changes its avatar, or another selected profile field.
fn match_pfp(index: usize, rule: &Rule, a: &ProfileActivity, activity: &Activity) -> Option<RuleVerdict> {
    let mut hit = Vec::new();
    for field in &rule.fields {
        if a.changed.contains(field) {
            hit.push(wire_name(field));
        }
    }
    if hit.is_empty() {
        return None;
    }
    let detail = format!("{} changed", hit.join(", "));
    Some(pick_mint(index, rule, &activity.signals, detail, format!("{}|{}", rule.name, activity.event_ref)))
}

/// Mode `callout`: a watched pump.fun caller posts a callout. `callout_update` only enriches an
/// earlier call, so it never buys.
fn match_callout(index: usize, rule: &Rule, a: &CalloutActivity, activity: &Activity) -> Vec<RuleVerdict> {
    let mut out = Vec::new();
    let caller = activity.actor.handle.as_deref().unwrap_or("");
    let watched = a.kind.is_some_and(|k| rule.callout_kinds.contains(&k));
    if a.op == CalloutOp::Update {
        if watched {
            out.push(skip(index, SkipCode::UpdateNotNew, "callout_update enriches an earlier call".to_string()));
        }
        return out;
    }
    let Some(kind) = a.kind.filter(|_| watched) else {
        return out;
    };
    let detail = format!("{} by {caller}", wire_name(&kind));
    match (rule.max_multiple, a.multiple) {
        (Some(max), Some(multiple)) if multiple > max => {
            out.push(skip(index, SkipCode::MultipleTooHigh, format!("{detail}: already {multiple}x, rule allows {max}x")));
        }
        _ if rule.require_caller_position && a.caller_holds != Some(true) => {
            out.push(skip(index, SkipCode::CallerNotHolding, format!("{detail}: the event does not show the caller holding the token")));
        }
        _ => out.extend(event_token(index, rule, &activity.signals, detail)),
    }
    out
}

/// Mode `fomo`: a FOMO thesis by a watched author or on a watched coin, or a watched coin's first
/// observed verification.
fn match_fomo(index: usize, rule: &Rule, a: &FomoActivity, activity: &Activity) -> Vec<RuleVerdict> {
    let mut out = Vec::new();
    if a.author_exited || !rule.fomo_events.contains(&a.op) {
        return out;
    }
    if a.op == FomoOp::TokenVerified && !rule.fomo_coins.contains(&a.coin) {
        return out;
    }
    if a.op == FomoOp::Thesis
        && let Some(blocked) = keyword_skip(index, rule, &[vec![a.text.clone()]])
    {
        out.push(blocked);
        return out;
    }
    let detail = match a.op {
        FomoOp::Thesis => format!("thesis by {}", activity.actor.handle.as_deref().unwrap_or("")),
        FomoOp::TokenVerified => "first observed verification".to_string(),
    };
    out.extend(event_token(index, rule, &activity.signals, detail));
    out
}

/// One rule mode: the activities it listens to, the account keys it listens under, and its matcher.
struct ModeEntry {
    activities: &'static [ActivityType],
    keys: fn(&Rule) -> Vec<String>,
    matcher: fn(usize, &Rule, &Activity, Option<&str>) -> Vec<RuleVerdict>,
}

fn x_keys(specs: &[String]) -> Vec<String> {
    specs.iter().map(|s| x_spec_key(s)).collect()
}

fn one(verdict: Option<RuleVerdict>) -> Vec<RuleVerdict> {
    verdict.into_iter().collect()
}

/// Every mode in one table.
fn mode_entry(mode: Mode) -> ModeEntry {
    match mode {
        Mode::CaScanner => ModeEntry {
            activities: &[ActivityType::Post, ActivityType::Profile, ActivityType::Pin],
            keys: |rule| x_keys(&rule.accounts),
            matcher: match_ca_scanner,
        },
        Mode::Engagement => ModeEntry {
            activities: &[ActivityType::Post],
            keys: |rule| x_keys(std::slice::from_ref(&rule.reactor)),
            matcher: |i, rule, a, _| match &a.kind {
                ActivityKind::Post(post) => one(match_engagement(i, rule, post, &a.signals)),
                _ => Vec::new(),
            },
        },
        Mode::NewTweet => ModeEntry {
            activities: &[ActivityType::Post, ActivityType::Profile],
            keys: |rule| x_keys(&rule.accounts),
            matcher: |i, rule, a, _| match &a.kind {
                ActivityKind::Post(post) => one(match_new_tweet_post(i, rule, post, &a.signals)),
                ActivityKind::Profile(profile) => one(match_new_tweet_bio(i, rule, profile, a)),
                _ => Vec::new(),
            },
        },
        Mode::Follow => ModeEntry {
            activities: &[ActivityType::Follow],
            keys: |rule| x_keys(&rule.accounts),
            matcher: |i, rule, a, _| match &a.kind {
                ActivityKind::Follow(follow) => one(match_follow(i, rule, follow, a)),
                _ => Vec::new(),
            },
        },
        Mode::PfpUpdate => ModeEntry {
            activities: &[ActivityType::Profile],
            keys: |rule| x_keys(&rule.accounts),
            matcher: |i, rule, a, _| match &a.kind {
                ActivityKind::Profile(profile) => one(match_pfp(i, rule, profile, a)),
                _ => Vec::new(),
            },
        },
        Mode::Callout => ModeEntry {
            activities: &[ActivityType::Callout],
            keys: |rule| rule.callers.iter().map(|c| format!("pf:{}", c.to_lowercase())).collect(),
            matcher: |i, rule, a, _| match &a.kind {
                ActivityKind::Callout(callout) => match_callout(i, rule, callout, a),
                _ => Vec::new(),
            },
        },
        Mode::Fomo => ModeEntry {
            activities: &[ActivityType::Fomo],
            keys: |rule| {
                let mut keys: Vec<String> = rule.fomo_users.iter().map(|u| format!("fu:{}", u.to_lowercase())).collect();
                keys.extend(rule.fomo_coins.iter().map(|c| format!("fc:{c}")));
                keys
            },
            matcher: |i, rule, a, _| match &a.kind {
                ActivityKind::Fomo(fomo) => match_fomo(i, rule, fomo, a),
                _ => Vec::new(),
            },
        },
    }
}

/// Returns the activity types a rule listens to and the account keys it listens under.
pub fn listens(rule: &Rule) -> Vec<(ActivityType, String)> {
    let entry = mode_entry(rule.mode);
    let mut out = Vec::new();
    for key in (entry.keys)(rule) {
        for activity in entry.activities {
            out.push((*activity, key.clone()));
        }
    }
    out
}

/// Returns the verdicts of one rule for one activity, in signal order.
pub fn match_rule(index: usize, rule: &Rule, activity: &Activity, known_bio: Option<&str>) -> Vec<RuleVerdict> {
    (mode_entry(rule.mode).matcher)(index, rule, activity, known_bio)
}
