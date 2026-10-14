//! Safety checks and rule filters for one fired rule, all started together under one time budget.
//! The only read every buy needs is one `getMultipleAccounts` for the mint and its pump.fun curve;
//! only a live pump.fun curve quoted in SOL passes. Next: `execute/trade.rs` builds and signs the buy.

use std::time::{Duration, Instant};

use crate::log::unix_ms;

use solana_pubkey::Pubkey;

use crate::detect::types::Signal;
use crate::execute::pump::{
    Curve, TOKEN_2022_PROGRAM, TOKEN_PROGRAM, bonding_curve_address, parse_curve, unsupported_curve, warm_buy_accounts,
};
use crate::network::{AccountData, MarketSnapshot, NetResult, Network, RugcheckSummary};
use crate::setup::config::{OnMissing, Rugcheck, Rule, Safety};

/// What the checks learned about the token.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateCode {
    /// A check failed or timed out.
    CheckUnknown,
    /// The address is not a token mint.
    NotAMint,
    /// Rugcheck reports a danger.
    RugcheckDanger,
    /// Rugcheck score too high.
    RugcheckScore,
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
    /// Not a coin on a live pump.fun curve quoted in SOL: graduated, never on a curve, quoted in
    /// another token, or a cashback coin.
    UnsupportedCurve,
}

/// The gate's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum GateVerdict {
    /// The token passed: a live pump.fun curve quoted in SOL.
    Pass {
        /// The curve to buy on.
        curve: Curve,
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
    pub verdict: GateVerdict,
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
    /// Cached SOL price, refreshed in the background.
    pub sol_usd: Option<f64>,
    /// The current epoch, which decides which Token-2022 transfer fee applies.
    pub epoch: Option<u64>,
    /// The wallet, so its token accounts are derived while the read is in flight.
    pub wallet: Pubkey,
}

/// What the gate reads from a mint account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintInfo {
    /// The Token-2022 transfer fee, when the mint has one.
    pub transfer_fee: Option<TransferFee>,
}

/// One setting of a Token-2022 transfer fee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferFeeSetting {
    /// First epoch it applies.
    pub epoch: u64,
    /// Basis points.
    pub bps: u16,
}

/// A Token-2022 transfer fee has an older and a newer setting; the newer one applies from its epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferFee {
    /// The setting before `newer.epoch`.
    pub older: TransferFeeSetting,
    /// The setting from `newer.epoch` on.
    pub newer: TransferFeeSetting,
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

/// Parses an SPL or Token-2022 mint. The SPL mint is 82 bytes. Token-2022 adds, after byte 165, an
/// account-type byte and TLV extensions.
///
/// `TransferFeeConfig` (type 1, 108 bytes): two authorities (64 bytes) and the withheld amount
/// (8), then the older and the newer fee, each `{ epoch u64, maximum_fee u64, basis_points u16 }`.
pub fn parse_mint(data: &[u8]) -> Option<MintInfo> {
    if data.len() < 82 {
        return None;
    }
    let mut info = MintInfo { transfer_fee: None };
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
        if kind == 1 && length >= 108 && at + 4 + 108 <= data.len() {
            let value = at + 4;
            let older = TransferFeeSetting { epoch: u64_le(data, value + 72), bps: u16_at(data, value + 88) };
            let newer = TransferFeeSetting { epoch: u64_le(data, value + 90), bps: u16_at(data, value + 106) };
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
) -> (GateVerdict, Facts) {
    let safety = deps.safety;
    let mut facts = Facts::default();
    let fail = |code: GateCode, detail: String| GateVerdict::Fail { code, detail };

    // The bot buys only on a live pump.fun curve, so without the mint and curve reads there is no buy.
    let accounts = match accounts {
        Ok(accounts) => accounts,
        Err(error) => return (fail(GateCode::CheckUnknown, format!("mint read {error} (budget {} ms)", safety.budget_ms)), facts),
    };
    let mut accounts = accounts.into_iter();
    let (mint_account, curve_account) = (accounts.next().flatten(), accounts.next().flatten());
    let Some(mint_account) = mint_account else {
        return (fail(GateCode::NotAMint, "no account at this address yet".to_string()), facts);
    };
    let owner = mint_account.owner.as_str();
    let token_program = if owner == TOKEN_PROGRAM.to_string() {
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
    let parsed = curve_account.and_then(|a| parse_curve(&a.data));
    // pump.fun creates every curve mint itself, with no mint or freeze authority, so a live curve
    // needs no authority checks.
    let Some(curve) = parsed.filter(|c| !c.complete) else {
        let detail = if parsed.is_some() { "the coin left its pump.fun curve" } else { "not a pump.fun curve coin" };
        return (fail(GateCode::UnsupportedCurve, detail.to_string()), facts);
    };
    if let Some(blocker) = unsupported_curve(&curve) {
        return (fail(GateCode::UnsupportedCurve, blocker.to_string()), facts);
    }
    facts.tax_pct = Some(f64::from(transfer_fee_bps(info.transfer_fee.as_ref(), deps.epoch)) / 100.0);
    if let Some(sol_usd) = deps.sol_usd {
        let cap_lamports = u128::from(curve.virtual_quote_reserves) * u128::from(curve.token_total_supply)
            / u128::from(curve.virtual_token_reserves.max(1));
        facts.market_cap_usd = Some(cap_lamports as f64 / 1e9 * sol_usd);
        facts.liquidity_usd = Some(curve.real_quote_reserves as f64 / 1e9 * sol_usd);
    }
    if let Some(Ok(snapshot)) = &market {
        facts.market_cap_usd = facts.market_cap_usd.or(snapshot.market_cap_usd);
        facts.liquidity_usd = facts.liquidity_usd.or(snapshot.liquidity_usd);
        if let Some(created) = snapshot.created_at_ms {
            facts.age_sec = Some(((unix_ms() as f64 - created) / 1000.0).max(0.0));
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
    (GateVerdict::Pass { curve, token_program }, facts)
}

/// Applies the rule's filters and the Rugcheck result to the facts, in a fixed order.
fn filter_failure(rule: &Rule, deps: &GateDeps, facts: &Facts, rug: Option<&Settled<Option<RugcheckSummary>>>) -> Option<GateVerdict> {
    let f = &rule.filters;
    let safety = deps.safety;
    let fail = |code: GateCode, detail: String| Some(GateVerdict::Fail { code, detail });
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
