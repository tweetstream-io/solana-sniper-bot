//! Reads secrets and endpoints from the environment and the repository's `.env`. Every problem gets
//! a message that says exactly what to change. Next: `setup/config.rs` loads the strategy file.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// Settings read from the environment.
#[derive(Debug, Clone)]
pub struct Env {
    /// Standard TweetStream key.
    pub api_key: Option<String>,
    /// Ultra Speed key.
    pub ultra_api_key: Option<String>,
    /// TweetStream WebSocket URL.
    pub ws_url: String,
    /// Path of `config.json`.
    pub config_path: PathBuf,
    /// Directory for durable state.
    pub state_dir: PathBuf,
    /// Dry run (the default) sends nothing.
    pub dry_run: bool,
    /// Solana RPC URLs; the first serves reads.
    pub rpc_urls: Vec<String>,
    /// Solana PubSub WebSocket for confirmations.
    pub rpc_ws_url: String,
    /// Base58 wallet secret key.
    pub wallet_secret: Option<String>,
    /// Jupiter API key.
    pub jupiter_api_key: Option<String>,
    /// Jito block engine, or `None` when turned off.
    pub jito_url: Option<String>,
    file: HashMap<String, String>,
}

/// A setup problem, with one line saying what to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupError(pub String);

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SetupError {}

/// The US endpoint. Run the bot in Ashburn, Virginia and keep it next to this endpoint.
const DEFAULT_WS_URL: &str = "wss://ws-iad.tweetstream.io/ws";
const DEFAULT_RPC: &str = "https://api.mainnet-beta.solana.com";
const DEFAULT_JITO: &str = "https://ny.mainnet.block-engine.jito.wtf";

/// Returns the repository root, where `.env`, `config.json`, and `state/` live for both bots.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

impl Env {
    /// Returns a variable from the process environment, else from `.env`, trimmed; empty is unset.
    pub fn var(&self, name: &str) -> Option<String> {
        let value = std::env::var(name).ok().or_else(|| self.file.get(name).cloned())?;
        let value = value.trim();
        if value.is_empty() { None } else { Some(value.to_string()) }
    }
}

/// Reads `.env` in the repository root (values already in the environment win) and the variables
/// the bot uses.
///
/// # Errors
///
/// Returns [`SetupError`] when `.env` cannot be parsed or a value is malformed.
pub fn read_env() -> Result<Env, SetupError> {
    let root = repo_root();
    let env_file = root.join(".env");
    let mut file = HashMap::new();
    if env_file.exists() {
        let entries = dotenvy::from_path_iter(&env_file).map_err(|e| SetupError(format!("{}: {e}", env_file.display())))?;
        for entry in entries {
            let (key, value) = entry.map_err(|e| SetupError(format!("{}: {e}", env_file.display())))?;
            file.insert(key, value);
        }
    }
    let mut env = Env {
        api_key: None,
        ultra_api_key: None,
        ws_url: String::new(),
        config_path: PathBuf::new(),
        state_dir: PathBuf::new(),
        dry_run: true,
        rpc_urls: Vec::new(),
        rpc_ws_url: String::new(),
        wallet_secret: None,
        jupiter_api_key: None,
        jito_url: None,
        file,
    };
    let dry_run = env.var("DRY_RUN").unwrap_or_else(|| "true".to_string()).to_lowercase();
    if dry_run != "true" && dry_run != "false" {
        return Err(SetupError(format!("DRY_RUN must be \"true\" or \"false\", got \"{dry_run}\".")));
    }
    let mut rpc_urls = Vec::new();
    for url in env.var("SOLANA_RPC_URLS").unwrap_or_else(|| DEFAULT_RPC.to_string()).split(',') {
        let url = url.trim();
        if url.is_empty() {
            continue;
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(SetupError(format!("SOLANA_RPC_URLS: \"{url}\" must start with https://")));
        }
        rpc_urls.push(url.to_string());
    }
    let Some(first_rpc) = rpc_urls.first().cloned() else {
        return Err(SetupError("SOLANA_RPC_URLS is empty. Put at least one https:// RPC URL in it.".to_string()));
    };
    env.rpc_ws_url = env.var("SOLANA_WS_URL").unwrap_or_else(|| format!("ws{}", first_rpc.strip_prefix("http").unwrap_or(&first_rpc)));
    env.jito_url = match env.var("JITO_URL") {
        Some(url) if url == "off" => None,
        Some(url) => Some(url),
        None => Some(DEFAULT_JITO.to_string()),
    };
    env.api_key = env.var("TWEETSTREAM_API_KEY");
    env.ultra_api_key = env.var("TWEETSTREAM_ULTRA_API_KEY");
    env.ws_url = env.var("TWEETSTREAM_WS_URL").unwrap_or_else(|| DEFAULT_WS_URL.to_string());
    env.config_path = root.join(env.var("SNIPER_CONFIG").unwrap_or_else(|| "config.json".to_string()));
    env.state_dir = root.join(env.var("STATE_DIR").unwrap_or_else(|| "state".to_string()));
    env.dry_run = dry_run != "false";
    env.rpc_urls = rpc_urls;
    env.wallet_secret = env.var("WALLET_SECRET_KEY");
    env.jupiter_api_key = env.var("JUPITER_API_KEY");
    Ok(env)
}

const UTM: &str = "utm_source=github&utm_medium=referral&utm_campaign=solana-sniper-bot";

/// Returns a tweetstream.io link with the campaign and a placement name, as every link the bot
/// prints carries.
pub fn product_link(path: &str, placement: &str) -> String {
    let joiner = if path.contains('?') { '&' } else { '?' };
    format!("https://tweetstream.io{path}{joiner}{UTM}&utm_content={placement}")
}

/// Returns the trial signup link tagged with where it was shown.
pub fn trial_link(placement: &str) -> String {
    product_link("/signup?offer=basic-monthly-trial", placement)
}

/// Returns the Ultra Speed trial link tagged with where it was shown.
pub fn ultra_trial_link(placement: &str) -> String {
    product_link("/signup?intent=ultra_speed&ultraBilling=MONTHLY", placement)
}

/// Returns the help printed when no API key is set.
pub fn setup_help() -> String {
    format!(
        "
No TweetStream API key found. The bot needs one to read the live feed.

  1. Start a trial: {}
  2. Copy your key from {}
  3. In the repository root: cp .env.example .env, then paste the key into TWEETSTREAM_API_KEY
  4. In the repository root: cp config.example.json config.json, then put your handles and amounts in it
  5. cargo run --release

The bot starts in dry run: it prints what it would buy and sends nothing.
To see it work first without a key, run: cargo run --release -- --demo
",
        trial_link("rust-cli-missing-key"),
        product_link("/dashboard?tab=api", "rust-cli-missing-key")
    )
}
