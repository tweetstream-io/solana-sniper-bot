//! Finds contract addresses (CAs) in text and links, following the rules in
//! `shared/addresses.json`. Used by `detect/post.rs` and `detect/account.rs`. A regex only finds
//! candidates: a Solana candidate must decode to exactly 32 bytes, because many base58-looking
//! strings (signatures, words, ids) do not. A "pump" suffix proves nothing.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

use super::types::{Chain, Confidence};

/// The detection rules both bots load, compiled into the binary from `shared/`.
const RULES_TEXT: &str = include_str!("../../../shared/addresses.json");

#[derive(Deserialize)]
struct RawPattern {
    pattern: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSolana {
    pattern: String,
    decoded_bytes: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawStandalone {
    word_chars: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLinkInText {
    pattern: String,
    flags: String,
    strip_trailing: String,
}

#[derive(Deserialize)]
struct RawLink {
    via: String,
    host: String,
    path: String,
    confidence: Confidence,
}

#[derive(Deserialize)]
struct RawDenied {
    address: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRules {
    solana: RawSolana,
    evm: RawPattern,
    standalone: RawStandalone,
    link_in_text: RawLinkInText,
    links: Vec<RawLink>,
    denylist: Vec<RawDenied>,
}

struct Link {
    via: String,
    host: Regex,
    path: Regex,
    confidence: Confidence,
}

struct Rules {
    solana_run: Regex,
    solana_exact: Regex,
    decoded_bytes: usize,
    evm_run: Regex,
    evm_exact: Regex,
    word_char: Regex,
    link_in_text: Regex,
    strip_trailing: Vec<char>,
    links: Vec<Link>,
    denylist: HashMap<String, String>,
}

/// JavaScript flags become inline flags; `g` only means "find all", which `find_iter` does anyway.
fn with_flags(pattern: &str, flags: &str) -> String {
    let inline: String = flags.chars().filter(|f| *f == 'i').collect();
    if inline.is_empty() { pattern.to_string() } else { format!("(?{inline}){pattern}") }
}

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("shared/addresses.json holds valid patterns")
}

static RULES: LazyLock<Rules> = LazyLock::new(|| {
    let raw: RawRules = serde_json::from_str(RULES_TEXT).expect("shared/addresses.json is valid");
    let mut links = Vec::new();
    for link in raw.links {
        links.push(Link { via: link.via, host: compile(&link.host), path: compile(&link.path), confidence: link.confidence });
    }
    let mut denylist = HashMap::new();
    for denied in raw.denylist {
        denylist.insert(denied.address, denied.name);
    }
    Rules {
        solana_run: compile(&raw.solana.pattern),
        solana_exact: compile(&format!("^(?:{})$", raw.solana.pattern)),
        decoded_bytes: raw.solana.decoded_bytes,
        evm_run: compile(&raw.evm.pattern),
        evm_exact: compile(&format!("^(?:{})$", raw.evm.pattern)),
        word_char: compile(&raw.standalone.word_chars),
        link_in_text: compile(&with_flags(&raw.link_in_text.pattern, &raw.link_in_text.flags)),
        strip_trailing: raw.link_in_text.strip_trailing.chars().collect(),
        links,
        denylist,
    }
});

/// One address found in a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundAddress {
    /// Solana or EVM.
    pub chain: Chain,
    /// The address; EVM addresses are lowercased.
    pub address: String,
    /// `address` for a bare address, otherwise the link kind (`pump.fun`, `dexscreener`, ...).
    pub via: String,
    /// High for a bare address; a link's confidence comes from `shared/addresses.json`.
    pub confidence: Confidence,
}

/// The one place that decides what an address is: Solana, EVM, or nothing (not an address, or on
/// the denylist of base assets and programs). EVM addresses come back lowercased.
pub fn classify_address(candidate: &str) -> Option<(Chain, String)> {
    let rules = &*RULES;
    if rules.solana_exact.is_match(candidate) {
        let mut bytes = [0u8; 64];
        let decoded = bs58::decode(candidate).onto(&mut bytes).ok()?;
        if decoded != rules.decoded_bytes || rules.denylist.contains_key(candidate) {
            return None;
        }
        return Some((Chain::Solana, candidate.to_string()));
    }
    if rules.evm_exact.is_match(candidate) {
        let address = candidate.to_ascii_lowercase();
        return if rules.denylist.contains_key(&address) { None } else { Some((Chain::Evm, address)) };
    }
    None
}

/// Returns true for a Solana address that is not on the denylist.
///
/// # Examples
///
/// ```
/// use solana_sniper_bot::detect::addresses::is_solana_address;
/// assert!(is_solana_address("FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA"));
/// assert!(!is_solana_address("4uQeVj5tqViQh7yWWGStvkEG1Zmhx6uasJtWCJziofL"));
/// ```
pub fn is_solana_address(candidate: &str) -> bool {
    match classify_address(candidate) {
        Some((Chain::Solana, _)) => true,
        Some((Chain::Evm, _)) | None => false,
    }
}

/// Returns what a denylisted address is (such as "wrapped SOL mint"), or `None` for any other.
pub fn denylisted(candidate: &str) -> Option<&'static str> {
    RULES.denylist.get(candidate).map(String::as_str)
}

/// A candidate glued to more letters or digits is a slice of something longer (a signature, a
/// slug). Punctuation around it ("CA:", "$", "(", ".") is fine.
fn standalone(text: &str, start: usize, end: usize) -> bool {
    let word_char = &RULES.word_char;
    let glued = |c: Option<char>| c.is_some_and(|c| word_char.is_match(c.encode_utf8(&mut [0u8; 4])));
    !glued(text[..start].chars().next_back()) && !glued(text[end..].chars().next())
}

struct Located {
    found: FoundAddress,
    at: usize,
}

fn locate_addresses(text: &str) -> Vec<Located> {
    let mut found = Vec::new();
    for run in [&RULES.evm_run, &RULES.solana_run] {
        for m in run.find_iter(text) {
            if !standalone(text, m.start(), m.end()) {
                continue;
            }
            if let Some((chain, address)) = classify_address(m.as_str()) {
                let found_address = FoundAddress { chain, address, via: "address".to_string(), confidence: Confidence::High };
                found.push(Located { found: found_address, at: m.start() });
            }
        }
    }
    found
}

/// Returns the plain addresses written anywhere in free text (post text, bio, image text, name).
pub fn find_addresses(text: Option<&str>) -> Vec<FoundAddress> {
    match text {
        Some(text) if !text.is_empty() => first_appearance(locate_addresses(text)),
        Some(_) | None => Vec::new(),
    }
}

/// Returns the mint a token link names, if it is one of the token-link forms in
/// `shared/addresses.json`.
pub fn addresses_from_url(raw: &str) -> Vec<FoundAddress> {
    let parsed = if raw.starts_with("http") { url::Url::parse(raw) } else { url::Url::parse(&format!("https://{raw}")) };
    let Ok(url) = parsed else {
        return Vec::new();
    };
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    for link in &RULES.links {
        if !link.host.is_match(&host) {
            continue;
        }
        let candidate = link.path.captures(url.path()).and_then(|c| c.get(1)).map_or("", |m| m.as_str());
        if let Some((Chain::Solana, address)) = classify_address(candidate) {
            return vec![FoundAddress { chain: Chain::Solana, address, via: link.via.clone(), confidence: link.confidence }];
        }
    }
    Vec::new()
}

/// Returns every address in the text, in order of first appearance, one per address. When an
/// address also appears inside a token link, the link form is kept, because a link names the mint
/// more reliably.
pub fn find_all(text: Option<&str>) -> Vec<FoundAddress> {
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return Vec::new();
    };
    let mut all = Vec::new();
    let mut linked = HashMap::new();
    for m in RULES.link_in_text.find_iter(text) {
        let link = m.as_str().trim_end_matches(RULES.strip_trailing.as_slice());
        for found in addresses_from_url(link) {
            linked.insert(found.address.clone(), (found.via.clone(), found.confidence));
            all.push(Located { found, at: m.start() });
        }
    }
    all.extend(locate_addresses(text));
    for located in &mut all {
        if let Some((via, confidence)) = linked.get(&located.found.address) {
            located.found.via = via.clone();
            located.found.confidence = *confidence;
        }
    }
    first_appearance(all)
}

fn first_appearance(mut list: Vec<Located>) -> Vec<FoundAddress> {
    list.sort_by_key(|l| l.at);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for Located { found, at: _ } in list {
        if seen.insert(found.address.clone()) {
            out.push(found);
        }
    }
    out
}
