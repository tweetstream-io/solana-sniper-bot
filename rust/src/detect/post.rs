//! Detectors for posts: `tweet/content`, `tweet/update` (after merging), and `tweet/meta`. Pure and
//! synchronous, so detection costs microseconds and tests need no network. Next: `rules/engine.rs`.

use std::collections::HashSet;

use crate::protocol::{Author, Reference, TweetMeta, normalize_handle};

use super::addresses::{FoundAddress, addresses_from_url, classify_address, find_all};
use super::store::Post;
use super::types::{Actor, Chain, Confidence, Namespace, PostActivity, PostTarget, Signal, SignalSource};

const MAX_REF_DEPTH: usize = 7;

/// Returns an account's effective platform: its own `platform`, else `fallback` (the event's or
/// the outer account's effective platform).
pub fn effective_platform<'a>(author: Option<&'a Author>, fallback: &'a str) -> &'a str {
    author.and_then(|a| a.platform.as_deref()).unwrap_or(fallback)
}

/// Returns the actor for a post author or account: `x` when its effective platform is twitter, the
/// platform name elsewhere.
pub fn x_actor(author: Option<&Author>, fallback: &str) -> Actor {
    Actor {
        namespace: Namespace::for_platform(effective_platform(author, fallback)),
        id: author.and_then(|a| a.id.clone()),
        handle: normalize_handle(author.and_then(|a| a.handle.as_deref())),
    }
}

/// The parts of a signal that are the same for every address in one event.
pub struct SignalContext<'a> {
    /// Who posted.
    pub actor: &'a Actor,
    /// `"<t>/<op> <key>"`.
    pub event_ref: &'a str,
    /// The post that carried it, if any.
    pub post_key: Option<&'a str>,
    /// Market cap the event carried, if any.
    pub market_cap_usd: Option<f64>,
}

impl SignalContext<'_> {
    /// Returns a signal for one address.
    pub fn signal(&self, source: SignalSource, found: &FoundAddress, confidence: Confidence, detail: String) -> Signal {
        Signal {
            source,
            chain: found.chain,
            mint: found.address.clone(),
            actor: self.actor.clone(),
            event_ref: self.event_ref.to_string(),
            confidence,
            detail,
            post_key: self.post_key.map(str::to_string),
            market_cap_usd: self.market_cap_usd,
        }
    }
}

/// Returns the referenced posts by depth: level 1 is the direct reference, level 2 what that one
/// references, at most seven levels (the most TweetStream sends).
pub fn ref_levels(reference: Option<&Reference>) -> Vec<Vec<&Reference>> {
    let mut levels = Vec::new();
    let mut current: Vec<&Reference> = reference.into_iter().collect();
    while !current.is_empty() && levels.len() < MAX_REF_DEPTH {
        let mut next = Vec::new();
        for r in &current {
            for child in [&r.quoted, &r.subtweet].into_iter().flatten() {
                next.push(child.as_ref());
            }
        }
        levels.push(current);
        current = next;
    }
    levels
}

/// Returns every contract address in a post, strongest evidence first, one signal per mint.
pub fn detect_post_signals(post: &Post, ctx: &SignalContext) -> Vec<Signal> {
    let mut out = Vec::new();
    for url in post.urls.iter().flatten() {
        for found in addresses_from_url(&url.url) {
            out.push(ctx.signal(SignalSource::Url, &found, found.confidence, format!("{} link", found.via)));
        }
    }
    for found in find_all(post.text.as_deref()) {
        if found.via == "address" {
            out.push(ctx.signal(SignalSource::Text, &found, Confidence::High, "in post text".to_string()));
        } else {
            out.push(ctx.signal(SignalSource::Url, &found, found.confidence, format!("{} link in text", found.via)));
        }
    }
    for level in ref_levels(post.reference.as_ref()) {
        for r in level {
            for found in find_all(r.text.as_deref()) {
                let detail = format!("in {} of {}", r.kind, r.tweet_id);
                out.push(ctx.signal(SignalSource::Reference, &found, Confidence::Medium, detail));
            }
        }
    }
    first_per_mint(out)
}

/// Returns the signals of a `tweet/meta` snapshot. Meta carries no author, so the context holds the
/// author of the stored post.
pub fn detect_meta_signals(meta: &TweetMeta, ctx: &SignalContext) -> Vec<Signal> {
    let mut out = Vec::new();
    let tokens = meta.detected.as_ref().and_then(|d| d.tokens.as_ref());
    for token in tokens.into_iter().flatten() {
        let Some((Chain::Solana, address)) = token.contract.as_deref().and_then(classify_address) else {
            continue;
        };
        let from_text = token.sources.as_ref().is_some_and(|s| s.iter().any(|source| source == "text"));
        let confidence = if from_text { Confidence::High } else { Confidence::Medium };
        let label = token.symbol.as_ref().map_or_else(|| "token".to_string(), |s| format!("${s}"));
        let found = FoundAddress { chain: Chain::Solana, address, via: "address".to_string(), confidence };
        out.push(ctx.signal(SignalSource::Meta, &found, confidence, format!("{label} detected")));
    }
    let ocr = meta.ocr.as_ref().and_then(|o| o.text.as_deref());
    for found in find_all(ocr) {
        out.push(ctx.signal(SignalSource::Ocr, &found, Confidence::Medium, "in image text".to_string()));
    }
    first_per_mint(out)
}

/// Returns what the post's author did, for the engagement and new tweet modes, or `None` until the
/// post's kind is known. `platform` is the post author's effective platform; a referenced author
/// without its own platform takes it.
pub fn post_activity(post: &Post, post_key: &str, platform: &str) -> Option<PostActivity> {
    let kind = post.kind.filter(|k| *k != crate::protocol::PostKind::Other)?;
    let mut texts = vec![vec![post.text.clone().unwrap_or_default()]];
    for level in ref_levels(post.reference.as_ref()) {
        let mut level_texts = Vec::new();
        for r in level {
            level_texts.push(r.text.clone().unwrap_or_default());
        }
        texts.push(level_texts);
    }
    let target =
        post.reference.as_ref().map(|r| PostTarget { tweet_id: r.tweet_id.clone(), account: x_actor(r.author.as_ref(), platform) });
    Some(PostActivity { kind, post_key: post_key.to_string(), target, texts })
}

/// Keeps the first signal for each mint.
pub fn first_per_mint(list: Vec<Signal>) -> Vec<Signal> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for signal in list {
        if seen.insert(signal.mint.clone()) {
            out.push(signal);
        }
    }
    out
}
