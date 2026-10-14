//! Runs fired rules as trades. For one buy, top to bottom: reserve in the trade book, then
//! `filters/gate.rs`, then build and sign (pump.fun locally, or Jupiter for everything else), then
//! write the pending trade to disk, then send (`land/land.rs`, or Jupiter `/execute`), then record
//! the outcome. Sells and the reconciler, which finishes trades whose outcome was unknown, follow
//! the same steps. State rules live in `execute/trades.rs`.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use rand::RngExt;
use serde::Deserialize;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_message::{VersionedMessage, v0};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use tokio::task::JoinHandle;

use crate::execute::cache::ChainCache;
use crate::execute::pump::{
    WSOL_MINT, bonding_curve_address, buy_instructions, curve_fee_bps, lamports_for_tokens, parse_curve, sell_instruction,
    tokens_for_lamports, unsupported_curve, with_slippage,
};
use crate::execute::trades::{
    BuyRequest, ExitTarget, Pending, PendingExit, ReserveError, Sent, Ticket, TradeBook, TradeOutcome, TradeRoute, Trigger, sol_to_lamports,
};
use crate::filters::gate::{GateDeps, GateResult, GateVerdict, run_gate};
use crate::land::land::{Lander, SenderKind, SenderTarget, Status};
use crate::log::{log, short, unix_ms, wire_name};
use crate::network::{Network, OrderRequest};
use crate::report::latency::{LatencyReport, Trace};
use crate::rules::modes::Fire;
use crate::setup::config::{Config, FeeSetting, Rule};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JupiterCodes {
    landed: Vec<i64>,
    rejected: Vec<i64>,
}

/// The values both bots load from `shared/trading.json`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Trading {
    jito_tip_accounts: Vec<String>,
    jupiter_execute_codes: JupiterCodes,
}

static TRADING: LazyLock<(Vec<Pubkey>, JupiterCodes)> = LazyLock::new(|| {
    let trading: Trading = serde_json::from_str(include_str!("../../../shared/trading.json")).expect("shared/trading.json is valid");
    let tips = trading.jito_tip_accounts.iter().map(|a| a.parse().expect("shared/trading.json tip accounts are pubkeys")).collect();
    (tips, trading.jupiter_execute_codes)
});

/// Fee per signature.
const SIGNATURE_FEE: u64 = 5_000;
/// Rent for a Token-2022 token account with ImmutableOwner (170 bytes) and for pump.fun's per-user
/// volume accumulator (137 bytes, 0.0018444 SOL in `TRADE_V3.md`); rent is (bytes + 128) x 6960.
const TOKEN_ACCOUNT_RENT: u64 = 2_074_080;
const VOLUME_ACCUMULATOR_RENT: u64 = 1_844_400;
/// A buy grows an older, shorter curve account to this length and pays the rent difference.
const CURVE_FULL_LENGTH: u64 = 115;
const RENT_PER_BYTE: u64 = 6_960;
/// Jito ignores smaller tips.
const JITO_MIN_TIP: u64 = 1_000;
/// Rebroadcasts stop this many blocks after a blockhash whose own window is not known.
const FALLBACK_VALIDITY_BLOCKS: u64 = 150;
/// The reconciler's period.
const RECONCILE_EVERY: Duration = Duration::from_secs(2);

/// What a Jupiter `/execute` code means, from `shared/trading.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JupiterCode {
    /// It landed.
    Landed,
    /// Refused before landing; nothing was debited.
    Rejected,
    /// Anything else: check the chain later.
    Unknown,
}

/// Classifies a Jupiter `/execute` code.
pub fn jupiter_code(code: i64) -> JupiterCode {
    let codes = &TRADING.1;
    if codes.landed.contains(&code) {
        JupiterCode::Landed
    } else if codes.rejected.contains(&code) {
        JupiterCode::Rejected
    } else {
        JupiterCode::Unknown
    }
}

/// A signed transaction and its wire bytes.
#[derive(Debug, Clone)]
pub struct Signed {
    /// The transaction.
    pub transaction: VersionedTransaction,
    /// Its wire encoding, as sent.
    pub bytes: Vec<u8>,
}

impl Signed {
    /// Returns the first signature (the transaction id) in base58, or `None` while that slot is
    /// unsigned (a Jupiter market maker signs during `/execute`).
    pub fn signature(&self) -> Option<String> {
        self.transaction.signatures.first().filter(|s| **s != Signature::default()).map(ToString::to_string)
    }
}

/// Signs a message for the wallet, at the wallet's place among the required signers, keeping any
/// other signer's slot.
///
/// # Errors
///
/// Returns why when the wallet is not a required signer or the transaction cannot be encoded.
pub fn sign(message: VersionedMessage, mut signatures: Vec<Signature>, wallet: &Keypair) -> Result<Signed, String> {
    let required = usize::from(message.header().num_required_signatures);
    let Some(index) = message.static_account_keys().iter().take(required).position(|k| *k == wallet.pubkey()) else {
        return Err("the wallet is not a signer of this transaction".to_string());
    };
    signatures.resize(required, Signature::default());
    signatures[index] = wallet.sign_message(&message.serialize());
    let transaction = VersionedTransaction { signatures, message };
    let bytes = wincode::serialize(&transaction).map_err(|e| format!("encode: {e}"))?;
    Ok(Signed { transaction, bytes })
}

/// Compiles instructions into a v0 message paid by the wallet and signs it.
///
/// # Errors
///
/// Returns why when the message cannot be compiled or encoded.
pub fn compile_and_sign(wallet: &Keypair, instructions: &[Instruction], blockhash: Hash) -> Result<Signed, String> {
    let message = v0::Message::try_compile(&wallet.pubkey(), instructions, &[], blockhash).map_err(|e| format!("compile: {e}"))?;
    sign(VersionedMessage::V0(message), Vec::new(), wallet)
}

/// What one transaction pays besides its principal.
#[derive(Debug, Clone, Copy)]
struct FeePlan {
    units: u32,
    micro_lamports: u64,
    jito_tip: u64,
}

impl FeePlan {
    fn priority_lamports(&self) -> u64 {
        (u128::from(self.micro_lamports) * u128::from(self.units)).div_ceil(1_000_000) as u64
    }
}

/// The hard maximum wallet debit of a locally built pump.fun buy: principal, signature fee, priority
/// fee, Jito tip, token account and volume accumulator rent, and the curve's rent top-up.
fn pump_max_debit(principal: u64, plan: &FeePlan, curve_length: usize) -> u64 {
    let top_up = CURVE_FULL_LENGTH.saturating_sub(curve_length as u64) * RENT_PER_BYTE;
    principal + SIGNATURE_FEE + plan.priority_lamports() + plan.jito_tip + TOKEN_ACCOUNT_RENT + VOLUME_ACCUMULATOR_RENT + top_up
}

/// A signed transaction ready to record and send.
struct Built {
    signed: Signed,
    blockhash: String,
    last_valid_block_height: Option<u64>,
    route: TradeRoute,
    targets: Vec<SenderTarget>,
    request_id: Option<String>,
    /// A buy's hard maximum wallet debit; 0 for sells.
    max_debit: u64,
    /// What a buy spends on the token; 0 for sells.
    amount: u64,
}

/// A buy reserved in the trade book, with the fees its reservation was computed for.
#[derive(Debug, Clone)]
pub struct ReservedBuy {
    ticket: Ticket,
    plan: FeePlan,
}

/// What a sell attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SellResult {
    /// The sell landed (or the dry run recorded it).
    Sold,
    /// Sent, not landed.
    Sent,
    /// Nothing to sell.
    NoPosition,
    /// Another trade of the mint is in flight.
    Busy,
    /// Built or recorded nothing.
    NotSent,
}

/// Gives a fire back to the rule engine when the buy bought nothing.
pub type ReleaseFire = Arc<dyn Fn(&Fire) + Send + Sync>;

/// What the trader works with.
pub struct TraderDeps<N> {
    /// The config.
    pub config: Arc<Config>,
    /// Outside calls.
    pub network: Arc<N>,
    /// The trade state.
    pub book: Arc<TradeBook>,
    /// Background values.
    pub cache: Arc<ChainCache>,
    /// Stage timings.
    pub report: Arc<LatencyReport>,
    /// A throwaway key in dry run, so build and sign are still measured.
    pub wallet: Keypair,
    /// Send nothing.
    pub dry_run: bool,
    /// `None` in dry run.
    pub lander: Option<Lander>,
    /// Every RPC URL and, when configured, Jito.
    pub senders: Vec<SenderTarget>,
    /// Called once for a fire that bought nothing.
    pub release_fire: ReleaseFire,
}

/// Marks a trade as driven by this task until dropped, so the reconciler leaves it alone. Dropping
/// it on any path, cancellation included, hands the trade to the reconciler.
struct Inflight<'a> {
    set: &'a Mutex<HashSet<String>>,
    id: String,
}

impl<'a> Inflight<'a> {
    fn new(set: &'a Mutex<HashSet<String>>, id: &str) -> Inflight<'a> {
        set.lock().unwrap_or_else(|p| p.into_inner()).insert(id.to_string());
        Inflight { set, id: id.to_string() }
    }
}

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        self.set.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.id);
    }
}

/// Runs buys, sells, exits, and the reconciler.
pub struct Trader<N> {
    d: TraderDeps<N>,
    inflight: Mutex<HashSet<String>>,
    unreviewed: Mutex<HashSet<String>>,
}

impl<N: Network> Trader<N> {
    /// Creates the trader.
    pub fn new(d: TraderDeps<N>) -> Trader<N> {
        Trader { d, inflight: Mutex::new(HashSet::new()), unreviewed: Mutex::new(HashSet::new()) }
    }

    /// Reserves a buy for a fire, synchronously, so an exit for the mint that arrives before the
    /// buy lands finds it. Returns `None`, with the fire released, when the buy is refused.
    pub fn reserve_buy(&self, fire: &Fire, trace: &Trace) -> Option<ReservedBuy> {
        let TraderDeps { book, config, .. } = &self.d;
        let rule = &config.rules[fire.rule];
        let plan = self.fee_plan(rule);
        let trigger = Trigger { event_ref: trace.event_ref.clone(), post_key: fire.signal.as_ref().and_then(|s| s.post_key.clone()) };
        let request = BuyRequest { mint: &fire.mint, rule: &rule.name, trigger, now: unix_ms() };
        match book.reserve_buy(request) {
            Ok(ticket) => Some(ReservedBuy { ticket, plan }),
            Err(reason) => {
                self.skip(fire, &reason.to_string(), trace);
                None
            }
        }
    }

    /// Checks, builds, signs, records, and sends one reserved buy.
    pub async fn buy(&self, fire: Fire, reserved: ReservedBuy, mut trace: Trace) {
        let TraderDeps { book, config, dry_run, cache, .. } = &self.d;
        let ReservedBuy { ticket, plan } = reserved;
        let rule = &config.rules[fire.rule];
        let principal = sol_to_lamports(rule.token.amount_sol);
        let Ok(mint) = fire.mint.parse::<Pubkey>() else {
            book.cancel(&ticket);
            return self.skip(&fire, "not a Solana address", &trace);
        };
        let deps = GateDeps { safety: &config.safety, sol_usd: cache.sol_usd(), epoch: cache.epoch(), wallet: self.d.wallet.pubkey() };
        let gate = run_gate(self.d.network.as_ref(), &mint, rule, fire.signal.as_ref(), &deps).await;
        trace.gate_done = Some(Instant::now());
        trace.gate = Some(gate.elapsed);
        let (curve, token_program) = match &gate.verdict {
            GateVerdict::Pass { curve, token_program } => (*curve, *token_program),
            GateVerdict::Fail { code, detail } => {
                book.cancel(&ticket);
                return self.skip(&fire, &format!("{}: {detail}", wire_name(code)), &trace);
            }
        };
        let Some(tiers) = cache.fee_tiers() else {
            book.cancel(&ticket);
            return self.skip(&fire, "not sent: pump.fun fee tiers are not loaded yet", &trace);
        };
        let tokens = tokens_for_lamports(&curve, principal, curve_fee_bps(&tiers, &curve));
        let instructions =
            buy_instructions(&self.d.wallet.pubkey(), &mint, &token_program, principal, with_slippage(tokens, rule.fees.slippage_pct));
        let mut built = match self.compile_local(rule, &plan, instructions) {
            Ok(built) => built,
            Err(error) => {
                book.cancel(&ticket);
                return self.skip(&fire, &format!("not sent: {error}"), &trace);
            }
        };
        (built.max_debit, built.amount) = (pump_max_debit(principal, &plan, curve.data_length), principal);
        trace.signed = Some(Instant::now());
        let what = format!("pump buy of {} for {} SOL{}", short(&fire.mint), rule.token.amount_sol, describe_facts(&gate));
        if *dry_run {
            log("dry run", &format!("{}: would send a signed {}-byte {what}", rule.name, built.signed.bytes.len()));
            let sent = dry_sent(&token_program, built.max_debit, built.amount);
            if let Err(reason) = book.confirm_dry_run(&ticket, sent, Some(tokens), &config.spend, unix_ms()) {
                return self.skip(&fire, &reason, &trace);
            }
            return self.d.report.finish(&trace, "dry run, not sent");
        }
        let _inflight = Inflight::new(&self.inflight, &ticket.id);
        let outcome = match self.send_recorded(&ticket, built, &token_program.to_string(), &mut trace).await {
            Ok(outcome) => outcome,
            Err(reason) => return self.skip(&fire, &format!("not sent: {reason}"), &trace),
        };
        log("send", &format!("{}: {what} -> {}", rule.name, outcome.kind()));
        book.resolve(&ticket.id, &outcome, unix_ms());
        self.d.report.finish(&trace, &finished("buy", &outcome));
        if let TradeOutcome::NotLanded { .. } = outcome {
            (self.d.release_fire)(&fire);
        }
    }

    /// Sells the whole position in a mint.
    pub async fn sell(&self, mint: &str, reason: &str) -> SellResult {
        let TraderDeps { book, dry_run, network, config, wallet, cache, .. } = &self.d;
        let ticket = match book.reserve_sell(mint, unix_ms()) {
            Ok(ticket) => ticket,
            Err(ReserveError::NoPosition) => return SellResult::NoPosition,
            Err(why) => {
                log("exit", &format!("{}: not selling now ({why})", short(mint)));
                return SellResult::Busy;
            }
        };
        let Some(position) = book.position(mint) else {
            book.cancel(&ticket);
            return SellResult::NoPosition;
        };
        if *dry_run {
            log("dry run", &format!("would sell {} ({reason})", short(mint)));
            let sent = Sent {
                route: TradeRoute::Pump,
                signature: None,
                blockhash: String::new(),
                last_valid_block_height: None,
                token_program: position.token_program.clone(),
                reserved_lamports: 0,
                amount_lamports: 0,
            };
            return match book.confirm_dry_run(&ticket, sent, None, &config.spend, unix_ms()) {
                Ok(()) => SellResult::Sold,
                Err(_) => SellResult::NotSent,
            };
        }
        let mut trace = Trace::new(mint, &format!("exit:{reason}"), "-", reason, None, unix_ms(), Instant::now());
        trace.decided = Some(trace.received);
        let built: Result<Built, String> = async {
            let tokens = match position.token_amount {
                Some(amount) => amount,
                None => network.token_balance(&wallet.pubkey().to_string(), mint).await.map_err(|e| e.0)?,
            };
            if tokens == 0 {
                return Err("token balance not known yet".to_string());
            }
            let mint_key: Pubkey = mint.parse().map_err(|_| "not a Solana address".to_string())?;
            let program: Pubkey = position.token_program.parse().map_err(|_| "token program is not a pubkey".to_string())?;
            let rule = config.rules.iter().find(|r| r.name == position.rule).unwrap_or(&config.rules[0]);
            let plan = self.fee_plan(rule);
            let accounts = network.get_accounts(&[bonding_curve_address(&mint_key).to_string()]).await.map_err(|e| e.0)?;
            let curve = accounts.into_iter().next().flatten().and_then(|a| parse_curve(&a.data));
            match (curve, cache.fee_tiers()) {
                (Some(curve), Some(tiers)) if !curve.complete && unsupported_curve(&curve).is_none() => {
                    let quote = lamports_for_tokens(&curve, tokens, curve_fee_bps(&tiers, &curve));
                    let min_out = with_slippage(quote, config.exits.sell_slippage_pct);
                    let mut built =
                        self.compile_local(rule, &plan, vec![sell_instruction(&wallet.pubkey(), &mint_key, &program, tokens, min_out)])?;
                    built.max_debit = SIGNATURE_FEE + plan.priority_lamports() + plan.jito_tip;
                    Ok(built)
                }
                // Jupiter sets its own fee, so the reservation is the automatic priority fee ceiling.
                _ => {
                    let mut built = self.jupiter_built(mint, &WSOL_MINT.to_string(), tokens).await?;
                    built.max_debit = SIGNATURE_FEE + sol_to_lamports(config.execution.max_auto_priority_fee_sol);
                    Ok(built)
                }
            }
        }
        .await;
        let built = match built {
            Ok(built) => built,
            Err(error) => {
                book.cancel(&ticket);
                log("exit", &format!("{}: sell not sent: {error}", short(mint)));
                return SellResult::NotSent;
            }
        };
        trace.signed = Some(Instant::now());
        let _inflight = Inflight::new(&self.inflight, &ticket.id);
        let outcome = match self.send_recorded(&ticket, built, &position.token_program, &mut trace).await {
            Ok(outcome) => outcome,
            Err(reason) => {
                log("exit", &format!("{}: sell not sent: {reason}", short(mint)));
                return SellResult::NotSent;
            }
        };
        book.resolve(&ticket.id, &outcome, unix_ms());
        self.d.report.finish(&trace, &finished("sell", &outcome));
        match outcome {
            TradeOutcome::Landed { .. } => SellResult::Sold,
            TradeOutcome::Failed { .. } | TradeOutcome::NotLanded { .. } | TradeOutcome::Unknown { .. } => SellResult::Sent,
        }
    }

    /// Sells a position whose exits are pending, once its token balance is known.
    pub async fn run_exits(&self, mint: &str) {
        let Some(position) = self.d.book.position(mint).filter(|p| !p.pending_exits.is_empty()) else {
            return;
        };
        let codes: Vec<String> = position.pending_exits.iter().map(|e| wire_name(&e.code)).collect();
        if position.token_amount.is_none() && !self.d.dry_run {
            return log("exit", &format!("{}: waiting for the token balance before selling ({})", short(mint), codes.join(", ")));
        }
        self.sell(mint, &codes.join(", ")).await;
    }

    /// Records an exit for a mint and sells the position now. An exit for a pending buy waits on
    /// it, and the position takes it over when the buy confirms.
    pub async fn exit(&self, mint: &str, exit: PendingExit) {
        let code = wire_name(&exit.code);
        match self.d.book.add_exit(mint, exit) {
            ExitTarget::Position => self.run_exits(mint).await,
            ExitTarget::PendingBuy => {
                log("exit", &format!("{}: {code} recorded on the pending buy; it sells once the buy confirms", short(mint)))
            }
            ExitTarget::None => {}
        }
    }

    /// Writes the pending trade and its spend reservation to disk before anything goes out, then
    /// sends it.
    ///
    /// # Errors
    ///
    /// Returns why nothing was sent: a spend cap, or the write failed.
    async fn send_recorded(&self, ticket: &Ticket, built: Built, token_program: &str, trace: &mut Trace) -> Result<TradeOutcome, String> {
        let sent = Sent {
            route: built.route,
            signature: built.signed.signature(),
            blockhash: built.blockhash.clone(),
            last_valid_block_height: built.last_valid_block_height,
            token_program: token_program.to_string(),
            reserved_lamports: built.max_debit,
            amount_lamports: built.amount,
        };
        self.d.book.mark_pending(ticket, sent, &self.d.config.spend, unix_ms())?;
        Ok(match &built.request_id {
            Some(request_id) => self.jupiter_execute(ticket, &built, request_id, trace).await,
            None => self.land_local(built, trace).await,
        })
    }

    async fn land_local(&self, built: Built, trace: &mut Trace) -> TradeOutcome {
        let (Some(lander), Some(signature)) = (&self.d.lander, built.signed.signature()) else {
            return TradeOutcome::Unknown { error: "no lander in dry run".to_string() };
        };
        let last_valid = built.last_valid_block_height.or_else(|| self.d.cache.block_height().map(|h| h + FALLBACK_VALIDITY_BLOCKS));
        let landing = lander.land(&built.signed.bytes, &signature, last_valid, built.targets).await;
        trace.sent = Some(landing.sent);
        trace.first_ack = landing.first_ack;
        trace.first_ack_by = landing.first_ack_by.clone();
        trace.confirmed = landing.confirmed;
        match landing.status {
            Status::Unknown(error) => TradeOutcome::Unknown { error },
            Status::Confirmed => TradeOutcome::Landed { token_amount: None, spent_lamports: self.debit(&signature).await },
            Status::Failed(error) => TradeOutcome::Failed { error, spent_lamports: self.debit(&signature).await },
        }
    }

    /// Jupiter lands the transaction itself; its code maps through `shared/trading.json`. A timeout
    /// or an unreadable answer proves nothing, so the trade stays pending.
    async fn jupiter_execute(&self, ticket: &Ticket, built: &Built, request_id: &str, trace: &mut Trace) -> TradeOutcome {
        trace.sent = Some(Instant::now());
        let base64 = base64::engine::general_purpose::STANDARD.encode(&built.signed.bytes);
        let timeout = Duration::from_millis(self.d.config.execution.confirm_timeout_ms);
        let result = match self.d.network.jupiter_execute(&base64, request_id, timeout).await {
            Ok(result) => result,
            Err(error) => return TradeOutcome::Unknown { error: error.0 },
        };
        let answered = Instant::now();
        (trace.first_ack, trace.confirmed, trace.first_ack_by) = (Some(answered), Some(answered), Some("jupiter".to_string()));
        if let Some(signature) = &result.signature {
            self.d.book.set_signature(&ticket.id, signature);
        }
        let error = format!("Jupiter code {}{}", result.code, result.error.as_ref().map(|e| format!(": {e}")).unwrap_or_default());
        match jupiter_code(result.code) {
            // A success must say so and name a signature to check on chain; anything less waits.
            JupiterCode::Landed => match result.signature.or_else(|| built.signed.signature()) {
                Some(signature) if result.status == "Success" => {
                    TradeOutcome::Landed { token_amount: None, spent_lamports: self.debit(&signature).await }
                }
                Some(_) | None => TradeOutcome::Unknown { error: format!("{error}, status {}, no usable signature", result.status) },
            },
            JupiterCode::Rejected => TradeOutcome::NotLanded { error },
            JupiterCode::Unknown => TradeOutcome::Unknown { error },
        }
    }

    async fn debit(&self, signature: &str) -> Option<u64> {
        let debit = self.d.network.transaction_debit(signature, &self.d.wallet.pubkey().to_string()).await.ok().flatten()?;
        u64::try_from(debit).ok()
    }

    /// Checks pending trades now and every 2 seconds. Abort the returned handle to stop.
    pub fn start(self: &Arc<Self>) -> JoinHandle<()> {
        let trader = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(RECONCILE_EVERY);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                trader.reconcile().await;
            }
        })
    }

    /// Finishes pending trades whose outcome is known now, reads token balances that are still
    /// missing, and sells positions with pending exits.
    pub async fn reconcile(&self) {
        let TraderDeps { book, network, .. } = &self.d;
        for pending in book.pending() {
            if self.inflight.lock().unwrap_or_else(|p| p.into_inner()).contains(&pending.id) {
                continue;
            }
            let Some(outcome) = self.find_outcome(&pending).await else { continue };
            book.resolve(&pending.id, &outcome, unix_ms());
            log("reconcile", &format!("{} {}: {}", wire_name(&pending.side), short(&pending.mint), outcome.kind()));
        }
        let wallet = self.d.wallet.pubkey().to_string();
        for position in book.positions().into_iter().filter(|p| p.token_amount.is_none()) {
            let balance = network.token_balance(&wallet, &position.mint).await.unwrap_or(0);
            book.set_token_amount(&position.mint, balance);
        }
        for position in book.positions_to_exit() {
            self.run_exits(&position.mint).await;
        }
    }

    /// Landed or failed once the signature is confirmed. Not landed only when the signature is not
    /// found, the blockhash is no longer valid, and a second lookup still finds nothing: a
    /// transaction can confirm between the first lookup and the blockhash check. `None` while it
    /// may still land.
    async fn find_outcome(&self, pending: &Pending) -> Option<TradeOutcome> {
        let network = &self.d.network;
        let Some(signature) = &pending.signature else {
            if self.unreviewed.lock().unwrap_or_else(|p| p.into_inner()).insert(pending.id.clone()) {
                log("reconcile", &manual_review(pending));
            }
            return None;
        };
        match self.lookup(signature).await {
            Lookup::Confirmed(outcome) => return Some(outcome),
            Lookup::Unconfirmed | Lookup::Failed => return None,
            Lookup::NotFound => {}
        }
        if network.is_blockhash_valid(&pending.blockhash).await != Ok(false) {
            return None;
        }
        match self.lookup(signature).await {
            Lookup::Confirmed(outcome) => Some(outcome),
            Lookup::NotFound => Some(TradeOutcome::NotLanded { error: "the signature is unknown and its blockhash expired".to_string() }),
            Lookup::Unconfirmed | Lookup::Failed => None,
        }
    }

    async fn lookup(&self, signature: &str) -> Lookup {
        match self.d.network.signature_status(signature).await {
            Err(_) => Lookup::Failed,
            Ok(None) => Lookup::NotFound,
            Ok(Some(status)) if !status.confirmed => Lookup::Unconfirmed,
            Ok(Some(status)) => {
                let spent = self.debit(signature).await;
                Lookup::Confirmed(match status.err {
                    Some(err) => TradeOutcome::Failed { error: err.to_string(), spent_lamports: spent },
                    None => TradeOutcome::Landed { token_amount: None, spent_lamports: spent },
                })
            }
        }
    }

    fn fee_plan(&self, rule: &Rule) -> FeePlan {
        let TraderDeps { cache, config, senders, .. } = &self.d;
        let units = config.execution.compute_unit_limit;
        let per_unit = |lamports: u64| (u128::from(lamports) * 1_000_000 / u128::from(units.max(1))) as u64;
        let micro_lamports = match rule.fees.priority_fee_sol {
            FeeSetting::Sol(sol) => per_unit(sol_to_lamports(sol)),
            FeeSetting::Auto(_) => {
                cache.priority_micro_lamports().unwrap_or(0).min(per_unit(sol_to_lamports(config.execution.max_auto_priority_fee_sol)))
            }
        };
        let tip_sol = match rule.fees.tip_sol {
            FeeSetting::Sol(sol) => sol,
            FeeSetting::Auto(_) => cache.tip_floor_sol().unwrap_or(0.0).min(config.execution.max_auto_tip_sol),
        };
        let jito = rule.fees.anti_mev && senders.iter().any(|s| s.kind == SenderKind::Jito);
        let jito_tip = if jito { sol_to_lamports(tip_sol).max(JITO_MIN_TIP) } else { 0 };
        FeePlan { units, micro_lamports, jito_tip }
    }

    /// Compute budget, the trade, then the Jito tip when the rule sends through Jito.
    fn compile_local(&self, rule: &Rule, plan: &FeePlan, trade: Vec<Instruction>) -> Result<Built, String> {
        let wanted = if rule.fees.anti_mev { SenderKind::Jito } else { SenderKind::Rpc };
        let targets: Vec<SenderTarget> = self.d.senders.iter().filter(|s| s.kind == wanted).cloned().collect();
        if targets.is_empty() && !self.d.dry_run {
            return Err("antiMev sends only through Jito; set JITO_URL in .env or turn antiMev off".to_string());
        }
        let Some(blockhash) = self.d.cache.fresh_blockhash() else {
            return Err("no fresh blockhash (the background refresh is failing)".to_string());
        };
        let hash: Hash = blockhash.blockhash.parse().map_err(|_| "cached blockhash is not base58".to_string())?;
        let mut instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(plan.units),
            ComputeBudgetInstruction::set_compute_unit_price(plan.micro_lamports),
        ];
        instructions.extend(trade);
        if plan.jito_tip > 0 {
            let accounts = &TRADING.0;
            let to = accounts[rand::rng().random_range(0..accounts.len())];
            instructions.push(solana_system_interface::instruction::transfer(&self.d.wallet.pubkey(), &to, plan.jito_tip));
        }
        let signed = compile_and_sign(&self.d.wallet, &instructions, hash)?;
        Ok(Built {
            signed,
            blockhash: blockhash.blockhash,
            last_valid_block_height: Some(blockhash.last_valid_block_height),
            route: TradeRoute::Pump,
            targets,
            request_id: None,
            max_debit: 0,
            amount: 0,
        })
    }

    /// Jupiter Swap V2 `/order` with the wallet as taker. Jupiter lands it through `/execute`, so
    /// the bot's own senders are not used.
    async fn jupiter_built(&self, input_mint: &str, output_mint: &str, amount: u64) -> Result<Built, String> {
        let request = OrderRequest {
            input_mint: input_mint.to_string(),
            output_mint: output_mint.to_string(),
            amount,
            taker: self.d.wallet.pubkey().to_string(),
        };
        let order = self.d.network.jupiter_order(request).await.map_err(|e| e.0)?;
        let Some(transaction) = order.transaction else {
            let why = order.error_message.unwrap_or_else(|| "no route".to_string());
            return Err(format!("Jupiter /order returned no transaction: {why}"));
        };
        let bytes =
            base64::engine::general_purpose::STANDARD.decode(transaction).map_err(|e| format!("Jupiter /order transaction: {e}"))?;
        let tx: VersionedTransaction = wincode::deserialize(&bytes).map_err(|e| format!("Jupiter /order transaction: {e}"))?;
        let VersionedTransaction { signatures, message } = tx;
        let blockhash = message.recent_blockhash().to_string();
        let signed = sign(message, signatures, &self.d.wallet)?;
        Ok(Built {
            signed,
            blockhash,
            last_valid_block_height: order.last_valid_block_height,
            route: TradeRoute::Jupiter,
            targets: Vec::new(),
            request_id: Some(order.request_id),
            max_debit: 0,
            amount: 0,
        })
    }

    /// Logs a buy that bought nothing and gives its fire back.
    fn skip(&self, fire: &Fire, reason: &str, trace: &Trace) {
        (self.d.release_fire)(fire);
        log("skip", &format!("{} {}: {reason}", self.d.config.rules[fire.rule].name, short(&fire.mint)));
        let code = reason.split(':').next().unwrap_or(reason);
        self.d.report.finish(trace, &format!("skipped ({code})"));
    }
}

fn finished(side: &str, outcome: &TradeOutcome) -> String {
    match outcome.error() {
        Some(error) => format!("{side} {}: {error}", outcome.kind()),
        None => format!("{side} {}", outcome.kind()),
    }
}

/// What a signature lookup found.
enum Lookup {
    /// The call failed.
    Failed,
    /// The RPC has not seen the signature.
    NotFound,
    /// Seen, not confirmed yet.
    Unconfirmed,
    /// Confirmed, with or without an error.
    Confirmed(TradeOutcome),
}

/// What to do about a pending trade with no signature to follow.
fn manual_review(pending: &Pending) -> String {
    format!(
        "trade {} ({} of {}) has no signature to follow and waits for you. Stop the bot and look up the wallet's \
         transactions for this mint. If the sell landed, delete positions[\"{}\"] and pending[\"{}\"] in the same edit. If it \
         did not, delete pending[\"{}\"] and subtract its reservedLamports ({}) from spend.totalLamports. Then start the bot again.",
        pending.id,
        wire_name(&pending.side),
        pending.mint,
        pending.mint,
        pending.id,
        pending.id,
        pending.reserved_lamports
    )
}

/// What a dry run records for a buy it does not send.
fn dry_sent(token_program: &Pubkey, reserved_lamports: u64, amount_lamports: u64) -> Sent {
    let token_program = token_program.to_string();
    let route = TradeRoute::Pump;
    Sent {
        route,
        signature: None,
        blockhash: String::new(),
        last_valid_block_height: None,
        token_program,
        reserved_lamports,
        amount_lamports,
    }
}

fn describe_facts(gate: &GateResult) -> String {
    let facts = &gate.facts;
    let mut parts = vec![format!("checks {:.1} ms", gate.elapsed.as_secs_f64() * 1000.0)];
    if let Some(cap) = facts.market_cap_usd {
        parts.push(format!("mcap ${}", cap.round()));
    }
    if let Some(liquidity) = facts.liquidity_usd {
        parts.push(format!("liquidity ${}", liquidity.round()));
    }
    if let Some(tax) = facts.tax_pct.filter(|t| *t != 0.0) {
        parts.push(format!("tax {tax}%"));
    }
    format!(" ({})", parts.join(", "))
}
