//! The bot's durable state, one JSON file per mode (`shared/ledger.schema.json`, rules in
//! `shared/README.md`, section Trade state): the mints it bought and which rule bought them, SOL
//! spent, open positions with their pending exits, and trades sent whose outcome is not known yet.
//! `execute/trade.rs` drives it; nothing else writes state.
//!
//! ```text
//! reserve (memory) --> pending (written, then sent) --> landed: position added, or position sold
//!        |                    |                    '--> failed or expired: removed
//!        '--> cancelled       '--> no answer: stays pending for the reconciler
//! ```
//!
//! Every method changes memory under one short lock and never waits on the disk inside it: the
//! state is cloned under the lock, then serialized and written after it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

use crate::log::{log, unix_ms, wire_name};
use crate::setup::config::Spend;

static SCHEMA: LazyLock<jsonschema::Validator> = LazyLock::new(|| {
    let schema: Value =
        serde_json::from_str(include_str!("../../../shared/ledger.schema.json")).expect("shared/ledger.schema.json is JSON");
    jsonschema::draft202012::new(&schema).expect("shared/ledger.schema.json is a schema")
});

const HOUR_MS: i64 = 3_600_000;

/// Lamports as the state file writes them: a decimal string.
mod decimal {
    use super::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
    }

    pub mod option {
        use super::super::{Deserialize, Deserializer, Serializer};

        pub fn serialize<S: Serializer>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
            match value {
                Some(value) => serializer.serialize_str(&value.to_string()),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
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

/// Buy or sell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// Spend SOL for the token.
    Buy,
    /// Sell the whole position.
    Sell,
}

/// How a trade is built and sent, and how a position is priced and sold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeRoute {
    /// Built locally on the pump.fun bonding curve.
    Pump,
    /// Built and landed by Jupiter.
    Jupiter,
}

/// The event a buy came from; `post_key` lets a delete of that post sell it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Trigger {
    /// The event.
    pub event_ref: String,
    /// The post, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_key: Option<String>,
}

/// Why a position is sold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// The post the buy came from was deleted.
    PostDeleted,
    /// The price reached `exits.takeProfitPct`.
    TakeProfit,
    /// The price reached `exits.stopLossPct`.
    StopLoss,
    /// The position was held for `exits.maxHoldSec`.
    MaxHold,
}

/// An exit waiting for a sell to confirm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingExit {
    /// Why.
    pub code: ExitReason,
    /// When it was asked for.
    pub at: i64,
}

/// An open position, keyed by mint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Position {
    /// The token (the key in the file).
    #[serde(skip)]
    pub mint: String,
    /// The rule that bought it.
    pub rule: String,
    /// The event it was bought on.
    pub trigger: Trigger,
    /// The mint's token program, base58.
    pub token_program: String,
    /// When it opened.
    pub opened_at: i64,
    /// What it cost, in lamports.
    #[serde(with = "decimal")]
    pub cost_lamports: u64,
    /// Raw token units; `None` until a balance read returns more than 0.
    #[serde(with = "decimal::option")]
    pub token_amount: Option<u64>,
    /// Exits waiting for a sell to confirm.
    pub pending_exits: Vec<PendingExit>,
}

/// A trade written before sending whose outcome is not known yet, keyed by trade id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Pending {
    /// The trade id (the key in the file).
    #[serde(skip)]
    pub id: String,
    /// Buy or sell.
    pub side: Side,
    /// The token.
    pub mint: String,
    /// How it was built.
    pub route: TradeRoute,
    /// The transaction's signature; `None` only when Jupiter returned none and the signed
    /// transaction had none of ours, which waits for manual review.
    pub signature: Option<String>,
    /// The transaction's recent blockhash.
    pub blockhash: String,
    /// When known from `getLatestBlockhash` for this blockhash.
    pub last_valid_block_height: Option<u64>,
    /// The largest possible wallet debit, held against the spend caps (0 for sells).
    #[serde(with = "decimal")]
    pub reserved_lamports: u64,
    /// What a buy spends on the token, which becomes the position's cost (0 for sells).
    #[serde(with = "decimal")]
    pub amount_lamports: u64,
    /// The rule that made it.
    pub rule: String,
    /// The event it came from.
    pub trigger: Trigger,
    /// The mint's token program.
    pub token_program: String,
    /// When it was sent.
    pub sent_at: i64,
    /// A buy's exits that arrived while it was pending; the position takes them over. Always
    /// empty for a sell.
    pub pending_exits: Vec<PendingExit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Purchase {
    rule: String,
    at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpendEntry {
    trade_id: String,
    at: i64,
    #[serde(with = "decimal")]
    lamports: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpendState {
    #[serde(with = "decimal")]
    total_lamports: u64,
    recent: Vec<SpendEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u8,
    mode: Mode,
    purchases: BTreeMap<String, Purchase>,
    spend: SpendState,
    positions: BTreeMap<String, Position>,
    pending: BTreeMap<String, Pending>,
}

/// A trade reserved in memory and not sent yet. Nothing about it is on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    /// Trade id.
    pub id: String,
    /// Buy or sell.
    pub side: Side,
    /// The token.
    pub mint: String,
    /// The rule that made it.
    pub rule: String,
    /// The event it came from.
    pub trigger: Trigger,
}

/// What a buy reservation needs.
#[derive(Debug, Clone)]
pub struct BuyRequest<'a> {
    /// The token.
    pub mint: &'a str,
    /// The rule.
    pub rule: &'a str,
    /// The event.
    pub trigger: Trigger,
    /// Wall clock ms.
    pub now: i64,
}

/// What a sent trade records before it goes on the wire.
#[derive(Debug, Clone)]
pub struct Sent {
    /// How it was built.
    pub route: TradeRoute,
    /// The signature to follow; see [`Pending::signature`].
    pub signature: Option<String>,
    /// The transaction's recent blockhash.
    pub blockhash: String,
    /// When known from `getLatestBlockhash` for this blockhash.
    pub last_valid_block_height: Option<u64>,
    /// The mint's token program.
    pub token_program: String,
    /// A buy's hard maximum wallet debit (see `shared/README.md`, section Money bound); 0 for sells.
    pub reserved_lamports: u64,
    /// What a buy spends on the token; 0 for sells.
    pub amount_lamports: u64,
}

/// How a sent trade ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TradeOutcome {
    /// Landed.
    Landed {
        /// Tokens received, when known.
        token_amount: Option<u64>,
        /// Actual wallet debit, when known.
        spent_lamports: Option<u64>,
    },
    /// Landed with an error: it paid its fees.
    Failed {
        /// The chain's error.
        error: String,
        /// Actual wallet debit, when known.
        spent_lamports: Option<u64>,
    },
    /// Never landed: Jupiter refused it, or the signature is unknown and its blockhash is invalid.
    NotLanded {
        /// Why.
        error: String,
    },
    /// No answer; it stays pending.
    Unknown {
        /// Why the outcome is not known.
        error: String,
    },
}

impl TradeOutcome {
    /// Returns the outcome's name for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            TradeOutcome::Landed { .. } => "landed",
            TradeOutcome::Failed { .. } => "failed",
            TradeOutcome::NotLanded { .. } => "not landed",
            TradeOutcome::Unknown { .. } => "unknown",
        }
    }

    /// Returns the error text, when the outcome has one.
    pub fn error(&self) -> Option<&str> {
        match self {
            TradeOutcome::Failed { error, .. } | TradeOutcome::NotLanded { error } | TradeOutcome::Unknown { error } => Some(error),
            TradeOutcome::Landed { .. } => None,
        }
    }
}

/// Where an exit was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitTarget {
    /// On the open position: sell now.
    Position,
    /// On the mint's pending buy: the position takes it over when the buy confirms.
    PendingBuy,
    /// The bot neither holds nor is buying the mint.
    None,
}

/// Why a reservation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveError {
    /// A buy of a mint bought before: one buy per mint per state file.
    AlreadyBought,
    /// Another trade of the mint is in flight.
    Busy(Side),
    /// A sell with nothing to sell.
    NoPosition,
}

impl fmt::Display for ReserveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReserveError::AlreadyBought => f.write_str("already bought"),
            ReserveError::Busy(side) => write!(f, "a {} of this mint is in flight", wire_name(side)),
            ReserveError::NoPosition => f.write_str("no position"),
        }
    }
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
pub fn sol_to_lamports(sol: f64) -> u64 {
    (sol * 1e9).round() as u64
}

struct Book {
    state: State,
    /// Trades reserved and not sent yet.
    tickets: HashMap<String, Ticket>,
    /// Pending trades whose write has not completed; the reconciler does not see them yet.
    unpersisted: HashSet<String>,
    next_id: u64,
    generation: u64,
}

impl Book {
    fn busy(&self, mint: &str) -> Option<Side> {
        let ticket = self.tickets.values().find(|t| t.mint == mint).map(|t| t.side);
        ticket.or_else(|| self.state.pending.values().find(|p| p.mint == mint).map(|p| p.side))
    }
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
    fn write(&self, generation: u64, state: &State) -> std::io::Result<()> {
        let mut written = self.written.lock().unwrap_or_else(|p| p.into_inner());
        if generation <= *written {
            return Ok(());
        }
        let mut bytes = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        let tmp = self.path.with_extension("json.tmp");
        let mut file = File::create(&tmp)?;
        file.write_all(&bytes)?;
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

/// `state/<mode>.lock`, held for the process lifetime: two bots on one state file could buy twice.
struct LockFile {
    path: PathBuf,
}

impl LockFile {
    /// Writes the owner to a temp file and hard-links it to the lock path, so the lock never exists
    /// without its content. An existing lock always refuses the start; it is never taken over.
    fn acquire(path: PathBuf) -> Result<LockFile, StateError> {
        let body = json!({ "pid": std::process::id(), "startedAt": unix_ms() }).to_string();
        let tmp = path.with_extension(format!("lock.{}", std::process::id()));
        let written = fs::write(&tmp, &body).and_then(|()| fs::hard_link(&tmp, &path));
        if let Err(error) = fs::remove_file(&tmp) {
            log("state", &format!("cannot remove {}: {error}", tmp.display()));
        }
        match written {
            Ok(()) => Ok(LockFile { path }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let pid =
                    fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["pid"].as_u64());
                let owner = match pid {
                    Some(pid) if pid_alive(pid) => format!("process {pid}, which is running"),
                    Some(pid) => format!("process {pid}, which is not running"),
                    None => "an unknown process".to_string(),
                };
                Err(StateError(format!(
                    "{} is held by {owner}. Check that no other bot uses this state directory, then delete {}.",
                    path.display(),
                    path.display()
                )))
            }
            Err(error) => Err(StateError(format!("cannot create {}: {error}", path.display()))),
        }
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path) {
            log("state", &format!("cannot remove the lock {}: {error}", self.path.display()));
        }
    }
}

#[cfg(unix)]
fn pid_alive(pid: u64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 sends nothing; it only checks that the process exists.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u64) -> bool {
    true
}

/// The durable trade state, shared by the rule engine and the trader.
pub struct TradeBook {
    book: Mutex<Book>,
    file: Option<StateFile>,
    lock: Mutex<Option<LockFile>>,
}

type Snapshot = Option<(u64, State)>;

impl TradeBook {
    /// Takes `state/<mode>.lock`, then opens the state file, creating it only when it does not
    /// exist. `None` keeps everything in memory (tests, the demo).
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] when another live process holds the lock, or the file cannot be
    /// read, is not JSON, does not match `shared/ledger.schema.json`, or holds the other mode's
    /// state: a reset file would let the bot buy the same mints again.
    pub fn open(path: Option<&Path>, mode: Mode) -> Result<TradeBook, StateError> {
        let empty = State {
            version: 2,
            mode,
            purchases: BTreeMap::new(),
            spend: SpendState { total_lamports: 0, recent: Vec::new() },
            positions: BTreeMap::new(),
            pending: BTreeMap::new(),
        };
        let Some(path) = path else {
            return Ok(TradeBook::with(empty, None, None));
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| StateError(format!("cannot create {}: {e}", dir.display())))?;
        }
        let lock = LockFile::acquire(path.with_extension("lock"))?;
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let book = TradeBook::with(empty, Some(path), Some(lock));
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
        if json["version"] != 2 {
            return Err(StateError(format!(
                "{} was written by an older version of the bot. Move or delete it to start fresh; the bot then forgets which mints it bought.",
                path.display()
            )));
        }
        if let Some(error) = SCHEMA.iter_errors(&json).next() {
            let at = error.instance_path().as_str().to_string();
            let at = if at.is_empty() { "/".to_string() } else { at };
            return Err(StateError(format!("{} does not match shared/ledger.schema.json ({at} {error}). Fix or move it.", path.display())));
        }
        let mut state: State = serde_json::from_value(json)
            .map_err(|e| StateError(format!("{} does not match shared/ledger.schema.json ({e}). Fix or move it.", path.display())))?;
        if state.mode != mode {
            return Err(StateError(format!(
                "{} holds {} state, but the bot runs in {} mode.",
                path.display(),
                wire_name(&state.mode),
                wire_name(&mode)
            )));
        }
        for (mint, position) in &mut state.positions {
            position.mint.clone_from(mint);
        }
        for (id, pending) in &mut state.pending {
            pending.id.clone_from(id);
        }
        Ok(TradeBook::with(state, Some(path), Some(lock)))
    }

    fn with(state: State, path: Option<&Path>, lock: Option<LockFile>) -> TradeBook {
        let book = Book { state, tickets: HashMap::new(), unpersisted: HashSet::new(), next_id: 0, generation: 0 };
        let file = path.map(|p| StateFile { path: p.to_path_buf(), written: Mutex::new(0) });
        TradeBook { book: Mutex::new(book), file, lock: Mutex::new(lock) }
    }

    /// Writes the state and releases the lock file. Call on a clean exit.
    pub fn close(&self) {
        self.flush();
        drop(self.lock.lock().unwrap_or_else(|p| p.into_inner()).take());
    }

    /// Returns how many mints a rule bought, is sending, or is buying, for `maxSnipes`.
    pub fn buys_by_rule(&self, rule: &str) -> u64 {
        let book = self.lock();
        let bought = book.state.purchases.values().filter(|p| p.rule == rule).count();
        let pending = book.state.pending.values().filter(|p| p.side == Side::Buy && p.rule == rule).count();
        let tickets = book.tickets.values().filter(|t| t.side == Side::Buy && t.rule == rule).count();
        (bought + pending + tickets) as u64
    }

    /// Reserves a buy in memory: one buy per mint per state file, so it is refused while the mint
    /// was bought, is pending, or is being bought. The spend caps are checked when it is written.
    ///
    /// # Errors
    ///
    /// Returns why the buy was refused.
    pub fn reserve_buy(&self, request: BuyRequest) -> Result<Ticket, ReserveError> {
        let BuyRequest { mint, rule, trigger, now } = request;
        let mut book = self.lock();
        if book.state.purchases.contains_key(mint) || book.state.positions.contains_key(mint) {
            return Err(ReserveError::AlreadyBought);
        }
        if let Some(side) = book.busy(mint) {
            return Err(ReserveError::Busy(side));
        }
        Ok(new_ticket(&mut book, Side::Buy, mint, rule, trigger, now))
    }

    /// Reserves a sell of the whole position. One trade per mint at a time.
    ///
    /// # Errors
    ///
    /// Returns why the sell was refused.
    pub fn reserve_sell(&self, mint: &str, now: i64) -> Result<Ticket, ReserveError> {
        let mut book = self.lock();
        let Some(position) = book.state.positions.get(mint) else {
            return Err(ReserveError::NoPosition);
        };
        let (rule, trigger) = (position.rule.clone(), position.trigger.clone());
        if let Some(side) = book.busy(mint) {
            return Err(ReserveError::Busy(side));
        }
        Ok(new_ticket(&mut book, Side::Sell, mint, &rule, trigger, now))
    }

    /// Drops a reservation that sent nothing.
    pub fn cancel(&self, ticket: &Ticket) {
        self.lock().tickets.remove(&ticket.id);
    }

    /// Writes the trade as pending, with its spend reservation, before the transaction goes on the
    /// wire: one write, with fsync. A buy's reservation must first fit the spend caps (per trade,
    /// per rolling hour, total). The reconciler sees the trade only after the write completed.
    ///
    /// # Errors
    ///
    /// Returns why nothing may be sent: a spend cap, an unknown ticket, or a failed write. The
    /// reservation is dropped.
    pub fn mark_pending(&self, ticket: &Ticket, sent: Sent, caps: &Spend, now: i64) -> Result<(), String> {
        let reserved_lamports = sent.reserved_lamports;
        let snapshot = {
            let mut book = self.lock();
            if book.tickets.remove(&ticket.id).is_none() {
                return Err(format!("trade {} is not reserved", ticket.id));
            }
            // A sell is never refused by the caps, so an exit always goes out.
            if ticket.side == Side::Buy {
                check_spend(&mut book.state.spend, reserved_lamports, caps, now)?;
            }
            book.state.spend.total_lamports += reserved_lamports;
            book.state.spend.recent.push(SpendEntry { trade_id: ticket.id.clone(), at: now, lamports: reserved_lamports });
            book.state.pending.insert(ticket.id.clone(), pending_of(ticket, sent, now));
            book.unpersisted.insert(ticket.id.clone());
            self.snapshot(&mut book)
        };
        let written = self.write(snapshot);
        let mut book = self.lock();
        book.unpersisted.remove(&ticket.id);
        let Err(error) = written else {
            return Ok(());
        };
        book.state.pending.remove(&ticket.id);
        refund(&mut book.state, &ticket.id, reserved_lamports);
        Err(format!("state write failed: {error}"))
    }

    /// Records the signature Jupiter returned for a pending trade.
    pub fn set_signature(&self, id: &str, signature: &str) {
        let snapshot = {
            let mut book = self.lock();
            let Some(pending) = book.state.pending.get_mut(id) else {
                return;
            };
            pending.signature = Some(signature.to_string());
            self.snapshot(&mut book)
        };
        self.try_write(snapshot);
    }

    /// Records the outcome of a pending trade in one write: a landed buy opens its position, a
    /// landed sell removes it. `Unknown` keeps it pending.
    pub fn resolve(&self, id: &str, outcome: &TradeOutcome, now: i64) {
        let snapshot = {
            let mut book = self.lock();
            if let TradeOutcome::Unknown { .. } = outcome {
                return;
            }
            let Some(pending) = book.state.pending.remove(id) else {
                return;
            };
            match (outcome, pending.side) {
                (TradeOutcome::Landed { token_amount, spent_lamports }, Side::Buy) => {
                    settle(&mut book.state, id, pending.reserved_lamports, *spent_lamports);
                    open_position(&mut book.state, &pending, *token_amount, now);
                }
                (TradeOutcome::Landed { .. }, Side::Sell) => {
                    book.state.positions.remove(&pending.mint);
                }
                // It paid its fees; without the actual debit the whole reservation stands.
                (TradeOutcome::Failed { spent_lamports, .. }, Side::Buy) => {
                    settle(&mut book.state, id, pending.reserved_lamports, *spent_lamports);
                }
                (TradeOutcome::NotLanded { .. }, Side::Buy | Side::Sell) => refund(&mut book.state, id, pending.reserved_lamports),
                // A sell keeps its reservation; failed, its position and pending exits stay.
                (TradeOutcome::Failed { .. } | TradeOutcome::Unknown { .. }, Side::Sell) | (TradeOutcome::Unknown { .. }, Side::Buy) => {}
            }
            self.snapshot(&mut book)
        };
        self.try_write(snapshot);
    }

    /// Dry run: a buy becomes a position at once, its reservation counted as spent (the spend caps
    /// apply as they would live); a sell removes the position.
    ///
    /// # Errors
    ///
    /// Returns the spend cap that refused the buy.
    pub fn confirm_dry_run(&self, ticket: &Ticket, sent: Sent, token_amount: Option<u64>, caps: &Spend, now: i64) -> Result<(), String> {
        let snapshot = {
            let mut book = self.lock();
            if book.tickets.remove(&ticket.id).is_none() {
                return Err(format!("trade {} is not reserved", ticket.id));
            }
            match ticket.side {
                Side::Buy => {
                    check_spend(&mut book.state.spend, sent.reserved_lamports, caps, now)?;
                    book.state.spend.total_lamports += sent.reserved_lamports;
                    book.state.spend.recent.push(SpendEntry { trade_id: ticket.id.clone(), at: now, lamports: sent.reserved_lamports });
                    let pending = pending_of(ticket, sent, now);
                    open_position(&mut book.state, &pending, token_amount, now);
                }
                Side::Sell => {
                    book.state.positions.remove(&ticket.mint);
                }
            }
            self.snapshot(&mut book)
        };
        self.try_write(snapshot);
        Ok(())
    }

    /// Records a token balance read for a position; 0 is not recorded.
    pub fn set_token_amount(&self, mint: &str, amount: u64) {
        let snapshot = {
            let mut book = self.lock();
            let Some(position) = book.state.positions.get_mut(mint).filter(|_| amount > 0) else {
                return;
            };
            position.token_amount = Some(amount);
            self.snapshot(&mut book)
        };
        self.try_write(snapshot);
    }

    /// Records an exit: on the position until a sell of it confirms, or on the mint's pending buy,
    /// whose position takes it over when the buy confirms.
    pub fn add_exit(&self, mint: &str, exit: PendingExit) -> ExitTarget {
        let (target, snapshot) = {
            let mut book = self.lock();
            let target = if let Some(position) = book.state.positions.get_mut(mint) {
                position.pending_exits.push(exit);
                ExitTarget::Position
            } else if let Some(pending) = book.state.pending.values_mut().find(|p| p.side == Side::Buy && p.mint == mint) {
                pending.pending_exits.push(exit);
                ExitTarget::PendingBuy
            } else {
                return ExitTarget::None;
            };
            (target, self.snapshot(&mut book))
        };
        self.try_write(snapshot);
        target
    }

    /// Returns the mints of positions and pending buys bought on a post (delete exits).
    pub fn exit_targets_for_post(&self, post_key: &str) -> Vec<String> {
        let book = self.lock();
        let on_post = |trigger: &Trigger| trigger.post_key.as_deref() == Some(post_key);
        let mut mints: Vec<String> = book.state.positions.values().filter(|p| on_post(&p.trigger)).map(|p| p.mint.clone()).collect();
        for pending in book.state.pending.values().filter(|p| p.side == Side::Buy && on_post(&p.trigger)) {
            if !mints.contains(&pending.mint) {
                mints.push(pending.mint.clone());
            }
        }
        mints
    }

    /// Positions to sell now: an exit is pending, the balance is known, and no trade of the mint
    /// is in flight.
    pub fn positions_to_exit(&self) -> Vec<Position> {
        let book = self.lock();
        let ready = |p: &&Position| !p.pending_exits.is_empty() && p.token_amount.is_some() && book.busy(&p.mint).is_none();
        book.state.positions.values().filter(ready).cloned().collect()
    }

    /// Returns the open position in a mint.
    pub fn position(&self, mint: &str) -> Option<Position> {
        self.lock().state.positions.get(mint).cloned()
    }

    /// Returns every open position.
    pub fn positions(&self) -> Vec<Position> {
        self.lock().state.positions.values().cloned().collect()
    }

    /// Returns true while a trade of the mint is reserved or pending.
    pub fn is_busy(&self, mint: &str) -> bool {
        self.lock().busy(mint).is_some()
    }

    /// Returns the pending trades the reconciler checks: those whose write completed.
    pub fn pending(&self) -> Vec<Pending> {
        let book = self.lock();
        book.state.pending.values().filter(|p| !book.unpersisted.contains(&p.id)).cloned().collect()
    }

    /// Writes the current state, logging a failure.
    pub fn flush(&self) {
        let snapshot = self.snapshot_now();
        self.try_write(snapshot);
    }

    fn lock(&self) -> MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Clones the state under the lock; it is serialized and written after the lock.
    fn snapshot(&self, book: &mut Book) -> Snapshot {
        self.file.as_ref()?;
        book.generation += 1;
        Some((book.generation, book.state.clone()))
    }

    fn snapshot_now(&self) -> Snapshot {
        let mut book = self.lock();
        self.snapshot(&mut book)
    }

    fn write(&self, snapshot: Snapshot) -> std::io::Result<()> {
        match (&self.file, snapshot) {
            (Some(file), Some((generation, state))) => file.write(generation, &state),
            (Some(_), None) | (None, _) => Ok(()),
        }
    }

    /// For changes after the money question is settled: a failed write is retried by the next one.
    fn try_write(&self, snapshot: Snapshot) {
        if let Err(error) = self.write(snapshot) {
            log("state", &format!("state write failed, the next change retries it: {error}"));
        }
    }
}

fn new_ticket(book: &mut Book, side: Side, mint: &str, rule: &str, trigger: Trigger, now: i64) -> Ticket {
    let id = format!("{}-{}", base36(now.max(0) as u64), base36(book.next_id));
    book.next_id += 1;
    let ticket = Ticket { id: id.clone(), side, mint: mint.to_string(), rule: rule.to_string(), trigger };
    book.tickets.insert(id, ticket.clone());
    ticket
}

fn pending_of(ticket: &Ticket, sent: Sent, now: i64) -> Pending {
    let Sent { route, signature, blockhash, last_valid_block_height, token_program, reserved_lamports, amount_lamports } = sent;
    Pending {
        id: ticket.id.clone(),
        side: ticket.side,
        mint: ticket.mint.clone(),
        route,
        signature,
        blockhash,
        last_valid_block_height,
        reserved_lamports,
        amount_lamports,
        rule: ticket.rule.clone(),
        trigger: ticket.trigger.clone(),
        token_program,
        sent_at: now,
        pending_exits: Vec::new(),
    }
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

fn check_spend(spend: &mut SpendState, lamports: u64, caps: &Spend, now: i64) -> Result<(), String> {
    if lamports > sol_to_lamports(caps.max_per_trade_sol) {
        return Err(format!("the largest possible debit is above spend.maxPerTradeSol ({} SOL)", caps.max_per_trade_sol));
    }
    spend.recent.retain(|s| now - s.at < HOUR_MS);
    let hour: u64 = spend.recent.iter().map(|s| s.lamports).sum();
    if hour + lamports > sol_to_lamports(caps.max_per_hour_sol) {
        return Err(format!("spend.maxPerHourSol ({} SOL) reached", caps.max_per_hour_sol));
    }
    if spend.total_lamports + lamports > sol_to_lamports(caps.max_total_sol) {
        return Err(format!("spend.maxTotalSol ({} SOL) reached", caps.max_total_sol));
    }
    Ok(())
}

/// Replaces a buy's reservation with what it actually cost. An unknown cost keeps the reservation.
fn settle(state: &mut State, id: &str, reserved: u64, actual: Option<u64>) {
    let Some(actual) = actual else {
        return;
    };
    state.spend.total_lamports = state.spend.total_lamports.saturating_sub(reserved) + actual;
    if let Some(entry) = state.spend.recent.iter_mut().find(|s| s.trade_id == id) {
        entry.lamports = actual;
    }
}

/// Gives a buy's reservation back: nothing was debited.
fn refund(state: &mut State, id: &str, reserved: u64) {
    settle(state, id, reserved, Some(0));
    state.spend.recent.retain(|s| s.trade_id != id);
}

fn open_position(state: &mut State, pending: &Pending, token_amount: Option<u64>, now: i64) {
    state.purchases.insert(pending.mint.clone(), Purchase { rule: pending.rule.clone(), at: now });
    let position = Position {
        mint: pending.mint.clone(),
        rule: pending.rule.clone(),
        trigger: pending.trigger.clone(),
        token_program: pending.token_program.clone(),
        opened_at: now,
        cost_lamports: pending.amount_lamports,
        token_amount: token_amount.filter(|t| *t > 0),
        pending_exits: pending.pending_exits.clone(),
    };
    state.positions.insert(pending.mint.clone(), position);
}
