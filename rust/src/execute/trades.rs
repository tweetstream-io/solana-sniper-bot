//! The trade state machine and the only owner of durable bot state: snipes used, SOL spent, buys
//! made, open positions, and every trade in flight. Format: `shared/ledger.schema.json`. Rules:
//! `shared/README.md`, section Trade state. `execute/trade.rs` drives it; nothing else writes state.
//!
//! ```text
//! reserved --> sending --> confirmed
//!    |            |----> failed        (the chain proved it failed)
//!    |            '----> unknown --> reconciled (landed or expired, found later)
//!    '--> failed (nothing was sent)
//! ```
//!
//! Every method changes memory under one short lock and never waits on the disk inside it. A change
//! that must be durable is serialized under the lock and written after it, so the rule engine can
//! take a snipe while another task is still flushing a trade to disk.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::detect::types::Actor;
use crate::log::log;
use crate::setup::config::Spend;

const SCHEMA_TEXT: &str = include_str!("../../../shared/ledger.schema.json");
static VALIDATOR: LazyLock<jsonschema::Validator> = LazyLock::new(|| {
    let schema: Value = serde_json::from_str(SCHEMA_TEXT).expect("shared/ledger.schema.json is valid JSON");
    jsonschema::draft202012::new(&schema).expect("shared/ledger.schema.json is a valid schema")
});

const HOUR_MS: i64 = 3_600_000;
const KEEP_RESOLVED: usize = 1_000;

/// Lamports as the state file writes them: a decimal string.
mod decimal {
    use super::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &i128, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i128, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
    }

    pub mod option {
        use super::super::{Deserialize, Deserializer, Serializer};

        pub fn serialize<S: Serializer>(value: &Option<i128>, serializer: S) -> Result<S::Ok, S::Error> {
            match value {
                Some(value) => serializer.serialize_str(&value.to_string()),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<i128>, D::Error> {
            let Some(text) = Option::<String>::deserialize(deserializer)? else {
                return Ok(None);
            };
            text.parse().map(Some).map_err(serde::de::Error::custom)
        }
    }
}

/// Which state file this is; the two never mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// `state/live.json`.
    Live,
    /// `state/dry-run.json`.
    DryRun,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Live => "live",
            Mode::DryRun => "dry_run",
        }
    }
}

/// Where a trade is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeState {
    /// Limits checked, nothing sent.
    Reserved,
    /// Written to disk, then sent.
    Sending,
    /// Landed.
    Confirmed,
    /// Nothing sent, or landed with an error, or refused.
    Failed,
    /// Sent, outcome not known.
    Unknown,
    /// Resolved later by the reconciler.
    Reconciled,
}

impl TradeState {
    /// Returns true while the trade still holds its reservation.
    pub fn is_active(self) -> bool {
        match self {
            TradeState::Reserved | TradeState::Sending | TradeState::Unknown => true,
            TradeState::Confirmed | TradeState::Failed | TradeState::Reconciled => false,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            TradeState::Reserved => "reserved",
            TradeState::Sending => "sending",
            TradeState::Confirmed => "confirmed",
            TradeState::Failed => "failed",
            TradeState::Unknown => "unknown",
            TradeState::Reconciled => "reconciled",
        }
    }
}

/// Buy or sell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// Spend SOL for the token.
    Buy,
    /// Sell the whole position.
    Sell,
}

/// How a trade was built and sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeRoute {
    /// Built locally on the pump.fun curve.
    Pump,
    /// Built by PumpPortal.
    Pumpportal,
    /// Built and landed by Jupiter.
    Jupiter,
    /// Not sent: dry run.
    DryRun,
}

impl TradeRoute {
    /// Returns the name used in logs.
    pub fn as_str(self) -> &'static str {
        match self {
            TradeRoute::Pump => "pump",
            TradeRoute::Pumpportal => "pumpportal",
            TradeRoute::Jupiter => "jupiter",
            TradeRoute::DryRun => "dry_run",
        }
    }
}

/// How an open position is priced and sold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionRoute {
    /// On the pump.fun curve.
    Pump,
    /// Through Jupiter.
    Jupiter,
}

/// Why a trade exists: the rule that fired, the event, and the account that triggered it. Kept on
/// the position so a delete of that post, or that caller or author leaving, can find it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Trigger {
    /// Rule name.
    pub rule: String,
    /// The fire-once key of the fire.
    pub fire_key: String,
    /// The event it came from.
    pub event_ref: String,
    /// The post it came from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_key: Option<String>,
    /// The account whose event fired the rule.
    pub actor: Actor,
}

/// An exit asked for while the buy was still pending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExitIntent {
    /// Exit code.
    pub code: String,
    /// When it was asked for.
    pub at: i64,
    /// Free text.
    pub detail: String,
}

/// One buy or sell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Trade {
    /// Unique id.
    pub id: String,
    /// Buy or sell.
    pub side: Side,
    /// The token.
    pub mint: String,
    /// How it was built and sent.
    pub route: TradeRoute,
    /// Where it is in its life.
    pub state: TradeState,
    /// When it was reserved.
    pub created_at: i64,
    /// Why it exists.
    pub trigger: Trigger,
    /// Largest possible wallet debit, held against the spend caps (0 for sells).
    #[serde(with = "decimal")]
    pub reserved_lamports: i128,
    /// What a transaction that lands and fails can still cost (fees, tips, rent).
    #[serde(with = "decimal")]
    pub fee_budget_lamports: i128,
    /// Actual debit, once known.
    #[serde(default, with = "decimal::option", skip_serializing_if = "Option::is_none")]
    pub spent_lamports: Option<i128>,
    /// Base58 signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Last block height the transaction can land at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_valid_block_height: Option<u64>,
    /// Signed bytes, base64, kept until resolved so a restart can rebroadcast.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx: Option<String>,
    /// Where it was sent; a rebroadcast uses the same senders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub senders: Option<Vec<String>>,
    /// When it was sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<i64>,
    /// Whether a transaction landed, recorded when the trade resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed: Option<bool>,
    /// Why it failed or is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Tokens received, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_amount: Option<String>,
    /// Exits asked for while it was pending.
    pub exit_intents: Vec<ExitIntent>,
}

/// An open position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Position {
    /// The token.
    pub mint: String,
    /// How it is priced and sold.
    pub route: PositionRoute,
    /// The mint's token program, base58.
    pub token_program: String,
    /// When it opened.
    pub opened_at: i64,
    /// The buy that opened it.
    pub buy_trade_id: String,
    /// What it cost, in lamports.
    #[serde(with = "decimal")]
    pub cost_lamports: i128,
    /// Raw token units; `None` until a balance read returns more than 0.
    pub token_amount: Option<String>,
    /// Why it was bought.
    pub trigger: Trigger,
    /// The sell in progress, if any. One at a time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sell_trade_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpendEntry {
    trade_id: String,
    at: i64,
    #[serde(with = "decimal")]
    lamports: i128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpendState {
    #[serde(with = "decimal")]
    total_lamports: i128,
    recent: Vec<SpendEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Purchase {
    first_at: i64,
    trade_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u8,
    mode: Mode,
    snipes: BTreeMap<String, u64>,
    spend: SpendState,
    purchases: BTreeMap<String, Purchase>,
    trades: BTreeMap<String, Trade>,
    positions: BTreeMap<String, Position>,
}

/// How a sent trade ended, as the sender, Jupiter, or the reconciler learned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Landed. A Jupiter signature replaces the bot's own.
    Landed {
        /// Tokens received, when known.
        token_amount: Option<u64>,
        /// Actual debit, when known.
        spent_lamports: Option<i128>,
        /// The signature that landed, when it differs.
        signature: Option<String>,
    },
    /// Landed with an error.
    Failed {
        /// The chain's error.
        error: String,
        /// Actual debit, when known.
        spent_lamports: Option<i128>,
    },
    /// Never landed and its blockhash expired.
    Expired,
    /// Refused before it could land (Jupiter `/execute` said so).
    Rejected {
        /// Why.
        error: String,
    },
    /// No answer; it may still land.
    Unknown {
        /// Why the outcome is not known.
        error: String,
    },
}

impl Outcome {
    /// Returns the outcome's name for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Outcome::Landed { .. } => "landed",
            Outcome::Failed { .. } => "failed",
            Outcome::Expired => "expired",
            Outcome::Rejected { .. } => "rejected",
            Outcome::Unknown { .. } => "unknown",
        }
    }

    /// Returns the error text, when the outcome has one.
    pub fn error(&self) -> Option<&str> {
        match self {
            Outcome::Failed { error, .. } | Outcome::Rejected { error } | Outcome::Unknown { error } => Some(error),
            Outcome::Landed { .. } | Outcome::Expired => None,
        }
    }
}

/// What a sent trade records before it goes on the wire.
#[derive(Debug, Clone)]
pub struct Sent {
    /// Base58 signature.
    pub signature: String,
    /// Last block height it can land at.
    pub last_valid_block_height: u64,
    /// Signed bytes, base64.
    pub tx: String,
    /// How it was built.
    pub route: TradeRoute,
    /// Where it goes.
    pub senders: Vec<String>,
}

/// What a buy reservation needs.
#[derive(Debug, Clone)]
pub struct BuyRequest<'a> {
    /// The token.
    pub mint: &'a str,
    /// Why.
    pub trigger: Trigger,
    /// How it will be built.
    pub route: TradeRoute,
    /// Largest possible wallet debit.
    pub max_debit: i128,
    /// The part of `max_debit` above the amount.
    pub fee_budget: i128,
    /// Spend caps.
    pub caps: &'a Spend,
    /// Refuse a mint bought before.
    pub skip_if_already_bought: bool,
    /// Refuse a mint still held.
    pub skip_if_already_holding: bool,
    /// Wall clock ms.
    pub now: i64,
}

/// Where an exit asked for now applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitTarget {
    /// Kept on the pending buy; applied when it lands.
    Pending,
    /// There is a position to sell now.
    Position,
    /// Nothing to exit.
    None,
}

/// A state file problem that stops startup or refuses a send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError(pub String);

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StateError {}

/// Returns `sol` in lamports, rounded to the nearest lamport.
pub fn sol_to_lamports(sol: f64) -> i128 {
    (sol * 1e9).round() as i128
}

struct Book {
    state: State,
    next_id: u64,
    generation: u64,
}

/// Writes snapshots in generation order: a snapshot older than the last one written is already
/// covered by it, so it is skipped.
struct StateFile {
    path: PathBuf,
    written: Mutex<u64>,
}

impl StateFile {
    /// Temp file, fsync, rename, fsync the directory: after a crash the file is either the old
    /// state or the new one, never half of each.
    fn write(&self, generation: u64, bytes: &[u8]) -> std::io::Result<()> {
        let mut written = self.written.lock().unwrap_or_else(|p| p.into_inner());
        if generation <= *written {
            return Ok(());
        }
        let tmp = self.path.with_extension("json.tmp");
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &self.path)?;
        if let Some(dir) = self.path.parent() {
            File::open(dir)?.sync_all()?;
        }
        *written = generation;
        Ok(())
    }
}

/// The durable trade state, shared by the rule engine and the trader.
pub struct TradeBook {
    book: Mutex<Book>,
    file: Option<StateFile>,
}

type Snapshot = Option<(u64, Vec<u8>)>;

impl TradeBook {
    /// Opens the state file, creating it only when it does not exist. `None` keeps everything in
    /// memory (tests, the demo).
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] when the file cannot be read, is not JSON, does not match
    /// `shared/ledger.schema.json`, or holds the other mode's state: a reset file would let the bot
    /// buy the same mints again.
    pub fn open(path: Option<&Path>, mode: Mode) -> Result<TradeBook, StateError> {
        let empty = State {
            version: 1,
            mode,
            snipes: BTreeMap::new(),
            spend: SpendState { total_lamports: 0, recent: Vec::new() },
            purchases: BTreeMap::new(),
            trades: BTreeMap::new(),
            positions: BTreeMap::new(),
        };
        let Some(path) = path else {
            return Ok(TradeBook::with(empty, None));
        };
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(dir) = path.parent() {
                    fs::create_dir_all(dir).map_err(|e| StateError(format!("cannot create {}: {e}", dir.display())))?;
                }
                let book = TradeBook::with(empty, Some(path));
                book.write(book.snapshot_now()).map_err(|e| StateError(format!("cannot write {}: {e}", path.display())))?;
                return Ok(book);
            }
            Err(error) => return Err(StateError(format!("cannot read {}: {error}", path.display()))),
        };
        let Ok(json) = serde_json::from_str::<Value>(&text) else {
            return Err(StateError(format!(
                "{} is not valid JSON. Fix or move it; deleting it lets the bot buy the same mints again.",
                path.display()
            )));
        };
        if let Some(error) = VALIDATOR.iter_errors(&json).next() {
            let at = error.instance_path().as_str().to_string();
            return Err(StateError(format!(
                "{} does not match shared/ledger.schema.json ({} {error}). Fix or move it.",
                path.display(),
                if at.is_empty() { "/" } else { &at }
            )));
        }
        let state: State = serde_json::from_value(json)
            .map_err(|e| StateError(format!("{} does not match shared/ledger.schema.json ({e}). Fix or move it.", path.display())))?;
        if state.mode != mode {
            return Err(StateError(format!(
                "{} holds {} state, but the bot runs in {} mode.",
                path.display(),
                state.mode.as_str(),
                mode.as_str()
            )));
        }
        Ok(TradeBook::with(state, Some(path)))
    }

    fn with(state: State, path: Option<&Path>) -> TradeBook {
        let file = path.map(|p| StateFile { path: p.to_path_buf(), written: Mutex::new(0) });
        TradeBook { book: Mutex::new(Book { state, next_id: 0, generation: 0 }), file }
    }

    fn lock(&self) -> MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn snapshot(&self, book: &mut Book) -> Snapshot {
        prune(&mut book.state);
        self.file.as_ref()?;
        book.generation += 1;
        let bytes = serde_json::to_vec_pretty(&book.state).expect("state serializes");
        Some((book.generation, bytes))
    }

    fn snapshot_now(&self) -> Snapshot {
        let mut book = self.lock();
        self.snapshot(&mut book)
    }

    fn write(&self, snapshot: Snapshot) -> std::io::Result<()> {
        match (&self.file, snapshot) {
            (Some(file), Some((generation, bytes))) => file.write(generation, &bytes),
            (Some(_), None) | (None, _) => Ok(()),
        }
    }

    /// For changes after the money question is settled: a failed write is retried by the next one.
    fn try_write(&self, snapshot: Snapshot) {
        if let Err(error) = self.write(snapshot) {
            log("state", &format!("state write failed, the next change retries it: {error}"));
        }
    }

    /// Fails every trade still `reserved` after a restart: `sending` is written before anything goes
    /// on the wire, so a reserved trade was never sent. Returns the trades it closed.
    pub fn recover_after_restart(&self) -> Vec<Trade> {
        let mut book = self.lock();
        let mut closed = Vec::new();
        let ids: Vec<String> = book.state.trades.values().filter(|t| t.state == TradeState::Reserved).map(|t| t.id.clone()).collect();
        for id in ids {
            fail_reserved(&mut book.state, &id, "the bot stopped before sending");
            closed.push(book.state.trades[&id].clone());
        }
        let snapshot = self.snapshot(&mut book);
        drop(book);
        self.try_write(snapshot);
        closed
    }

    /// Takes one snipe for the rule, or returns false when `max_snipes` (0 = no limit) is used up.
    pub fn reserve_snipe(&self, rule: &str, max_snipes: u64) -> bool {
        let mut book = self.lock();
        let used = book.state.snipes.entry(rule.to_string()).or_default();
        if max_snipes > 0 && *used >= max_snipes {
            return false;
        }
        *used += 1;
        true
    }

    /// Gives a snipe back.
    pub fn release_snipe(&self, rule: &str) {
        let mut book = self.lock();
        let used = book.state.snipes.entry(rule.to_string()).or_default();
        *used = used.saturating_sub(1);
    }

    /// Reserves a buy. Refused when another buy of the mint is in flight, the mint was bought
    /// before (and the rule says skip), the bot still holds it, or the spend caps would be passed.
    ///
    /// # Errors
    ///
    /// Returns why the buy was refused.
    pub fn reserve_buy(&self, request: BuyRequest) -> Result<Trade, String> {
        let BuyRequest { mint, trigger, route, max_debit, fee_budget, caps, skip_if_already_bought, skip_if_already_holding, now } =
            request;
        let mut book = self.lock();
        if active_trade(&book.state, mint, Side::Buy).is_some() {
            return Err("another buy of this mint is in flight".to_string());
        }
        if skip_if_already_bought && let Some(purchase) = book.state.purchases.get(mint) {
            return Err(format!("already bought (trade {})", purchase.trade_id));
        }
        if skip_if_already_holding && book.state.positions.contains_key(mint) {
            return Err("the bot still holds this token".to_string());
        }
        check_spend(&mut book.state, max_debit, caps, now)?;
        let trade = new_trade(&mut book, Side::Buy, mint, route, trigger, max_debit, fee_budget, now);
        book.state.spend.recent.push(SpendEntry { trade_id: trade.id.clone(), at: now, lamports: max_debit });
        book.state.spend.total_lamports += max_debit;
        Ok(trade)
    }

    /// Reserves a sell of the whole position. One sell at a time: a sell that is sending or unknown
    /// must resolve first; the reconciler rebroadcasts it meanwhile.
    ///
    /// # Errors
    ///
    /// Returns why the sell was refused (`"no position"` when there is nothing to sell).
    pub fn reserve_sell(&self, mint: &str, trigger: Trigger, route: TradeRoute, now: i64) -> Result<Trade, String> {
        let mut book = self.lock();
        let Some(position) = book.state.positions.get(mint) else {
            return Err("no position".to_string());
        };
        if let Some(pending) = position.sell_trade_id.as_ref().and_then(|id| book.state.trades.get(id)).filter(|t| t.state.is_active()) {
            return Err(format!("sell {} is still {}", pending.id, pending.state.as_str()));
        }
        let trade = new_trade(&mut book, Side::Sell, mint, route, trigger, 0, 0, now);
        if let Some(position) = book.state.positions.get_mut(mint) {
            position.sell_trade_id = Some(trade.id.clone());
        }
        Ok(trade)
    }

    /// Records `sending` (signature, validity window, signed bytes) and writes it to disk, with
    /// fsync, before the transaction goes on the wire.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] when the trade is not `reserved` or the write failed. The trade is
    /// then still `reserved`: the caller must not send, and must call
    /// [`TradeBook::fail_before_send`].
    pub fn mark_sending(&self, id: &str, sent: Sent, now: i64) -> Result<(), StateError> {
        let Sent { signature, last_valid_block_height, tx, route, senders } = sent;
        let (before, snapshot) = {
            let mut book = self.lock();
            let trade = expect(&mut book.state, id, &[TradeState::Reserved])?;
            let before = trade.clone();
            trade.state = TradeState::Sending;
            trade.sent_at = Some(now);
            trade.signature = Some(signature);
            trade.last_valid_block_height = Some(last_valid_block_height);
            trade.tx = Some(tx);
            trade.route = route;
            trade.senders = Some(senders);
            (before, self.snapshot(&mut book))
        };
        if let Err(error) = self.write(snapshot) {
            self.lock().state.trades.insert(id.to_string(), before);
            return Err(StateError(format!("state write failed: {error}")));
        }
        Ok(())
    }

    /// Fails a trade that sent nothing: the filters said no, a build failed, or the pre-send write
    /// failed. Everything reserved for it comes back.
    pub fn fail_before_send(&self, id: &str, reason: &str) {
        let snapshot = {
            let mut book = self.lock();
            if book.state.trades.get(id).is_none_or(|t| t.state != TradeState::Reserved) {
                return;
            }
            fail_reserved(&mut book.state, id, reason);
            self.snapshot(&mut book)
        };
        self.try_write(snapshot);
    }

    /// Records the result of a sent trade and returns the exit intents to act on now (a buy that
    /// landed while exits were asked for it). `Unknown` holds every reservation until the
    /// reconciler finds out.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] when the trade is not `sending` or `unknown`.
    pub fn resolve(&self, id: &str, outcome: Outcome, now: i64) -> Result<Vec<ExitIntent>, StateError> {
        let (intents, snapshot) = {
            let mut book = self.lock();
            let intents = resolve_trade(&mut book.state, id, outcome, now)?;
            (intents, self.snapshot(&mut book))
        };
        self.try_write(snapshot);
        Ok(intents)
    }

    /// Dry run: a trade goes straight from reserved to confirmed, with its reservation as its cost.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] when the trade is not a reserved dry-run trade.
    pub fn confirm_dry_run(&self, id: &str, now: i64) -> Result<Vec<ExitIntent>, StateError> {
        let (intents, snapshot) = {
            let mut book = self.lock();
            let trade = expect(&mut book.state, id, &[TradeState::Reserved])?;
            if trade.route != TradeRoute::DryRun {
                return Err(StateError(format!("trade {id} is live; only dry-run trades confirm without sending")));
            }
            trade.state = TradeState::Confirmed;
            trade.landed = Some(true);
            trade.spent_lamports = Some(trade.reserved_lamports);
            let (side, mint, intents) = (trade.side, trade.mint.clone(), trade.exit_intents.clone());
            match side {
                Side::Buy => {
                    book.state.purchases.entry(mint).or_insert(Purchase { first_at: now, trade_id: id.to_string() });
                }
                Side::Sell => {
                    book.state.positions.remove(&mint);
                }
            }
            (intents, self.snapshot(&mut book))
        };
        self.try_write(snapshot);
        Ok(intents)
    }

    /// Opens a position after a dry run or a landed buy. `token_amount` `None` means not read yet.
    pub fn open_position(&self, position: Position) {
        let snapshot = {
            let mut book = self.lock();
            book.state.positions.insert(position.mint.clone(), position);
            self.snapshot(&mut book)
        };
        self.try_write(snapshot);
    }

    /// Records a token balance read for a position.
    pub fn set_token_amount(&self, mint: &str, amount: u64) {
        let snapshot = {
            let mut book = self.lock();
            let Some(position) = book.state.positions.get_mut(mint) else {
                return;
            };
            position.token_amount = Some(amount.to_string());
            self.snapshot(&mut book)
        };
        self.try_write(snapshot);
    }

    /// Records an exit for a mint. Asked for while the buy is still pending, it is kept on the buy
    /// and applied when the buy lands.
    pub fn add_exit_intent(&self, mint: &str, code: &str, detail: &str, now: i64) -> ExitTarget {
        let snapshot = {
            let mut book = self.lock();
            let Some(id) = active_trade(&book.state, mint, Side::Buy).map(|t| t.id.clone()) else {
                return if book.state.positions.contains_key(mint) { ExitTarget::Position } else { ExitTarget::None };
            };
            if let Some(trade) = book.state.trades.get_mut(&id) {
                trade.exit_intents.push(ExitIntent { code: code.to_string(), at: now, detail: detail.to_string() });
            }
            self.snapshot(&mut book)
        };
        self.try_write(snapshot);
        ExitTarget::Pending
    }

    /// Returns a trade.
    pub fn trade(&self, id: &str) -> Option<Trade> {
        self.lock().state.trades.get(id).cloned()
    }

    /// Returns the open position in a mint.
    pub fn position(&self, mint: &str) -> Option<Position> {
        self.lock().state.positions.get(mint).cloned()
    }

    /// Returns every open position.
    pub fn positions(&self) -> Vec<Position> {
        self.lock().state.positions.values().cloned().collect()
    }

    /// Returns sent trades whose outcome the reconciler still has to find.
    pub fn unresolved(&self) -> Vec<Trade> {
        let book = self.lock();
        let mut out = Vec::new();
        for trade in book.state.trades.values() {
            if trade.state == TradeState::Sending || trade.state == TradeState::Unknown {
                out.push(trade.clone());
            }
        }
        out
    }

    /// Returns the mints of positions and pending buys bought on a post (delete exits).
    pub fn exit_targets_for_post(&self, post_key: &str) -> Vec<String> {
        self.exit_targets(|trigger| trigger.post_key.as_deref() == Some(post_key))
    }

    /// Returns the mints of positions and pending buys a rule bought because of this account
    /// (caller and author exits).
    pub fn exit_targets_for_actor(&self, rule: &str, actor: &Actor) -> Vec<String> {
        self.exit_targets(|trigger| trigger.rule == rule && trigger.actor.same_account(actor))
    }

    fn exit_targets(&self, matches: impl Fn(&Trigger) -> bool) -> Vec<String> {
        let book = self.lock();
        let mut seen = HashSet::new();
        let mut mints = Vec::new();
        let pending = book.state.trades.values().filter(|t| t.side == Side::Buy && t.state.is_active()).map(|t| (&t.mint, &t.trigger));
        for (mint, trigger) in book.state.positions.values().map(|p| (&p.mint, &p.trigger)).chain(pending) {
            if matches(trigger) && seen.insert(mint.clone()) {
                mints.push(mint.clone());
            }
        }
        mints
    }

    /// Writes the current state, logging a failure.
    pub fn flush(&self) {
        let snapshot = self.snapshot_now();
        self.try_write(snapshot);
    }
}

fn active_trade<'a>(state: &'a State, mint: &str, side: Side) -> Option<&'a Trade> {
    state.trades.values().find(|t| t.side == side && t.mint == mint && t.state.is_active())
}

#[expect(clippy::too_many_arguments, reason = "every field of a new trade")]
fn new_trade(
    book: &mut Book,
    side: Side,
    mint: &str,
    route: TradeRoute,
    trigger: Trigger,
    reserved: i128,
    fee_budget: i128,
    now: i64,
) -> Trade {
    let id = format!("{}-{}", base36(now.max(0) as u64), base36(book.next_id));
    book.next_id += 1;
    let trade = Trade {
        id: id.clone(),
        side,
        mint: mint.to_string(),
        route,
        state: TradeState::Reserved,
        created_at: now,
        trigger,
        reserved_lamports: reserved,
        fee_budget_lamports: fee_budget,
        spent_lamports: None,
        signature: None,
        last_valid_block_height: None,
        tx: None,
        senders: None,
        sent_at: None,
        landed: None,
        error: None,
        token_amount: None,
        exit_intents: Vec::new(),
    };
    book.state.trades.insert(id, trade.clone());
    trade
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("ascii digits")
}

fn expect<'a>(state: &'a mut State, id: &str, states: &[TradeState]) -> Result<&'a mut Trade, StateError> {
    let found = state.trades.get(id).map(|t| t.state);
    match state.trades.get_mut(id) {
        Some(trade) if states.contains(&trade.state) => Ok(trade),
        _ => {
            let wanted: Vec<&str> = states.iter().map(|s| s.as_str()).collect();
            let is = found.map_or("missing", TradeState::as_str);
            Err(StateError(format!("trade {id} is {is}, expected {}", wanted.join(" or "))))
        }
    }
}

fn check_spend(state: &mut State, lamports: i128, caps: &Spend, now: i64) -> Result<(), String> {
    if lamports > sol_to_lamports(caps.max_per_trade_sol) {
        return Err(format!("the largest possible debit is above spend.maxPerTradeSol ({} SOL)", caps.max_per_trade_sol));
    }
    state.spend.recent.retain(|s| now - s.at < HOUR_MS);
    let hour: i128 = state.spend.recent.iter().map(|s| s.lamports).sum();
    if hour + lamports > sol_to_lamports(caps.max_per_hour_sol) {
        return Err(format!("spend.maxPerHourSol ({} SOL) reached", caps.max_per_hour_sol));
    }
    if state.spend.total_lamports + lamports > sol_to_lamports(caps.max_total_sol) {
        return Err(format!("spend.maxTotalSol ({} SOL) reached", caps.max_total_sol));
    }
    Ok(())
}

/// Replaces the reservation with what the trade actually cost. An unknown actual cost keeps it.
fn settle_spend(state: &mut State, id: &str, actual: Option<i128>) {
    let Some(actual) = actual else {
        return;
    };
    let Some(trade) = state.trades.get_mut(id) else {
        return;
    };
    let reserved = trade.spent_lamports.unwrap_or(trade.reserved_lamports);
    trade.spent_lamports = Some(actual);
    state.spend.total_lamports = state.spend.total_lamports - reserved + actual;
    if let Some(entry) = state.spend.recent.iter_mut().find(|s| s.trade_id == id) {
        entry.lamports = actual;
    }
}

fn clear_sell(state: &mut State, mint: &str, id: &str) {
    if let Some(position) = state.positions.get_mut(mint)
        && position.sell_trade_id.as_deref() == Some(id)
    {
        position.sell_trade_id = None;
    }
}

fn fail_reserved(state: &mut State, id: &str, reason: &str) {
    let Some(trade) = state.trades.get_mut(id) else {
        return;
    };
    trade.state = TradeState::Failed;
    trade.landed = Some(false);
    trade.error = Some(reason.to_string());
    let (side, mint) = (trade.side, trade.mint.clone());
    match side {
        Side::Buy => settle_spend(state, id, Some(0)),
        Side::Sell => clear_sell(state, &mint, id),
    }
}

fn resolve_trade(state: &mut State, id: &str, outcome: Outcome, now: i64) -> Result<Vec<ExitIntent>, StateError> {
    let trade = expect(state, id, &[TradeState::Sending, TradeState::Unknown])?;
    let (side, mint, was_unknown, fee_budget) =
        (trade.side, trade.mint.clone(), trade.state == TradeState::Unknown, trade.fee_budget_lamports);
    if let Outcome::Unknown { error } = outcome {
        trade.state = TradeState::Unknown;
        trade.error = Some(error);
        if side == Side::Buy {
            state.purchases.entry(mint).or_insert(Purchase { first_at: now, trade_id: id.to_string() });
        }
        return Ok(Vec::new());
    }
    trade.state = match (&outcome, was_unknown) {
        (_, true) | (Outcome::Expired, false) => TradeState::Reconciled,
        (Outcome::Landed { .. }, false) => TradeState::Confirmed,
        (Outcome::Failed { .. } | Outcome::Rejected { .. } | Outcome::Unknown { .. }, false) => TradeState::Failed,
    };
    trade.tx = None;
    let mut intents = Vec::new();
    match outcome {
        Outcome::Landed { token_amount, spent_lamports, signature } => {
            trade.landed = Some(true);
            if signature.is_some() {
                trade.signature = signature;
            }
            if let Some(amount) = token_amount {
                trade.token_amount = Some(amount.to_string());
            }
            match side {
                Side::Buy => {
                    intents = trade.exit_intents.clone();
                    settle_spend(state, id, spent_lamports);
                    state.purchases.entry(mint).or_insert(Purchase { first_at: now, trade_id: id.to_string() });
                }
                Side::Sell => {
                    state.positions.remove(&mint);
                }
            }
        }
        Outcome::Failed { error, spent_lamports } => {
            trade.landed = Some(true);
            trade.error = Some(error);
            match side {
                Side::Buy => {
                    settle_spend(state, id, Some(spent_lamports.unwrap_or(fee_budget)));
                    forget_purchase(state, &mint, id);
                }
                Side::Sell => clear_sell(state, &mint, id),
            }
        }
        Outcome::Rejected { error } => {
            trade.landed = Some(false);
            trade.error = Some(error);
            match side {
                Side::Buy => {
                    settle_spend(state, id, Some(0));
                    forget_purchase(state, &mint, id);
                }
                Side::Sell => clear_sell(state, &mint, id),
            }
        }
        Outcome::Expired | Outcome::Unknown { .. } => {
            trade.landed = Some(false);
            match side {
                Side::Buy => {
                    settle_spend(state, id, Some(0));
                    forget_purchase(state, &mint, id);
                }
                Side::Sell => clear_sell(state, &mint, id),
            }
        }
    }
    Ok(intents)
}

fn forget_purchase(state: &mut State, mint: &str, id: &str) {
    if state.purchases.get(mint).is_some_and(|p| p.trade_id == id) {
        state.purchases.remove(mint);
    }
}

/// Keeps the newest resolved trades and any a position still points to; active trades stay.
fn prune(state: &mut State) {
    let mut resolved: Vec<(i64, String)> = Vec::new();
    for trade in state.trades.values() {
        if !trade.state.is_active() {
            resolved.push((trade.created_at, trade.id.clone()));
        }
    }
    if resolved.len() <= KEEP_RESOLVED {
        return;
    }
    let mut keep = HashSet::new();
    for position in state.positions.values() {
        keep.insert(position.buy_trade_id.clone());
        keep.extend(position.sell_trade_id.clone());
    }
    resolved.sort();
    let excess = resolved.len() - KEEP_RESOLVED;
    for (_, id) in resolved.into_iter().take(excess) {
        if !keep.contains(&id) {
            state.trades.remove(&id);
        }
    }
}
