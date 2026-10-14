//! Safety checks and rule filters for one fired rule, all started together under one time budget.
//! The only read every buy needs is one `getMultipleAccounts` for the mint and its pump.fun curve;
//! it decides the route as well. Next: `execute/build.rs` builds and signs the transaction.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use solana_pubkey::Pubkey;

use crate::detect::types::Signal;
use crate::execute::pump::{
    Curve, TOKEN_2022_PROGRAM, TOKEN_PROGRAM, bonding_curve_address, local_build_blocker, parse_curve, warm_buy_accounts,
};
use crate::network::{AccountData, MarketSnapshot, NetResult, Network, RugcheckSummary};
use crate::setup::config::{OnMissing, PumpBuilder, Rugcheck, Rule, Safety};

/// How the buy is built and sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Built locally on this live pump.fun curve.
    Pump(Curve),
    /// Built by PumpPortal on this live curve (cashback or non-SOL curves, or by config).
    Pumpportal(Curve),
    /// Swapped through Jupiter.
    Jupiter,
}

/// What the checks learned about the token.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    /// Venue.
    pub platform: Option<String>,
    /// Market cap.
    pub market_cap_usd: Option<f64>,
    /// Liquidity.
    pub liquidity_usd: Option<f64>,
    /// Age in seconds.
    pub age_sec: Option<f64>,
    /// Highest transfer fee the mint can charge, in percent.
    pub tax_pct: Option<f64>,
    /// The safety service flagged dangers.
    pub suspicious: Option<bool>,
}

/// Why the gate rejected a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateCode {
    /// A check failed or timed out.
    CheckUnknown,
    /// The address is not a token mint.
    NotAMint,
    /// The supply can still be increased.
    MintAuthority,
    /// Holder accounts can be frozen.
    FreezeAuthority,
    /// A risky Token-2022 extension.
    RiskyExtension,
    /// Rugcheck reports a danger.
    RugcheckDanger,
    /// Rugcheck score too high.
    RugcheckScore,
    /// Venue not allowed.
    FilterPlatform,
    /// Transfer fee too high.
    FilterTax,
    /// Market cap out of range.
    FilterMarketCap,
    /// Liquidity too low.
    FilterLiquidity,
    /// Age out of range.
    FilterAge,
    /// Flagged as suspicious.
    FilterSuspicious,
    /// A filter needed data that did not arrive.
    MissingData,
}

impl GateCode {
    /// Returns the stable code.
    pub fn as_str(self) -> &'static str {
        match self {
            GateCode::CheckUnknown => "check_unknown",
            GateCode::NotAMint => "not_a_mint",
            GateCode::MintAuthority => "mint_authority",
            GateCode::FreezeAuthority => "freeze_authority",
            GateCode::RiskyExtension => "risky_extension",
            GateCode::RugcheckDanger => "rugcheck_danger",
            GateCode::RugcheckScore => "rugcheck_score",
            GateCode::FilterPlatform => "filter_platform",
            GateCode::FilterTax => "filter_tax",
            GateCode::FilterMarketCap => "filter_market_cap",
            GateCode::FilterLiquidity => "filter_liquidity",
            GateCode::FilterAge => "filter_age",
            GateCode::FilterSuspicious => "filter_suspicious",
            GateCode::MissingData => "missing_data",
        }
    }
}

/// The gate's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// The token passed; trade it this way.
    Pass {
        /// How to build the trade.
        route: Route,
        /// The mint's token program.
        token_program: Pubkey,
    },
    /// The token failed a check.
    Fail {
        /// Stable code.
        code: GateCode,
        /// Free text for logs.
        detail: String,
    },
}

/// The verdict, the facts behind it, and how long the checks took.
#[derive(Debug, Clone, PartialEq)]
pub struct GateResult {
    /// Pass or fail.
    pub verdict: Verdict,
    /// What the checks learned.
    pub facts: Facts,
    /// Wall time of the checks.
    pub elapsed: Duration,
}

/// What the gate reads besides the network.
#[derive(Debug, Clone)]
pub struct GateDeps<'a> {
    /// Safety settings.
    pub safety: &'a Safety,
    /// Where pump.fun buys are built.
    pub pump_builder: PumpBuilder,
    /// Cached SOL price, refreshed in the background.
    pub sol_usd: Option<f64>,
    /// The current epoch, which decides which Token-2022 transfer fee applies.
    pub epoch: Option<u64>,
    /// The wallet, so its token accounts are derived while the read is in flight.
    pub wallet: Pubkey,
}

/// Token-2022 extension types (spl-token-2022 `ExtensionType`) that let someone other than the
/// holder block, take, or tax transfers.
fn risky_extension(kind: u16) -> Option<&'static str> {
    match kind {
        6 => Some("default account state (accounts start frozen)"),
        9 => Some("non-transferable"),
        12 => Some("permanent delegate (someone can move your tokens)"),
        14 => Some("transfer hook (custom code runs on every transfer)"),
        26 => Some("pausable"),
        _ => None,
    }
}

/// What the gate reads from a mint account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintInfo {
    /// A mint authority is set.
    pub mint_authority: bool,
    /// A freeze authority is set.
    pub freeze_authority: bool,
    /// Raw supply.
    pub supply: u64,
    /// Decimals.
    pub decimals: u8,
    /// Token-2022 extension types, in account order.
    pub extensions: Vec<u16>,
    /// The Token-2022 transfer fee, when the mint has one.
    pub transfer_fee: Option<TransferFee>,
}

/// One setting of a Token-2022 transfer fee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeSetting {
    /// First epoch it applies.
    pub epoch: u64,
    /// Basis points.
    pub bps: u16,
}

/// A Token-2022 transfer fee has an older and a newer setting; the newer one applies from its epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferFee {
    /// The setting before `newer.epoch`.
    pub older: FeeSetting,
    /// The setting from `newer.epoch` on.
    pub newer: FeeSetting,
}

/// Returns the transfer fee in force at `epoch` in basis points; with no epoch known, the higher of
/// the two.
pub fn transfer_fee_bps(fee: Option<&TransferFee>, epoch: Option<u64>) -> u16 {
    let Some(fee) = fee else {
        return 0;
    };
    match epoch {
        None => fee.older.bps.max(fee.newer.bps),
        Some(epoch) if epoch >= fee.newer.epoch => fee.newer.bps,
        Some(_) => fee.older.bps,
    }
}

fn u64_le(data: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[at..at + 8]);
    u64::from_le_bytes(bytes)
}

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

/// Parses an SPL or Token-2022 mint. The SPL mint is 82 bytes (`COption<Pubkey>` mint authority,
/// u64 supply, u8 decimals, bool, `COption<Pubkey>` freeze authority). Token-2022 adds, after byte
/// 165, an account-type byte and TLV extensions.
///
/// `TransferFeeConfig` (type 1, 108 bytes): two authorities (64 bytes) and the withheld amount
/// (8), then the older and the newer fee, each `{ epoch u64, maximum_fee u64, basis_points u16 }`.
pub fn parse_mint(data: &[u8]) -> Option<MintInfo> {
    if data.len() < 82 {
        return None;
    }
    let mut supply = [0u8; 8];
    supply.copy_from_slice(&data[36..44]);
    let mut info = MintInfo {
        mint_authority: u32_at(data, 0) == 1,
        supply: u64::from_le_bytes(supply),
        decimals: data[44],
        freeze_authority: u32_at(data, 46) == 1,
        extensions: Vec::new(),
        transfer_fee: None,
    };
    if data.len() <= 165 || data[165] != 1 {
        return Some(info);
    }
    let mut at = 166;
    while at + 4 <= data.len() {
        let kind = u16_at(data, at);
        let length = usize::from(u16_at(data, at + 2));
        if kind == 0 {
            break;
        }
        info.extensions.push(kind);
        if kind == 1 && length >= 108 && at + 4 + 108 <= data.len() {
            let value = at + 4;
            let older = FeeSetting { epoch: u64_le(data, value + 72), bps: u16_at(data, value + 88) };
            let newer = FeeSetting { epoch: u64_le(data, value + 90), bps: u16_at(data, value + 106) };
            info.transfer_fee = Some(TransferFee { older, newer });
        }
        at += 4 + length;
    }
    Some(info)
}

fn needs_market_data(rule: &Rule) -> bool {
    let f = &rule.filters;
    f.min_token_age_sec.is_some()
        || f.max_token_age_sec.is_some()
        || f.min_market_cap_usd.is_some()
        || f.max_market_cap_usd.is_some()
        || f.min_liquidity_usd.is_some()
        || !f.platforms.is_empty()
}

/// The outcome of one check: its value, or why it has none ("timed out" or the error).
type Settled<T> = Result<T, String>;

async fn settle<T>(budget: Duration, read: impl Future<Output = NetResult<T>>) -> Settled<T> {
    match tokio::time::timeout(budget, read).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(error.0),
        Err(_) => Err("timed out".to_string()),
    }
}

/// Runs the chain read, safety checks, and rule filters for one mint under `safety.budgetMs`.
pub async fn run_gate<N: Network>(network: &N, mint: &Pubkey, rule: &Rule, signal: Option<&Signal>, deps: &GateDeps<'_>) -> GateResult {
    let started = Instant::now();
    let budget = Duration::from_millis(deps.safety.budget_ms);
    let mint_text = mint.to_string();
    let addresses = [mint_text.clone(), bonding_curve_address(mint).to_string()];
    let accounts = settle(budget, network.get_accounts(&addresses));
    let market = async { if needs_market_data(rule) { Some(settle(budget, network.market_snapshot(&mint_text)).await) } else { None } };
    let rug = async {
        match deps.safety.rugcheck {
            Rugcheck::Off => None,
            Rugcheck::Advisory | Rugcheck::Required => Some(settle(budget, network.rugcheck(&mint_text)).await),
        }
    };
    let warm = async {
        tokio::task::yield_now().await;
        warm_buy_accounts(mint, &deps.wallet);
    };
    let (accounts, market, rug, ()) = tokio::join!(accounts, market, rug, warm);
    let (verdict, facts) = judge(rule, signal, deps, accounts, market, rug);
    GateResult { verdict, facts, elapsed: started.elapsed() }
}

fn judge(
    rule: &Rule,
    signal: Option<&Signal>,
    deps: &GateDeps,
    accounts: Settled<Vec<Option<AccountData>>>,
    market: Option<Settled<MarketSnapshot>>,
    rug: Option<Settled<Option<RugcheckSummary>>>,
) -> (Verdict, Facts) {
    let safety = deps.safety;
    let mut facts = Facts::default();
    let fail = |code: GateCode, detail: String| Verdict::Fail { code, detail };

    let mut route = Route::Jupiter;
    let mut token_program = TOKEN_PROGRAM;
    match accounts {
        Err(error) => {
            if safety.on_unknown == OnMissing::Reject {
                return (fail(GateCode::CheckUnknown, format!("mint read {error} (budget {} ms)", safety.budget_ms)), facts);
            }
        }
        Ok(accounts) => {
            let mut accounts = accounts.into_iter();
            let (mint_account, curve_account) = (accounts.next().flatten(), accounts.next().flatten());
            let Some(mint_account) = mint_account else {
                return (fail(GateCode::NotAMint, "no account at this address yet".to_string()), facts);
            };
            let owner = mint_account.owner.as_str();
            token_program = if owner == TOKEN_PROGRAM.to_string() {
                TOKEN_PROGRAM
            } else if owner == TOKEN_2022_PROGRAM.to_string() {
                TOKEN_2022_PROGRAM
            } else {
                let detail = format!("owned by {owner}, not a token program (a chart link can point at a pool)");
                return (fail(GateCode::NotAMint, detail), facts);
            };
            let Some(info) = parse_mint(&mint_account.data) else {
                return (fail(GateCode::NotAMint, "account data is too short for a mint".to_string()), facts);
            };
            let live = curve_account.and_then(|a| parse_curve(&a.data)).filter(|c| !c.complete);
            if live.is_none() {
                if safety.reject_mint_authority && info.mint_authority {
                    return (fail(GateCode::MintAuthority, "the supply can still be increased".to_string()), facts);
                }
                if safety.reject_freeze_authority && info.freeze_authority {
                    return (fail(GateCode::FreezeAuthority, "holder accounts can be frozen".to_string()), facts);
                }
                let risky = info.extensions.iter().find_map(|kind| risky_extension(*kind));
                if let Some(risky) = risky.filter(|_| safety.reject_risky_extensions) {
                    return (fail(GateCode::RiskyExtension, risky.to_string()), facts);
                }
            }
            facts.tax_pct = Some(f64::from(transfer_fee_bps(info.transfer_fee.as_ref(), deps.epoch)) / 100.0);
            if let Some(live) = live {
                let portal = local_build_blocker(&live).is_some() || deps.pump_builder == PumpBuilder::Pumpportal;
                route = if portal { Route::Pumpportal(live) } else { Route::Pump(live) };
                facts.platform = Some("pumpfun".to_string());
                if let Some(sol_usd) = deps.sol_usd.filter(|_| live.sol_paired) {
                    let cap_lamports = u128::from(live.virtual_quote_reserves) * u128::from(live.token_total_supply)
                        / u128::from(live.virtual_token_reserves.max(1));
                    facts.market_cap_usd = Some(cap_lamports as f64 / 1e9 * sol_usd);
                    facts.liquidity_usd = Some(live.real_quote_reserves as f64 / 1e9 * sol_usd);
                }
            }
        }
    }
    if let Some(Ok(snapshot)) = &market {
        facts.platform = facts.platform.take().or_else(|| snapshot.platform.clone());
        facts.market_cap_usd = facts.market_cap_usd.or(snapshot.market_cap_usd);
        facts.liquidity_usd = facts.liquidity_usd.or(snapshot.liquidity_usd);
        if let Some(created) = snapshot.created_at_ms {
            facts.age_sec = Some(((now_ms() - created) / 1000.0).max(0.0));
        }
    }
    if let Some(cap) = signal.and_then(|s| s.market_cap_usd) {
        facts.market_cap_usd = Some(cap);
    }
    if let Some(Ok(Some(report))) = &rug {
        facts.suspicious = Some(!report.dangers.is_empty());
    }
    if let Some(failed) = filter_failure(rule, deps, &facts, rug.as_ref()) {
        return (failed, facts);
    }
    (Verdict::Pass { route, token_program }, facts)
}

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

/// Applies the rule's filters and the Rugcheck result to the facts, in a fixed order.
fn filter_failure(rule: &Rule, deps: &GateDeps, facts: &Facts, rug: Option<&Settled<Option<RugcheckSummary>>>) -> Option<Verdict> {
    let f = &rule.filters;
    let safety = deps.safety;
    let fail = |code: GateCode, detail: String| Some(Verdict::Fail { code, detail });
    let missing = |filter: &str, what: &str| match f.on_missing_data {
        OnMissing::Reject => fail(GateCode::MissingData, format!("{filter} needs {what}, and none arrived within {} ms", safety.budget_ms)),
        OnMissing::Allow => None,
    };
    let range = |value: Option<f64>, min: Option<f64>, max: Option<f64>, code: GateCode, name: &str, unit: &str| {
        if min.is_none() && max.is_none() {
            return None;
        }
        let Some(value) = value else {
            return missing(name, unit);
        };
        if let Some(min) = min.filter(|min| value < *min) {
            return fail(code, format!("{unit} {} is below {min}", value.round()));
        }
        if let Some(max) = max.filter(|max| value > *max) {
            return fail(code, format!("{unit} {} is above {max}", value.round()));
        }
        None
    };
    if !f.platforms.is_empty() {
        match &facts.platform {
            None => {
                if let Some(failed) = missing("platforms", "the venue") {
                    return Some(failed);
                }
            }
            Some(platform) if !f.platforms.contains(platform) => {
                return fail(GateCode::FilterPlatform, format!("venue {platform} is not in [{}]", f.platforms.join(", ")));
            }
            Some(_) => {}
        }
    }
    let checks = [
        range(facts.tax_pct, None, f.max_pool_tax_pct, GateCode::FilterTax, "maxPoolTaxPct", "transfer fee %"),
        range(
            facts.market_cap_usd,
            f.min_market_cap_usd,
            f.max_market_cap_usd,
            GateCode::FilterMarketCap,
            "market cap filter",
            "market cap USD",
        ),
        range(facts.liquidity_usd, f.min_liquidity_usd, None, GateCode::FilterLiquidity, "minLiquidityUsd", "liquidity USD"),
        range(facts.age_sec, f.min_token_age_sec, f.max_token_age_sec, GateCode::FilterAge, "token age filter", "age in seconds"),
    ];
    if let Some(failed) = checks.into_iter().flatten().next() {
        return Some(failed);
    }
    let Some(rug) = rug else {
        return if f.skip_suspicious_pools { missing("skipSuspiciousPools", "a rugcheck report (turn on safety.rugcheck)") } else { None };
    };
    let report = match rug {
        Ok(Some(report)) => report,
        Ok(None) | Err(_) => {
            if safety.rugcheck == Rugcheck::Required && safety.on_unknown == OnMissing::Reject {
                let why = match rug {
                    Err(error) => error.clone(),
                    Ok(_) => "had no report".to_string(),
                };
                return fail(GateCode::CheckUnknown, format!("rugcheck {why}"));
            }
            return if f.skip_suspicious_pools { missing("skipSuspiciousPools", "a rugcheck report") } else { None };
        }
    };
    if !report.dangers.is_empty() {
        let code = if f.skip_suspicious_pools { GateCode::FilterSuspicious } else { GateCode::RugcheckDanger };
        return fail(code, report.dangers.join(", "));
    }
    match (safety.max_rugcheck_score, report.score) {
        (Some(max), Some(score)) if score > max => fail(GateCode::RugcheckScore, format!("score {score} is above {max}")),
        (Some(_), None) if safety.rugcheck == Rugcheck::Required && safety.on_unknown == OnMissing::Reject => {
            fail(GateCode::CheckUnknown, "rugcheck report has no score".to_string())
        }
        _ => None,
    }
}
