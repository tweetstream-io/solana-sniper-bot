//! Runs fired rules as trades. For one buy, top to bottom: reserve in the trade book, then
//! `filters/gate.rs`, then build and sign (pump.fun locally, PumpPortal, or Jupiter), then record
//! `sending` on disk, then send (`land/land.rs`, or Jupiter `/execute`), then resolve. Sells and the
//! reconciler, which finishes trades whose outcome was unknown, follow the same steps. State rules
//! live in `execute/trades.rs`.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use rand::RngExt;
use serde_json::json;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::{VersionedMessage, v0};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use tokio::task::JoinHandle;

use crate::execute::cache::ChainCache;
use crate::execute::pump::{
    bonding_curve_address, buy_instructions, curve_fee_bps, lamports_for_tokens, local_build_blocker, parse_curve, sell_instruction,
    tokens_for_lamports, with_slippage,
};
use crate::execute::trades::{
    BuyRequest, ExitIntent, ExitTarget, Outcome, Position, PositionRoute, Sent, Side, Trade, TradeBook, TradeRoute, Trigger,
    sol_to_lamports,
};
use crate::filters::gate::{GateDeps, GateResult, Route, Verdict, run_gate};
use crate::land::land::{Lander, SenderKind, SenderTarget, Status};
use crate::log::{log, short};
use crate::network::{Network, OrderRequest, WSOL};
use crate::report::latency::{LatencyReport, Trace};
use crate::rules::modes::Fire;
use crate::setup::config::{Config, FeeSetting, PumpBuilder, Rule};

/// Jito tip accounts, as listed by `getTipAccounts` in <https://docs.jito.wtf/lowlatencytxnsend/>.
/// Jito says they do not change, so they are not fetched at startup.
const JITO_TIP_ACCOUNTS: [Pubkey; 8] = [
    Pubkey::from_str_const("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5"),
    Pubkey::from_str_const("HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe"),
    Pubkey::from_str_const("Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY"),
    Pubkey::from_str_const("ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49"),
    Pubkey::from_str_const("DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh"),
    Pubkey::from_str_const("ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt"),
    Pubkey::from_str_const("DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL"),
    Pubkey::from_str_const("3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT"),
];

/// Largest costs a buy can add to its principal, for the spend reservation: the signature fee,
/// rent for a Token-2022 token account with ImmutableOwner (170 bytes), and rent for pump.fun's
/// per-user volume accumulator (137 bytes, 0.0018444 SOL in `TRADE_V3.md`). Rent is
/// (bytes + 128) x 6960.
const SIGNATURE_FEE: i128 = 5_000;
const TOKEN_ACCOUNT_RENT: i128 = 2_074_080;
const VOLUME_ACCUMULATOR_RENT: i128 = 1_844_400;
/// Jito ignores smaller tips.
const JITO_MIN_TIP: i128 = 1_000;
/// Jupiter `/execute` codes that reject a transaction before it can land.
const JUPITER_REJECTED: [i64; 9] = [-1, -2, -3, -1002, -1003, -1004, -2002, -2003, -2004];

/// A signed transaction and its wire bytes.
#[derive(Debug, Clone)]
pub struct Signed {
    /// The transaction.
    pub transaction: VersionedTransaction,
    /// Its wire encoding, as sent.
    pub bytes: Vec<u8>,
}

impl Signed {
    /// Returns the fee payer's signature in base58.
    pub fn signature(&self) -> String {
        self.transaction.signatures.first().map(ToString::to_string).unwrap_or_default()
    }
}

/// Signs a message for the wallet, at the wallet's place among the required signers, keeping any
/// other signer's slot (a Jupiter market maker signs during `/execute`).
///
/// # Errors
///
/// Returns why when the wallet is not a required signer or the transaction cannot be encoded.
pub fn sign(message: VersionedMessage, mut signatures: Vec<solana_signature::Signature>, wallet: &Keypair) -> Result<Signed, String> {
    let required = usize::from(message.header().num_required_signatures);
    let Some(index) = message.static_account_keys().iter().take(required).position(|k| *k == wallet.pubkey()) else {
        return Err("the wallet is not a signer of this transaction".to_string());
    };
    signatures.resize(required, solana_signature::Signature::default());
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

/// One tip to an extra sender.
#[derive(Debug, Clone)]
struct SenderTip {
    name: String,
    account: Pubkey,
    lamports: i128,
}

/// What one transaction pays besides its principal, and so where it may be sent.
#[derive(Debug, Clone)]
struct FeePlan {
    priority_lamports: i128,
    jito_tip: i128,
    sender_tips: Vec<SenderTip>,
}

/// A signed transaction ready to record and send.
struct Built {
    signed: Signed,
    last_valid_block_height: u64,
    route: TradeRoute,
    targets: Vec<SenderTarget>,
    request_id: Option<String>,
}

/// What a sell attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SellResult {
    /// The sell landed (or the dry run recorded it).
    Sold,
    /// Sent, outcome not landed.
    Sent,
    /// Nothing to sell.
    NoPosition,
    /// An earlier sell is unresolved.
    Busy,
    /// Built or recorded nothing.
    NotSent,
}

/// Gives a fire back to the rule engine when nothing of it was sent.
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
    /// Every configured sender; each transaction picks its subset.
    pub senders: Vec<SenderTarget>,
    /// Called once for a fire that sent nothing.
    pub release_fire: ReleaseFire,
}

/// Runs buys, sells, exits, and the reconciler.
pub struct Trader<N> {
    d: TraderDeps<N>,
    inflight: Mutex<HashSet<String>>,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl<N: Network> Trader<N> {
    /// Creates the trader.
    pub fn new(d: TraderDeps<N>) -> Trader<N> {
        Trader { d, inflight: Mutex::new(HashSet::new()) }
    }

    /// Reserves, checks, builds, signs, records, and sends one buy.
    pub async fn buy(&self, fire: Fire, mut trace: Trace) {
        let TraderDeps { book, config, dry_run, .. } = &self.d;
        let rule = &config.rules[fire.rule];
        let plan = self.fee_plan(rule);
        let principal = sol_to_lamports(rule.token.amount_sol);
        let fee_budget = plan_cost(&plan) + TOKEN_ACCOUNT_RENT + VOLUME_ACCUMULATOR_RENT;
        let trigger = Trigger {
            rule: rule.name.clone(),
            fire_key: fire.fire_key.clone(),
            event_ref: trace.event_ref.clone(),
            post_key: fire.signal.as_ref().and_then(|s| s.post_key.clone()),
            actor: fire.actor.clone(),
        };
        let reserved = book.reserve_buy(BuyRequest {
            mint: &fire.mint,
            trigger,
            route: if *dry_run { TradeRoute::DryRun } else { TradeRoute::Pump },
            max_debit: principal + fee_budget,
            fee_budget,
            caps: &config.spend,
            skip_if_already_bought: rule.filters.skip_if_already_bought,
            skip_if_already_holding: rule.filters.skip_if_already_holding,
            now: now_ms(),
        });
        let trade = match reserved {
            Ok(trade) => trade,
            Err(reason) => return self.drop_fire(&fire, &reason, &trace),
        };
        let Ok(mint) = fire.mint.parse::<Pubkey>() else {
            book.fail_before_send(&trade.id, "not a Solana address");
            return self.drop_fire(&fire, "not a Solana address", &trace);
        };
        let deps = GateDeps {
            safety: &config.safety,
            pump_builder: config.execution.pump_builder,
            sol_usd: self.d.cache.sol_usd(),
            epoch: self.d.cache.epoch(),
            wallet: self.d.wallet.pubkey(),
        };
        let gate = run_gate(self.d.network.as_ref(), &mint, rule, fire.signal.as_ref(), &deps).await;
        trace.gate_done = Some(Instant::now());
        trace.gate = Some(gate.elapsed);
        if let Verdict::Fail { code, detail } = &gate.verdict {
            let reason = format!("{}: {detail}", code.as_str());
            book.fail_before_send(&trade.id, &reason);
            return self.drop_fire(&fire, &reason, &trace);
        }
        let sent = self.attempt_buy(&fire, trade, gate, plan, principal, trace, rule.fees.retries_on_graduation).await;
        if !sent {
            (self.d.release_fire)(&fire);
        }
    }

    /// One attempt, and a graduation retry if the curve completed under it. Returns whether any
    /// transaction was sent (a dry run counts as sent); the fire is released only when none was.
    #[expect(clippy::too_many_arguments, reason = "one attempt's inputs, passed again to the retry")]
    fn attempt_buy<'a>(
        &'a self,
        fire: &'a Fire,
        trade: Trade,
        gate: GateResult,
        plan: FeePlan,
        principal: i128,
        mut trace: Trace,
        retries: u32,
    ) -> Boxed<'a, bool> {
        Box::pin(async move {
            let TraderDeps { book, config, dry_run, cache, .. } = &self.d;
            let rule = &config.rules[fire.rule];
            let Verdict::Pass { route, token_program } = gate.verdict else {
                return false;
            };
            let position_route = match route {
                Route::Jupiter => PositionRoute::Jupiter,
                Route::Pump(_) | Route::Pumpportal(_) => PositionRoute::Pump,
            };
            let position = |token_amount: Option<u64>| Position {
                mint: fire.mint.clone(),
                route: position_route,
                token_program: token_program.to_string(),
                opened_at: now_ms(),
                buy_trade_id: trade.id.clone(),
                cost_lamports: principal,
                token_amount: token_amount.map(|t| t.to_string()),
                trigger: trade.trigger.clone(),
                sell_trade_id: None,
            };
            if *dry_run && route == Route::Jupiter {
                let what = format!(
                    "{}: would buy {} for {} SOL through Jupiter /order and /execute{}",
                    rule.name,
                    short(&fire.mint),
                    rule.token.amount_sol,
                    describe_facts(&gate)
                );
                log("dry run", &what);
                let intents = book.confirm_dry_run(&trade.id, now_ms()).unwrap_or_default();
                book.open_position(position(None));
                self.d.report.finish(&trace, "dry run, not built");
                self.run_intents(&fire.mint, intents).await;
                return true;
            }
            let built = match self.build_buy(rule, &fire.mint, principal, route, token_program, &plan).await {
                Ok(built) => built,
                Err(error) => {
                    book.fail_before_send(&trade.id, &format!("build failed: {error}"));
                    log("skip", &format!("{} {}: build failed: {error}", rule.name, short(&fire.mint)));
                    self.d.report.finish(&trace, "build failed, nothing sent");
                    return false;
                }
            };
            trace.signed = Some(Instant::now());
            let what =
                format!("{} buy of {} for {} SOL{}", built.route.as_str(), short(&fire.mint), rule.token.amount_sol, describe_facts(&gate));
            if *dry_run {
                log("dry run", &format!("{}: would send a signed {}-byte {what}", rule.name, built.signed.bytes.len()));
                let intents = book.confirm_dry_run(&trade.id, now_ms()).unwrap_or_default();
                let tokens = match (route, cache.fee_tiers()) {
                    (Route::Pump(curve) | Route::Pumpportal(curve), Some(tiers)) => {
                        Some(tokens_for_lamports(&curve, principal as u64, curve_fee_bps(&tiers, &curve)))
                    }
                    (Route::Pump(_) | Route::Pumpportal(_) | Route::Jupiter, _) => None,
                };
                book.open_position(position(tokens));
                self.d.report.finish(&trace, "dry run, not sent");
                self.run_intents(&fire.mint, intents).await;
                return true;
            }
            let signature = built.signed.signature();
            let built_route = built.route;
            let Some(outcome) = self.send_recorded(&trade, built, &mut trace).await else {
                return false;
            };
            log("send", &format!("{}: {what}, {signature} -> {}", rule.name, outcome.kind()));
            let finished = match outcome.error() {
                Some(error) => format!("buy {}: {error}", outcome.kind()),
                None => format!("buy {}", outcome.kind()),
            };
            let intents = match book.resolve(&trade.id, outcome.clone(), now_ms()) {
                Ok(intents) => intents,
                Err(error) => {
                    log("error", &error.0);
                    Vec::new()
                }
            };
            self.d.report.finish(&trace, &finished);
            match outcome {
                Outcome::Landed { token_amount, .. } => {
                    book.open_position(position(token_amount));
                    self.run_intents(&fire.mint, intents).await;
                }
                Outcome::Failed { .. } if built_route == TradeRoute::Pump && retries > 0 && self.graduated(&fire.mint).await => {
                    log("retry", &format!("{} graduated during the buy; retrying through Jupiter", short(&fire.mint)));
                    let retry = book.reserve_buy(BuyRequest {
                        mint: &fire.mint,
                        trigger: trade.trigger.clone(),
                        route: TradeRoute::Jupiter,
                        max_debit: trade.reserved_lamports,
                        fee_budget: trade.fee_budget_lamports,
                        caps: &config.spend,
                        skip_if_already_bought: true,
                        skip_if_already_holding: true,
                        now: now_ms(),
                    });
                    if let Ok(retry) = retry {
                        let gate = GateResult { verdict: Verdict::Pass { route: Route::Jupiter, token_program }, ..gate };
                        self.attempt_buy(fire, retry, gate, plan, principal, trace, retries - 1).await;
                    }
                }
                Outcome::Failed { .. } | Outcome::Expired | Outcome::Rejected { .. } | Outcome::Unknown { .. } => {}
            }
            true
        })
    }

    async fn graduated(&self, mint: &str) -> bool {
        let Ok(mint) = mint.parse::<Pubkey>() else {
            return false;
        };
        let accounts = self.d.network.get_accounts(&[bonding_curve_address(&mint).to_string()]).await.unwrap_or_default();
        accounts.into_iter().next().flatten().and_then(|a| parse_curve(&a.data)).is_some_and(|c| c.complete)
    }

    async fn build_buy(
        &self,
        rule: &Rule,
        mint: &str,
        lamports: i128,
        route: Route,
        token_program: Pubkey,
        plan: &FeePlan,
    ) -> Result<Built, String> {
        let wallet = &self.d.wallet;
        let mint_key: Pubkey = mint.parse().map_err(|_| "not a Solana address".to_string())?;
        match (route, self.d.cache.fee_tiers()) {
            (Route::Pump(curve), Some(tiers)) => {
                let tokens = tokens_for_lamports(&curve, lamports as u64, curve_fee_bps(&tiers, &curve));
                let min_out = with_slippage(tokens, rule.fees.slippage_pct);
                self.compile_local(rule, plan, buy_instructions(&wallet.pubkey(), &mint_key, &token_program, lamports as u64, min_out))
            }
            (Route::Pump(_) | Route::Pumpportal(_), _) => {
                let body = json!({
                    "publicKey": wallet.pubkey().to_string(), "action": "buy", "mint": mint, "amount": lamports as f64 / 1e9,
                    "denominatedInSol": "true", "slippage": rule.fees.slippage_pct, "priorityFee": plan.priority_lamports as f64 / 1e9, "pool": "auto"
                });
                let bytes = self.d.network.pumpportal_tx(body).await.map_err(|e| e.0)?;
                self.adopt_pumpportal(rule, plan, &bytes)
            }
            (Route::Jupiter, _) => self.jupiter_built(WSOL, mint, lamports as u64).await,
        }
    }

    /// Sells the whole position in a mint.
    pub async fn sell(&self, mint: &str, reason: &str) -> SellResult {
        let TraderDeps { book, dry_run, network, config, wallet, cache, .. } = &self.d;
        let Some(position) = book.position(mint) else {
            return SellResult::NoPosition;
        };
        let route = if *dry_run { TradeRoute::DryRun } else { TradeRoute::Pump };
        let trade = match book.reserve_sell(mint, position.trigger.clone(), route, now_ms()) {
            Ok(trade) => trade,
            Err(why) => {
                log("exit", &format!("{}: not selling now ({why})", short(mint)));
                return if why == "no position" { SellResult::NoPosition } else { SellResult::Busy };
            }
        };
        if *dry_run {
            log("dry run", &format!("would sell {} ({reason})", short(mint)));
            if let Err(error) = book.confirm_dry_run(&trade.id, now_ms()) {
                log("error", &error.0);
            }
            return SellResult::Sold;
        }
        let mut trace = Trace::new(mint, &format!("exit:{reason}"), "-", reason, None, now_ms(), Instant::now());
        trace.decided = Some(trace.received);
        let built: Result<Built, String> = async {
            let tokens = match &position.token_amount {
                Some(amount) => amount.parse::<u64>().unwrap_or(0),
                None => network.token_balance(&wallet.pubkey().to_string(), mint).await.map_err(|e| e.0)?,
            };
            if tokens == 0 {
                return Err("token balance not known yet".to_string());
            }
            let rule = config.rules.iter().find(|r| r.name == position.trigger.rule).unwrap_or(&config.rules[0]);
            let plan = self.fee_plan(rule);
            let mint_key: Pubkey = mint.parse().map_err(|_| "not a Solana address".to_string())?;
            let accounts = network.get_accounts(&[bonding_curve_address(&mint_key).to_string()]).await.map_err(|e| e.0)?;
            let curve = accounts.into_iter().next().flatten().and_then(|a| parse_curve(&a.data));
            match (curve, cache.fee_tiers()) {
                (Some(curve), Some(tiers)) if local_build_blocker(&curve).is_none() && config.execution.pump_builder == PumpBuilder::Local => {
                    let min_out = with_slippage(lamports_for_tokens(&curve, tokens, curve_fee_bps(&tiers, &curve)), config.exits.sell_slippage_pct);
                    let program: Pubkey = position.token_program.parse().map_err(|_| "token program is not a pubkey".to_string())?;
                    self.compile_local(rule, &plan, vec![sell_instruction(&wallet.pubkey(), &mint_key, &program, tokens, min_out)])
                }
                (Some(curve), _) if !curve.complete => {
                    let body = json!({
                        "publicKey": wallet.pubkey().to_string(), "action": "sell", "mint": mint, "amount": "100%", "denominatedInSol": "false",
                        "slippage": config.exits.sell_slippage_pct, "priorityFee": plan.priority_lamports as f64 / 1e9, "pool": "auto"
                    });
                    let bytes = network.pumpportal_tx(body).await.map_err(|e| e.0)?;
                    self.adopt_pumpportal(rule, &plan, &bytes)
                }
                _ => self.jupiter_built(mint, WSOL, tokens).await,
            }
        }
        .await;
        let built = match built {
            Ok(built) => built,
            Err(error) => {
                book.fail_before_send(&trade.id, &error);
                log("exit", &format!("{}: sell not sent: {error}", short(mint)));
                return SellResult::NotSent;
            }
        };
        trace.signed = Some(Instant::now());
        let Some(outcome) = self.send_recorded(&trade, built, &mut trace).await else {
            return SellResult::NotSent;
        };
        if let Err(error) = book.resolve(&trade.id, outcome.clone(), now_ms()) {
            log("error", &error.0);
        }
        let finished = match outcome.error() {
            Some(error) => format!("sell {}: {error}", outcome.kind()),
            None => format!("sell {}", outcome.kind()),
        };
        self.d.report.finish(&trace, &finished);
        match outcome {
            Outcome::Landed { .. } => SellResult::Sold,
            Outcome::Failed { .. } | Outcome::Expired | Outcome::Rejected { .. } | Outcome::Unknown { .. } => SellResult::Sent,
        }
    }

    /// Exits a mint: sold now if there is a position, kept on the buy if the buy is pending.
    pub async fn exit(&self, mint: &str, code: &str, detail: &str) {
        match self.d.book.add_exit_intent(mint, code, detail, now_ms()) {
            ExitTarget::Pending => {
                log("exit", &format!("{}: {code} recorded on the pending buy; it sells when the buy lands", short(mint)))
            }
            ExitTarget::Position => {
                self.sell(mint, code).await;
            }
            ExitTarget::None => {}
        }
    }

    async fn run_intents(&self, mint: &str, intents: Vec<ExitIntent>) {
        if intents.is_empty() {
            return;
        }
        let codes: Vec<&str> = intents.iter().map(|i| i.code.as_str()).collect();
        self.sell(mint, &codes.join(", ")).await;
    }

    /// Writes `sending` (signature, validity window, signed bytes) to disk before anything goes out.
    /// Returns `None`, with the trade failed and nothing sent, when that write fails.
    async fn send_recorded(&self, trade: &Trade, built: Built, trace: &mut Trace) -> Option<Outcome> {
        let base64 = base64::engine::general_purpose::STANDARD.encode(&built.signed.bytes);
        let mut names = Vec::new();
        for target in &built.targets {
            names.push(target.name.clone());
        }
        let sent = Sent {
            signature: built.signed.signature(),
            last_valid_block_height: built.last_valid_block_height,
            tx: base64.clone(),
            route: built.route,
            senders: names,
        };
        if let Err(error) = self.d.book.mark_sending(&trade.id, sent, now_ms()) {
            self.d.book.fail_before_send(&trade.id, &format!("state write failed, not sent: {error}"));
            log("error", &format!("{}: state write failed, transaction not sent: {error}", short(&trade.mint)));
            return None;
        }
        self.inflight.lock().unwrap_or_else(|p| p.into_inner()).insert(trade.id.clone());
        let outcome = match &built.request_id {
            Some(request_id) => self.jupiter_execute(&built, &base64, request_id, trace).await,
            None => self.land_local(built, trace).await,
        };
        self.inflight.lock().unwrap_or_else(|p| p.into_inner()).remove(&trade.id);
        Some(outcome)
    }

    async fn land_local(&self, built: Built, trace: &mut Trace) -> Outcome {
        let Some(lander) = &self.d.lander else {
            return Outcome::Unknown { error: "no lander in dry run".to_string() };
        };
        if built.targets.is_empty() {
            return Outcome::Unknown { error: "no sender can take this transaction".to_string() };
        }
        let landing = lander.land(&built.signed, built.last_valid_block_height, built.targets).await;
        trace.sent = Some(landing.sent);
        trace.first_ack = landing.first_ack;
        trace.first_ack_by = landing.first_ack_by.clone();
        trace.confirmed = landing.confirmed;
        match landing.status {
            Status::Unknown(error) => Outcome::Unknown { error },
            Status::Confirmed => {
                Outcome::Landed { token_amount: None, spent_lamports: self.debit(&landing.signature).await, signature: None }
            }
            Status::Failed(error) => Outcome::Failed { error, spent_lamports: self.debit(&landing.signature).await },
        }
    }

    /// Jupiter lands the transaction itself. Code 0 is success; the rejection codes refuse it
    /// before landing; any other code (-1000/-2000 "failed to land", -1001/-2001 "unknown error"), a
    /// timeout, or an unreadable answer proves nothing either way.
    async fn jupiter_execute(&self, built: &Built, base64: &str, request_id: &str, trace: &mut Trace) -> Outcome {
        trace.sent = Some(Instant::now());
        let timeout = Duration::from_millis(self.d.config.execution.confirm_timeout_ms);
        let result = match self.d.network.jupiter_execute(base64, request_id, timeout).await {
            Ok(result) => result,
            Err(error) => return Outcome::Unknown { error: error.0 },
        };
        let answered = Instant::now();
        (trace.first_ack, trace.confirmed, trace.first_ack_by) = (Some(answered), Some(answered), Some("jupiter".to_string()));
        let error = format!("Jupiter code {}{}", result.code, result.error.as_ref().map(|e| format!(": {e}")).unwrap_or_default());
        if result.status == "Success" && result.code == 0 {
            let signature = result.signature.unwrap_or_else(|| built.signed.signature());
            let tokens =
                result.total_output_amount.as_deref().and_then(|t| t.parse::<u64>().ok()).filter(|_| built.route == TradeRoute::Jupiter);
            return Outcome::Landed { token_amount: tokens, spent_lamports: self.debit(&signature).await, signature: Some(signature) };
        }
        if JUPITER_REJECTED.contains(&result.code) { Outcome::Rejected { error } } else { Outcome::Unknown { error } }
    }

    async fn debit(&self, signature: &str) -> Option<i128> {
        self.d.network.transaction_debit(signature, &self.d.wallet.pubkey().to_string()).await.ok().flatten()
    }

    /// Fails every trade that was reserved but never sent before a restart, then reconciles now and
    /// every 2 seconds. Abort the returned handle to stop.
    pub fn start(self: &Arc<Self>) -> JoinHandle<()> {
        for trade in self.d.book.recover_after_restart() {
            let side = match trade.side {
                Side::Buy => "buy",
                Side::Sell => "sell",
            };
            log("state", &format!("trade {} ({side} {}) was never sent; released", trade.id, short(&trade.mint)));
        }
        let trader = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(2));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                trader.reconcile().await;
            }
        })
    }

    /// Finishes trades whose outcome is not known yet, rebroadcasts the ones still valid, and reads
    /// token balances that are still missing.
    pub async fn reconcile(&self) {
        let TraderDeps { book, network, cache, .. } = &self.d;
        for trade in book.unresolved() {
            let Some(signature) = trade.signature.clone() else { continue };
            if self.inflight.lock().unwrap_or_else(|p| p.into_inner()).contains(&trade.id) {
                continue;
            }
            let Ok(status) = network.signature_status(&signature).await else {
                continue;
            };
            let height = cache.block_height();
            let outcome = match (&status, height, trade.last_valid_block_height) {
                (Some(status), _, _) if status.confirmed => match &status.err {
                    Some(err) => Some(Outcome::Failed { error: err.to_string(), spent_lamports: self.debit(&signature).await }),
                    None => Some(Outcome::Landed { token_amount: None, spent_lamports: self.debit(&signature).await, signature: None }),
                },
                (None, Some(height), Some(last_valid)) if height > last_valid => Some(Outcome::Expired),
                _ => {
                    if let (Some(tx), Some(lander)) = (&trade.tx, &self.d.lander) {
                        let names = trade.senders.clone().unwrap_or_default();
                        let targets = self.d.senders.iter().filter(|s| names.contains(&s.name)).cloned().collect();
                        lander.rebroadcast(tx.clone(), targets).await;
                    }
                    None
                }
            };
            let Some(outcome) = outcome else { continue };
            let kind = outcome.kind();
            let landed = matches!(outcome, Outcome::Landed { .. });
            let intents = match book.resolve(&trade.id, outcome, now_ms()) {
                Ok(intents) => intents,
                Err(error) => {
                    log("error", &error.0);
                    continue;
                }
            };
            let side = match trade.side {
                Side::Buy => "buy",
                Side::Sell => "sell",
            };
            log("reconcile", &format!("{side} {} {}: {kind}", short(&trade.mint), &signature[..signature.len().min(8)]));
            if trade.side == Side::Buy && landed {
                let owner = network
                    .get_accounts(std::slice::from_ref(&trade.mint))
                    .await
                    .ok()
                    .and_then(|a| a.into_iter().next().flatten())
                    .map(|a| a.owner);
                book.open_position(Position {
                    mint: trade.mint.clone(),
                    route: if trade.route == TradeRoute::Jupiter { PositionRoute::Jupiter } else { PositionRoute::Pump },
                    token_program: owner.unwrap_or_default(),
                    opened_at: now_ms(),
                    buy_trade_id: trade.id.clone(),
                    cost_lamports: trade.reserved_lamports - trade.fee_budget_lamports,
                    token_amount: None,
                    trigger: trade.trigger.clone(),
                    sell_trade_id: None,
                });
                self.run_intents(&trade.mint, intents).await;
            }
        }
        let wallet = self.d.wallet.pubkey().to_string();
        for position in book.positions() {
            if position.token_amount.is_some() {
                continue;
            }
            let balance = network.token_balance(&wallet, &position.mint).await.unwrap_or(0);
            if balance > 0 {
                book.set_token_amount(&position.mint, balance);
            }
        }
    }

    fn fee_plan(&self, rule: &Rule) -> FeePlan {
        let TraderDeps { cache, config, senders, .. } = &self.d;
        let units = i128::from(config.execution.compute_unit_limit);
        let priority_lamports = match rule.fees.priority_fee_sol {
            FeeSetting::Sol(sol) => sol_to_lamports(sol),
            FeeSetting::Auto(_) => {
                let estimate = cache.priority_micro_lamports().map_or(0, |micro| i128::from(micro) * units / 1_000_000);
                estimate.min(sol_to_lamports(config.execution.max_auto_priority_fee_sol))
            }
        };
        let jito_usable = rule.fees.anti_mev && senders.iter().any(|s| s.kind == SenderKind::Jito);
        let tip_sol = match rule.fees.tip_sol {
            FeeSetting::Sol(sol) => sol,
            FeeSetting::Auto(_) => cache.tip_floor_sol().unwrap_or(0.0).min(config.execution.max_auto_tip_sol),
        };
        let jito_tip = if jito_usable { sol_to_lamports(tip_sol).max(JITO_MIN_TIP) } else { 0 };
        let mut sender_tips = Vec::new();
        if !rule.fees.anti_mev {
            let mut rng = rand::rng();
            for sender in &config.senders {
                if sender.tip_accounts.is_empty() || sender.tip_sol <= 0.0 {
                    continue;
                }
                let Ok(account) = sender.tip_accounts[rng.random_range(0..sender.tip_accounts.len())].parse() else { continue };
                sender_tips.push(SenderTip { name: sender.name.clone(), account, lamports: sol_to_lamports(sender.tip_sol) });
            }
        }
        FeePlan { priority_lamports, jito_tip, sender_tips }
    }

    /// With `antiMev`: Jito only, and only when the transaction carries its tip. Otherwise every RPC,
    /// plus each tip-requiring sender whose tip the transaction carries.
    fn targets_for(&self, rule: &Rule, plan: &FeePlan, tipped: bool) -> Vec<SenderTarget> {
        let mut targets = Vec::new();
        if rule.fees.anti_mev {
            if tipped && plan.jito_tip > 0 {
                targets.extend(self.d.senders.iter().filter(|s| s.kind == SenderKind::Jito).cloned());
            }
            return targets;
        }
        let paid: HashSet<&str> = if tipped { plan.sender_tips.iter().map(|t| t.name.as_str()).collect() } else { HashSet::new() };
        targets.extend(
            self.d.senders.iter().filter(|s| s.kind == SenderKind::Rpc && (!s.needs_tip || paid.contains(s.name.as_str()))).cloned(),
        );
        targets
    }

    fn tip_instructions(&self, plan: &FeePlan) -> Vec<Instruction> {
        let from = self.d.wallet.pubkey();
        let mut tips = Vec::new();
        if plan.jito_tip > 0 {
            let to = JITO_TIP_ACCOUNTS[rand::rng().random_range(0..JITO_TIP_ACCOUNTS.len())];
            tips.push(solana_system_interface::instruction::transfer(&from, &to, plan.jito_tip as u64));
        }
        for tip in &plan.sender_tips {
            tips.push(solana_system_interface::instruction::transfer(&from, &tip.account, tip.lamports as u64));
        }
        tips
    }

    /// Compute budget, the trade, then every tip, so one signed transaction suits every sender.
    fn compile_local(&self, rule: &Rule, plan: &FeePlan, trade: Vec<Instruction>) -> Result<Built, String> {
        let Some(blockhash) = self.d.cache.fresh_blockhash() else {
            return Err("no fresh blockhash (the background refresh is failing)".to_string());
        };
        let hash: Hash = blockhash.blockhash.parse().map_err(|_| "cached blockhash is not base58".to_string())?;
        let units = self.d.config.execution.compute_unit_limit;
        let mut instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(units),
            ComputeBudgetInstruction::set_compute_unit_price((plan.priority_lamports * 1_000_000 / i128::from(units)) as u64),
        ];
        instructions.extend(trade);
        instructions.extend(self.tip_instructions(plan));
        let signed = compile_and_sign(&self.d.wallet, &instructions, hash)?;
        Ok(Built {
            signed,
            last_valid_block_height: blockhash.last_valid_block_height,
            route: TradeRoute::Pump,
            targets: self.targets_for(rule, plan, true),
            request_id: None,
        })
    }

    /// PumpPortal builds the trade. When its message uses no lookup tables we add our tips and our
    /// fresh blockhash, which also gives a known validity window. Otherwise it goes out unchanged,
    /// to senders that need no tip, with a window of 150 blocks from now as an upper bound.
    fn adopt_pumpportal(&self, rule: &Rule, plan: &FeePlan, bytes: &[u8]) -> Result<Built, String> {
        let original: VersionedTransaction =
            wincode::deserialize(bytes).map_err(|e| format!("PumpPortal sent an unreadable transaction: {e}"))?;
        let fresh = self.d.cache.fresh_blockhash();
        let no_lookups = original.message.address_table_lookups().is_none_or(|l| l.is_empty());
        if let (true, Some(blockhash)) = (no_lookups, fresh) {
            let hash: Hash = blockhash.blockhash.parse().map_err(|_| "cached blockhash is not base58".to_string())?;
            let mut instructions = decompile(&original.message);
            instructions.extend(self.tip_instructions(plan));
            let signed = compile_and_sign(&self.d.wallet, &instructions, hash)?;
            let targets = self.targets_for(rule, plan, true);
            return Ok(Built {
                signed,
                last_valid_block_height: blockhash.last_valid_block_height,
                route: TradeRoute::Pumpportal,
                targets,
                request_id: None,
            });
        }
        let Some(height) = self.d.cache.block_height() else {
            return Err("block height unknown".to_string());
        };
        let VersionedTransaction { signatures, message } = original;
        let signed = sign(message, signatures, &self.d.wallet)?;
        Ok(Built {
            signed,
            last_valid_block_height: height + 150,
            route: TradeRoute::Pumpportal,
            targets: self.targets_for(rule, plan, false),
            request_id: None,
        })
    }

    /// Jupiter Swap V2 `/order` with the wallet as taker. Jupiter lands it through `/execute`, so the
    /// bot's own senders are not used. The order's `lastValidBlockHeight` is the hard expiry.
    async fn jupiter_built(&self, input_mint: &str, output_mint: &str, amount: u64) -> Result<Built, String> {
        let request = OrderRequest {
            input_mint: input_mint.to_string(),
            output_mint: output_mint.to_string(),
            amount,
            taker: self.d.wallet.pubkey().to_string(),
        };
        let order = self.d.network.jupiter_order(request).await.map_err(|e| e.0)?;
        let Some(transaction) = order.transaction else {
            return Err(format!(
                "Jupiter /order returned no transaction: {}",
                order.error_message.unwrap_or_else(|| "no route".to_string())
            ));
        };
        let last_valid =
            order.last_valid_block_height.or_else(|| self.d.cache.block_height().map(|h| h + 150)).ok_or("block height unknown")?;
        let bytes =
            base64::engine::general_purpose::STANDARD.decode(transaction).map_err(|e| format!("Jupiter /order transaction: {e}"))?;
        let tx: VersionedTransaction = wincode::deserialize(&bytes).map_err(|e| format!("Jupiter /order transaction: {e}"))?;
        let VersionedTransaction { signatures, message } = tx;
        let signed = sign(message, signatures, &self.d.wallet)?;
        Ok(Built {
            signed,
            last_valid_block_height: last_valid,
            route: TradeRoute::Jupiter,
            targets: Vec::new(),
            request_id: Some(order.request_id),
        })
    }

    fn drop_fire(&self, fire: &Fire, reason: &str, trace: &Trace) {
        (self.d.release_fire)(fire);
        let rule = &self.d.config.rules[fire.rule];
        log("skip", &format!("{} {}: {reason}", rule.name, short(&fire.mint)));
        let code = reason.split(':').next().unwrap_or(reason);
        self.d.report.finish(trace, &format!("skipped ({code})"));
    }
}

fn plan_cost(plan: &FeePlan) -> i128 {
    let mut cost = SIGNATURE_FEE + plan.priority_lamports + plan.jito_tip;
    for tip in &plan.sender_tips {
        cost += tip.lamports;
    }
    cost
}

/// Turns a message without lookup tables back into instructions, with signer and writable flags
/// from its header.
fn decompile(message: &VersionedMessage) -> Vec<Instruction> {
    let keys = message.static_account_keys();
    let header = message.header();
    let signed = usize::from(header.num_required_signatures);
    let writable = |i: usize| {
        if i < signed {
            i < signed - usize::from(header.num_readonly_signed_accounts)
        } else {
            i < keys.len() - usize::from(header.num_readonly_unsigned_accounts)
        }
    };
    let mut out = Vec::new();
    for compiled in message.instructions() {
        let mut accounts = Vec::new();
        for &index in &compiled.accounts {
            let i = usize::from(index);
            accounts.push(AccountMeta { pubkey: keys[i], is_signer: i < signed, is_writable: writable(i) });
        }
        out.push(Instruction { program_id: keys[usize::from(compiled.program_id_index)], accounts, data: compiled.data.clone() });
    }
    out
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
