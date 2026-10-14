//! The rule modes: for each, which activities and account keys it listens to ([`listens`]) and a
//! pure matcher ([`match_rule`]) that answers fire (with a mint), skip (with a stable code), exit,
//! or nothing. Next: `rules/engine.rs` applies the limits that need memory.
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
use crate::protocol::PostKind;
use crate::setup::config::{KeywordFilter, Mode, Rule};

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

/// Returns the activity types a rule listens to and the account keys it listens under.
pub fn listens(rule: &Rule) -> Vec<(ActivityType, String)> {
    let x = |types: &[ActivityType], specs: &[String]| {
        let mut keys = Vec::new();
        for t in types {
            for spec in specs {
                keys.push((*t, x_spec_key(spec)));
            }
        }
        keys
    };
    match rule.mode {
        Mode::CaScanner => x(&[ActivityType::Post, ActivityType::Profile, ActivityType::Pin], &rule.accounts),
        Mode::Engagement => x(&[ActivityType::Post], std::slice::from_ref(&rule.reactor)),
        Mode::NewTweet => x(&[ActivityType::Post, ActivityType::Profile], &rule.accounts),
        Mode::Follow => x(&[ActivityType::Follow], &rule.accounts),
        Mode::PfpUpdate => x(&[ActivityType::Profile], &rule.accounts),
        Mode::Callout => {
            let mut keys = Vec::new();
            for caller in &rule.callers {
                keys.push((ActivityType::Callout, format!("pf:{}", caller.to_lowercase())));
            }
            keys
        }
        Mode::Fomo => {
            let mut keys = Vec::new();
            for user in &rule.fomo_users {
                keys.push((ActivityType::Fomo, format!("fu:{}", user.to_lowercase())));
            }
            for coin in &rule.fomo_coins {
                keys.push((ActivityType::Fomo, format!("fc:{coin}")));
            }
            keys
        }
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

impl SkipCode {
    /// Returns the stable code.
    pub fn as_str(self) -> &'static str {
        match self {
            SkipCode::StalePost => "stale_post",
            SkipCode::KeywordIncludeMissing => "keyword_include_missing",
            SkipCode::KeywordExcludeFound => "keyword_exclude_found",
            SkipCode::NoContractAddress => "no_contract_address",
            SkipCode::EvmNotSupported => "evm_not_supported",
            SkipCode::LowConfidence => "low_confidence",
            SkipCode::MintNotInBio => "mint_not_in_bio",
            SkipCode::BioNotInPayload => "bio_not_in_payload",
            SkipCode::UpdateNotNew => "update_not_new",
            SkipCode::MultipleTooHigh => "multiple_too_high",
            SkipCode::CallerNotHolding => "caller_not_holding",
            SkipCode::Cooldown => "cooldown",
            SkipCode::MaxSnipes => "max_snipes",
        }
    }
}

/// Why a rule exits a position it opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitCode {
    /// The post the buy came from was deleted.
    PostDeleted,
    /// The caller reports holding none of the token.
    CallerExited,
    /// The FOMO author closed the position.
    AuthorExited,
}

impl ExitCode {
    /// Returns the stable code.
    pub fn as_str(self) -> &'static str {
        match self {
            ExitCode::PostDeleted => "post_deleted",
            ExitCode::CallerExited => "caller_exited",
            ExitCode::AuthorExited => "author_exited",
        }
    }
}

/// What a matcher returns when its rule fires. The engine adds the acting account to make a
/// [`Fire`].
#[derive(Debug, Clone)]
pub struct Hit {
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

impl Hit {
    /// Returns the fire, triggered by `actor`.
    pub fn fire(self, actor: Actor) -> Fire {
        let Hit { rule, mint, signal, detail, fire_key } = self;
        Fire { rule, mint, signal, detail, fire_key, actor }
    }
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
    /// Who triggered it, kept on the position for caller and author exits.
    pub actor: Actor,
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

/// A rule sells a position it bought, because the account it followed has left the trade.
#[derive(Debug, Clone)]
pub struct RuleExit {
    /// Index of the rule in the config.
    pub rule: usize,
    /// The caller or author who left; only positions they triggered are sold.
    pub actor: Actor,
    /// The mint to sell.
    pub mint: String,
    /// Why.
    pub code: ExitCode,
    /// Free text for logs.
    pub detail: String,
}

/// What one rule made of one activity.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// Trade.
    Fire(Hit),
    /// Do not trade, with a reason.
    Skip(Skip),
    /// Sell a position this rule opened.
    Exit(RuleExit),
}

fn skip(rule: usize, code: SkipCode, detail: String) -> Verdict {
    Verdict::Skip(Skip { rule, code, detail })
}

/// Returns the verdicts of one rule for one activity, in signal order.
pub fn match_rule(index: usize, rule: &Rule, activity: &Activity, known_bio: Option<&str>) -> Vec<Verdict> {
    let one = |v: Option<Verdict>| v.into_iter().collect();
    match (rule.mode, &activity.kind) {
        (Mode::CaScanner, _) => match_ca_scanner(index, rule, activity, known_bio),
        (Mode::Engagement, ActivityKind::Post(post)) => one(match_engagement(index, rule, post, &activity.signals)),
        (Mode::NewTweet, ActivityKind::Post(post)) => one(match_new_tweet_post(index, rule, post, &activity.signals)),
        (Mode::NewTweet, ActivityKind::Profile(profile)) => one(match_new_tweet_bio(index, rule, profile, activity)),
        (Mode::Follow, ActivityKind::Follow(follow)) => one(match_follow(index, rule, follow, activity)),
        (Mode::PfpUpdate, ActivityKind::Profile(profile)) => one(match_pfp(index, rule, profile, activity)),
        (Mode::Callout, ActivityKind::Callout(callout)) => match_callout(index, rule, callout, activity),
        (Mode::Fomo, ActivityKind::Fomo(fomo)) => match_fomo(index, rule, fomo, activity),
        _ => Vec::new(),
    }
}

fn keyword_skip(index: usize, rule: &Rule, texts: &[Vec<String>]) -> Option<Verdict> {
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
fn pick_mint(index: usize, rule: &Rule, signals: &[Signal], detail: String, fire_key: String) -> Verdict {
    if !rule.token.mint.is_empty() {
        return Verdict::Fire(Hit { rule: index, mint: rule.token.mint.clone(), signal: None, detail, fire_key });
    }
    if let Some(signal) = signals.iter().find(|s| s.chain == Chain::Solana) {
        let detail = format!("{detail}, CA from {}", signal.source.as_str());
        return Verdict::Fire(Hit { rule: index, mint: signal.mint.clone(), signal: Some(signal.clone()), detail, fire_key });
    }
    match signals.iter().find(|s| s.chain == Chain::Evm) {
        Some(evm) => {
            skip(index, SkipCode::EvmNotSupported, format!("{detail}: only an EVM address {}; execution is Solana-only", evm.mint))
        }
        None => skip(index, SkipCode::NoContractAddress, format!("{detail}: no contract address in the event yet")),
    }
}

/// The token the event names, limited to `token.mint` when the rule sets one.
fn event_token(index: usize, rule: &Rule, signals: &[Signal], detail: String) -> Option<Verdict> {
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
    Some(Verdict::Fire(Hit { rule: index, mint: signal.mint.clone(), signal: Some(signal.clone()), detail, fire_key }))
}

/// Mode `ca_scanner`: trade contract addresses the watched accounts post.
fn match_ca_scanner(index: usize, rule: &Rule, activity: &Activity, known_bio: Option<&str>) -> Vec<Verdict> {
    let mut out = Vec::new();
    for s in &activity.signals {
        if !rule.sources.contains(&s.source) {
            continue;
        }
        if !rule.token.mint.is_empty() && s.mint != rule.token.mint {
            continue;
        }
        if s.chain != Chain::Solana {
            out.push(skip(index, SkipCode::EvmNotSupported, format!("EVM address {} in {}", s.mint, s.source.as_str())));
        } else if s.confidence < rule.min_confidence {
            let detail = format!(
                "{} signal is {}, rule needs {}",
                s.source.as_str(),
                confidence_name(s.confidence),
                confidence_name(rule.min_confidence)
            );
            out.push(skip(index, SkipCode::LowConfidence, detail));
        } else if rule.require_mint_in_bio && !known_bio.unwrap_or("").contains(&s.mint) {
            out.push(skip(index, SkipCode::MintNotInBio, format!("{} is not in the account's last known bio", s.mint)));
        } else {
            let fire_key = format!("{}|{}", rule.name, s.mint);
            out.push(Verdict::Fire(Hit {
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
fn match_engagement(index: usize, rule: &Rule, a: &PostActivity, signals: &[Signal]) -> Option<Verdict> {
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
    Some(pick_mint(index, rule, signals, format!("{} of {what}", a.kind.as_str()), format!("{}|{}", rule.name, a.post_key)))
}

/// Mode `new_tweet` on posts: a watched account posts, retweets, quotes, or replies.
fn match_new_tweet_post(index: usize, rule: &Rule, a: &PostActivity, signals: &[Signal]) -> Option<Verdict> {
    if !rule.events.iter().any(|e| e == a.kind.as_str()) {
        return None;
    }
    if let Some(blocked) = keyword_skip(index, rule, &a.texts) {
        return Some(blocked);
    }
    Some(pick_mint(index, rule, signals, format!("new {}", a.kind.as_str()), format!("{}|{}", rule.name, a.post_key)))
}

/// Mode `new_tweet` on bios: a watched account changes its bio.
fn match_new_tweet_bio(index: usize, rule: &Rule, a: &ProfileActivity, activity: &Activity) -> Option<Verdict> {
    let bio = a.bio.as_ref()?;
    if !rule.events.iter().any(|e| e == "bio") {
        return None;
    }
    if let Some(blocked) = keyword_skip(index, rule, &[vec![bio.clone()]]) {
        return Some(blocked);
    }
    Some(pick_mint(index, rule, &activity.signals, "bio changed".to_string(), format!("{}|{}", rule.name, activity.event_ref)))
}

/// Mode `follow`: a watched account follows or unfollows someone.
fn match_follow(index: usize, rule: &Rule, a: &FollowActivity, activity: &Activity) -> Option<Verdict> {
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
fn match_pfp(index: usize, rule: &Rule, a: &ProfileActivity, activity: &Activity) -> Option<Verdict> {
    let mut hit = Vec::new();
    for field in &rule.fields {
        if a.changed.contains(field) {
            hit.push(field.as_str());
        }
    }
    if hit.is_empty() {
        return None;
    }
    let detail = format!("{} changed", hit.join(", "));
    Some(pick_mint(index, rule, &activity.signals, detail, format!("{}|{}", rule.name, activity.event_ref)))
}

/// Mode `callout`: a watched pump.fun caller posts a callout. `callout_update` only enriches an
/// earlier call, so it never buys; it can still report that the caller sold.
fn match_callout(index: usize, rule: &Rule, a: &CalloutActivity, activity: &Activity) -> Vec<Verdict> {
    let mut out = Vec::new();
    let caller = activity.actor.handle.as_deref().unwrap_or("");
    if let Some(signal) = activity.signals.first()
        && rule.sell_when_caller_exits
        && a.caller_holds == Some(false)
    {
        let detail = format!("@{caller} reports holding 0");
        out.push(Verdict::Exit(RuleExit {
            actor: activity.actor.clone(),
            rule: index,
            mint: signal.mint.clone(),
            code: ExitCode::CallerExited,
            detail,
        }));
    }
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
    let detail = format!("{} by {caller}", kind.as_str());
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
fn match_fomo(index: usize, rule: &Rule, a: &FomoActivity, activity: &Activity) -> Vec<Verdict> {
    let mut out = Vec::new();
    let by_watched_author = rule.fomo_users.iter().any(|u| matches_fomo_user(u, &activity.actor));
    if let Some(signal) = activity.signals.first()
        && a.op == FomoOp::Thesis
        && rule.sell_when_author_exits
        && a.author_exited
        && by_watched_author
    {
        let detail = format!("{} closed the position", activity.actor.handle.as_deref().unwrap_or(""));
        out.push(Verdict::Exit(RuleExit {
            actor: activity.actor.clone(),
            rule: index,
            mint: signal.mint.clone(),
            code: ExitCode::AuthorExited,
            detail,
        }));
    }
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
