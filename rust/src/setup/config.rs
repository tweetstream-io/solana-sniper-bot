//! Loads `config.json`: fills defaults from `shared/config.schema.json`, validates against the same
//! schema the TypeScript bot uses, then checks what a schema cannot (32-byte mints, placeholders).
//! Next: `pipeline.rs` builds the rule index from the result.

use std::fmt;
use std::path::Path;
use std::sync::LazyLock;

use jsonschema::error::ValidationErrorKind;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::detect::addresses::is_solana_address;
use crate::detect::types::{CalloutKind, Confidence, FollowOp, FomoOp, ProfileField, SignalSource};
use crate::protocol::PostKind;
use crate::rules::modes::keyword_pattern;

/// The schema both bots validate against, compiled into the binary from `shared/`.
pub const SCHEMA_TEXT: &str = include_str!("../../../shared/config.schema.json");

static SCHEMA: LazyLock<Value> = LazyLock::new(|| serde_json::from_str(SCHEMA_TEXT).expect("shared/config.schema.json is valid JSON"));
static VALIDATOR: LazyLock<jsonschema::Validator> =
    LazyLock::new(|| jsonschema::draft202012::new(&SCHEMA).expect("shared/config.schema.json is a valid 2020-12 schema"));

/// What makes a rule fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// A watched account posts a contract address.
    CaScanner,
    /// A watched account retweets, quotes, or replies to a target.
    Engagement,
    /// A watched account posts or changes its bio.
    NewTweet,
    /// A watched account follows or unfollows someone.
    Follow,
    /// A watched account changes a profile field.
    PfpUpdate,
    /// A watched pump.fun caller calls a token.
    Callout,
    /// A FOMO thesis or token verification.
    Fomo,
}

/// Whole-word keywords searched up to a reference depth.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeywordFilter {
    /// The words, matched without case.
    pub words: Vec<String>,
    /// 0 reads only the account's own text; each level adds one referenced post.
    pub scan_depth: usize,
    /// One compiled pattern per word, filled in after loading.
    #[serde(skip)]
    pub patterns: Vec<Regex>,
}

/// What fires a new_tweet rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NewTweetEvent {
    /// An original post.
    Post,
    /// A repost.
    Retweet,
    /// A quote.
    Quote,
    /// A reply.
    Reply,
    /// A bio change.
    Bio,
    /// A post kind the protocol may add later; no rule lists it.
    #[serde(skip)]
    Other,
}

impl From<PostKind> for NewTweetEvent {
    fn from(kind: PostKind) -> Self {
        match kind {
            PostKind::Post => NewTweetEvent::Post,
            PostKind::Retweet => NewTweetEvent::Retweet,
            PostKind::Quote => NewTweetEvent::Quote,
            PostKind::Reply => NewTweetEvent::Reply,
            PostKind::Other => NewTweetEvent::Other,
        }
    }
}

/// The token a rule trades.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Token {
    /// The mint, or empty to trade the address in the event.
    pub mint: String,
    /// SOL per buy.
    pub amount_sol: f64,
}

/// Reject or allow when data is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnMissing {
    /// Unknown counts as failing.
    Reject,
    /// Unknown counts as passing.
    Allow,
}

/// Per-rule filters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Filters {
    /// Trades this rule may make; 0 means no limit.
    pub max_snipes: u64,
    /// Seconds between fires.
    pub cooldown_sec: f64,
    /// Youngest token age.
    pub min_token_age_sec: Option<f64>,
    /// Oldest token age.
    pub max_token_age_sec: Option<f64>,
    /// Smallest market cap.
    pub min_market_cap_usd: Option<f64>,
    /// Largest market cap.
    pub max_market_cap_usd: Option<f64>,
    /// Least liquidity.
    pub min_liquidity_usd: Option<f64>,
    /// Highest transfer fee in percent.
    pub max_pool_tax_pct: Option<f64>,
    /// Skip tokens the safety service flags.
    pub skip_suspicious_pools: bool,
    /// What missing data means.
    pub on_missing_data: OnMissing,
}

/// A fee in SOL or `"auto"`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FeeSetting {
    /// A fixed amount in SOL.
    Sol(f64),
    /// The cached recent level.
    Auto(AutoTag),
}

/// The literal `"auto"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoTag {
    /// `"auto"`.
    Auto,
}

/// Per-rule fees.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fees {
    /// Slippage in percent.
    pub slippage_pct: f64,
    /// Priority fee for the whole transaction.
    pub priority_fee_sol: FeeSetting,
    /// Jito tip.
    pub tip_sol: FeeSetting,
    /// Send only through Jito, with a tip.
    pub anti_mev: bool,
}

/// One rule. After defaults every rule carries every field; each mode reads its own.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    /// Unique name.
    pub name: String,
    /// Whether the rule runs.
    pub enabled: bool,
    /// What makes it fire.
    pub mode: Mode,
    /// Watched X accounts.
    pub accounts: Vec<String>,
    /// ca_scanner: allowed signal sources.
    pub sources: Vec<SignalSource>,
    /// ca_scanner: lowest confidence to act on.
    pub min_confidence: Confidence,
    /// ca_scanner: also require the mint in the last known bio.
    pub require_mint_in_bio: bool,
    /// engagement: the reacting account.
    #[serde(default)]
    pub reactor: String,
    /// engagement: the target post.
    pub target_tweet_id: String,
    /// engagement: target accounts.
    pub reacted_to: Vec<String>,
    /// engagement: which reactions count.
    pub actions: Vec<PostKind>,
    /// Fire only with one of these words.
    pub include_keywords: KeywordFilter,
    /// Never fire with one of these words.
    pub exclude_keywords: KeywordFilter,
    /// new_tweet: which actions fire.
    pub events: Vec<NewTweetEvent>,
    /// follow: follows, unfollows, or both.
    pub follow_events: Vec<FollowOp>,
    /// follow: only these followed accounts.
    pub targets: Vec<String>,
    /// pfp_update: watched profile fields.
    pub fields: Vec<ProfileField>,
    /// callout: pump.fun usernames.
    pub callers: Vec<String>,
    /// callout: which callout kinds fire.
    pub callout_kinds: Vec<CalloutKind>,
    /// callout: skip past this multiple.
    pub max_multiple: Option<f64>,
    /// callout: require the caller to hold.
    pub require_caller_position: bool,
    /// fomo: watched usernames or author ids.
    pub fomo_users: Vec<String>,
    /// fomo: watched coin addresses.
    pub fomo_coins: Vec<String>,
    /// fomo: theses, verifications, or both.
    pub fomo_events: Vec<FomoOp>,
    /// Trade the first address in the event when `token.mint` is empty.
    pub use_detected_ca: bool,
    /// The token.
    pub token: Token,
    /// Filters.
    pub filters: Filters,
    /// Fees.
    pub fees: Fees,
}

/// What `safety.rugcheck` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rugcheck {
    /// Not called.
    Off,
    /// Reject only on a danger result that arrives in time.
    Advisory,
    /// No answer counts as unknown.
    Required,
}

/// Safety checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Safety {
    /// Time limit for all checks together.
    pub budget_ms: u64,
    /// What a failed or timed-out check means.
    pub on_unknown: OnMissing,
    /// Rugcheck mode.
    pub rugcheck: Rugcheck,
    /// Highest normalized risk score.
    pub max_rugcheck_score: Option<f64>,
}

/// Spend caps in SOL.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Spend {
    /// Largest single buy.
    pub max_per_trade_sol: f64,
    /// Most in any rolling hour.
    pub max_per_hour_sol: f64,
    /// Most ever.
    pub max_total_sol: f64,
}

/// Exit settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Exits {
    /// Take profit in percent.
    pub take_profit_pct: Option<f64>,
    /// Stop loss in percent.
    pub stop_loss_pct: Option<f64>,
    /// Longest hold in seconds.
    pub max_hold_sec: Option<f64>,
    /// Pricing interval.
    pub poll_ms: u64,
    /// Sell when the buy's post is deleted.
    pub sell_on_delete: bool,
    /// Exit slippage in percent.
    pub sell_slippage_pct: f64,
}

/// Execution settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Execution {
    /// Compute unit limit.
    pub compute_unit_limit: u32,
    /// Ceiling for an automatic priority fee.
    pub max_auto_priority_fee_sol: f64,
    /// Ceiling for an automatic tip.
    pub max_auto_tip_sol: f64,
    /// Blockhash refresh interval.
    pub blockhash_refresh_ms: u64,
    /// Rebroadcast interval.
    pub rebroadcast_ms: u64,
    /// Confirmation timeout.
    pub confirm_timeout_ms: u64,
}

/// The whole config after defaults and validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// The rules, in order.
    pub rules: Vec<Rule>,
    /// Oldest post to act on.
    pub max_signal_age_ms: i64,
    /// Safety checks.
    pub safety: Safety,
    /// Spend caps.
    pub spend: Spend,
    /// Exits.
    pub exits: Exits,
    /// Execution.
    pub execution: Execution,
}

/// A config problem, with one line saying what to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// The sentence shown to the user.
    pub message: String,
    /// The rule's name, or `None` for a top-level field.
    pub rule: Option<String>,
    /// The dot-separated path inside that rule (or the config), as `shared/fixtures/config` names it.
    pub field: Option<String>,
}

impl ConfigError {
    fn new(message: String, rule: Option<&str>, field: Option<&str>) -> ConfigError {
        ConfigError { message, rule: rule.map(str::to_string), field: field.map(str::to_string) }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Reads, fills, validates, and checks a config file.
///
/// # Errors
///
/// Returns [`ConfigError`] when the file is missing, is not JSON, or fails a check.
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let Ok(text) = std::fs::read_to_string(path) else {
        let message = format!(
            "Cannot read {}. Copy config.example.json to config.json in the repository root, then edit the handles and amounts.",
            path.display()
        );
        return Err(ConfigError::new(message, None, None));
    };
    let json: Value =
        serde_json::from_str(&text).map_err(|e| ConfigError::new(format!("{} is not valid JSON: {e}", path.display()), None, None))?;
    parse_config(json)
}

/// Fills defaults, validates, and checks a config value.
///
/// # Errors
///
/// Returns [`ConfigError`] with the first problem found.
pub fn parse_config(json: Value) -> Result<Config, ConfigError> {
    let mut config = json;
    with_defaults(&SCHEMA, &mut config);
    let errors: Vec<jsonschema::ValidationError> = VALIDATOR.iter_errors(&config).collect();
    if !errors.is_empty() {
        let first = errors.iter().find(|e| is_additional_property(e.kind())).unwrap_or(&errors[0]);
        return Err(explain(first, &config));
    }
    let config: Config = serde_json::from_value(config).map_err(|e| ConfigError::new(format!("config: {e}"), None, None))?;
    check_semantics(config)
}

fn is_additional_property(kind: &ValidationErrorKind) -> bool {
    if let ValidationErrorKind::AdditionalProperties { .. } = kind {
        return true;
    }
    false
}

fn resolve(node: &Value) -> &Value {
    match node.get("$ref").and_then(Value::as_str) {
        Some(reference) => {
            let name = reference.trim_start_matches("#/$defs/");
            resolve(&SCHEMA["$defs"][name])
        }
        None => node,
    }
}

/// Fills every missing property that has a schema default, and walks into objects and arrays.
/// The schema is the only place defaults are written down.
fn with_defaults(schema_node: &Value, value: &mut Value) {
    let node = resolve(schema_node);
    if let (Value::Array(items), Some(item_schema)) = (&mut *value, node.get("items")) {
        for item in items {
            with_defaults(item_schema, item);
        }
        return;
    }
    let (Value::Object(object), Some(Value::Object(properties))) = (value, node.get("properties")) else {
        return;
    };
    for (key, child) in properties {
        let resolved = resolve(child);
        if !object.contains_key(key) {
            let default = child.get("default").or_else(|| resolved.get("default"));
            match default {
                Some(default) => object.insert(key.clone(), default.clone()),
                None if resolved.get("properties").is_some() => object.insert(key.clone(), Value::Object(Map::new())),
                None => continue,
            };
        }
        if let Some(slot) = object.get_mut(key) {
            with_defaults(child, slot);
        }
    }
}

/// Turns a schema error into a sentence naming the rule and the field.
fn explain(error: &jsonschema::ValidationError, config: &Value) -> ConfigError {
    let path = error.instance_path().as_str().to_string();
    let mut parts: Vec<String> = path.split('/').filter(|p| !p.is_empty()).map(str::to_string).collect();
    let mut place = "config".to_string();
    let mut rule = None;
    if parts.first().is_some_and(|p| p == "rules")
        && let Some(index) = parts.get(1).and_then(|p| p.parse::<usize>().ok())
    {
        let name = config["rules"][index]["name"].as_str().map_or_else(|| format!("#{}", index + 1), str::to_string);
        place = format!("rule \"{name}\"");
        rule = Some(name);
        parts.drain(..2);
    }
    match error.kind() {
        ValidationErrorKind::AdditionalProperties { unexpected } => parts.extend(unexpected.first().cloned()),
        ValidationErrorKind::Required { property } => parts.extend(property.as_str().map(str::to_string)),
        _ => {}
    }
    let field = parts.join(".");
    let message = describe(error, &place, &field);
    ConfigError { message, rule, field: if field.is_empty() { None } else { Some(field) } }
}

fn describe(error: &jsonschema::ValidationError, place: &str, field: &str) -> String {
    match error.kind() {
        ValidationErrorKind::MinLength { .. } if field == "token.mint" => {
            format!(
                "{place}: set token.mint to the Solana mint to trade, or set \"useDetectedCa\": true to trade the address found in the event."
            )
        }
        ValidationErrorKind::Required { .. } if field == "reactor" => {
            format!("{place}: an engagement rule needs \"reactor\": the handle or id whose retweets, quotes, or replies fire it.")
        }
        ValidationErrorKind::AnyOf { .. } if field.is_empty() => {
            format!("{place}: an engagement rule needs targetTweetId or reactedTo; a fomo rule needs fomoUsers or fomoCoins.")
        }
        ValidationErrorKind::MinItems { .. } if ["accounts", "callers", "fomoCoins"].contains(&field) => {
            let note = if field == "fomoCoins" { " (token_verified only fires for watched coins)" } else { "" };
            format!("{place}: {field} needs at least one entry{note}.")
        }
        ValidationErrorKind::AdditionalProperties { .. } => {
            format!("{place}: unknown field \"{field}\". Check the spelling against docs/config.md.")
        }
        ValidationErrorKind::Enum { options } => {
            let mut names = Vec::new();
            for option in options.as_array().into_iter().flatten() {
                names.push(option.to_string());
            }
            format!("{place}: {field} must be one of {}.", names.join(", "))
        }
        ValidationErrorKind::Pattern { .. } if is_account_field(field) => {
            format!("{place}: {field} must be an X handle (letters, digits, _; at most 15) or a numeric account id.")
        }
        ValidationErrorKind::Pattern { .. } if field.ends_with("mint") => {
            format!("{place}: {field} is not a Solana address (base58, 32 to 44 characters).")
        }
        _ => {
            let field = if field.is_empty() { "config" } else { field };
            format!("{place}: {field} {}.", schema_message(error))
        }
    }
}

fn is_account_field(field: &str) -> bool {
    let field = field.to_lowercase();
    ["account", "reactor", "reacted", "targets"].iter().any(|word| field.contains(word))
}

/// The validator's message names the instance first ("[] has less than 1 item"); keep the part
/// after the value so the sentence reads "rules has less than 1 item".
fn schema_message(error: &jsonschema::ValidationError) -> String {
    let message = error.to_string();
    let value = error.instance().to_string();
    message.strip_prefix(&value).map_or(message.clone(), |rest| rest.trim().to_string())
}

static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new("(?i)^@?YOUR_").expect("valid pattern"));

/// Checks the schema cannot express. Disabled rules may keep placeholders.
fn check_semantics(mut config: Config) -> Result<Config, ConfigError> {
    let mut names = std::collections::HashSet::new();
    for rule in &mut config.rules {
        let name = rule.name.as_str();
        if !names.insert(rule.name.clone()) {
            return Err(ConfigError::new(format!("rule names must be unique: \"{name}\" appears twice."), Some(name), Some("name")));
        }
        for filter in [&mut rule.include_keywords, &mut rule.exclude_keywords] {
            filter.patterns = filter.words.iter().map(|w| keyword_pattern(w)).collect();
        }
        if !rule.enabled {
            continue;
        }
        let place = format!("rule \"{name}\"");
        let mut mints = vec![("token.mint".to_string(), &rule.token.mint)];
        for (i, coin) in rule.fomo_coins.iter().enumerate() {
            mints.push((format!("fomoCoins.{i}"), coin));
        }
        for (field, mint) in mints {
            if !mint.is_empty() && !is_solana_address(mint) {
                let message = format!("{place}: \"{mint}\" does not decode to a 32-byte Solana address.");
                return Err(ConfigError::new(message, Some(name), Some(&field)));
            }
        }
        let mut specs = vec![("reactor".to_string(), &rule.reactor)];
        let lists = [
            ("accounts", &rule.accounts),
            ("reactedTo", &rule.reacted_to),
            ("targets", &rule.targets),
            ("callers", &rule.callers),
            ("fomoUsers", &rule.fomo_users),
        ];
        for (list_name, list) in lists {
            for (i, spec) in list.iter().enumerate() {
                specs.push((format!("{list_name}.{i}"), spec));
            }
        }
        for (field, spec) in specs {
            if PLACEHOLDER.is_match(spec) {
                let message = format!("{place}: replace the placeholder \"{spec}\" with the real account.");
                return Err(ConfigError::new(message, Some(name), Some(&field)));
            }
        }
    }
    if !config.rules.iter().any(|r| r.enabled) {
        return Err(ConfigError::new("every rule has \"enabled\": false. Turn one on.".to_string(), None, Some("rules")));
    }
    Ok(config)
}

/// Returns one warning per enabled rule that watches an account by handle. Handles can change
/// owner; numeric ids cannot.
pub fn handle_warnings(config: &Config) -> Vec<String> {
    let mut out = Vec::new();
    for rule in config.rules.iter().filter(|r| r.enabled) {
        let specs = match rule.mode {
            Mode::Engagement => std::slice::from_ref(&rule.reactor),
            Mode::CaScanner | Mode::NewTweet | Mode::Follow | Mode::PfpUpdate | Mode::Callout | Mode::Fomo => &rule.accounts[..],
        };
        for spec in specs {
            if spec.is_empty() || !spec.bytes().all(|b| b.is_ascii_digit()) {
                out.push(format!(
                    "rule \"{}\" watches {spec} by handle. A handle can be renamed or taken over; the numeric account id cannot.",
                    rule.name
                ));
            }
        }
    }
    out
}
