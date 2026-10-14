//! Builds pump.fun bonding-curve trades locally, with no HTTP round trip to a trade API, and quotes
//! them with the program's fee tiers. Layouts follow pump-fun/pump-public-docs (`idl/pump.json`,
//! `docs/instructions/TRADE_V3.md`, `docs/FEE_RECIPIENTS.md`, `docs/FEE_PROGRAM_README.md`). Used by
//! `execute/trade.rs`, which signs and sends.

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

use rand::RngExt;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

/// pump.fun program.
pub const PUMP_PROGRAM: Pubkey = Pubkey::from_str_const("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
const PUMP_FEES_PROGRAM: Pubkey = Pubkey::from_str_const("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
/// SPL Token program.
pub const TOKEN_PROGRAM: Pubkey = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// Token-2022 program.
pub const TOKEN_2022_PROGRAM: Pubkey = Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const ATA_PROGRAM: Pubkey = Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// System program.
pub const SYSTEM_PROGRAM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
/// Wrapped SOL mint.
pub const WSOL_MINT: Pubkey = Pubkey::from_str_const("So11111111111111111111111111111111111111112");

/// Any of these 8 wallets is a valid buyback fee recipient for SOL-paired coins
/// (`FEE_RECIPIENTS.md`).
const BUYBACK_FEE_RECIPIENTS: [Pubkey; 8] = [
    Pubkey::from_str_const("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD"),
    Pubkey::from_str_const("9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7"),
    Pubkey::from_str_const("GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL"),
    Pubkey::from_str_const("3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR"),
    Pubkey::from_str_const("5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6"),
    Pubkey::from_str_const("EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL"),
    Pubkey::from_str_const("5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD"),
    Pubkey::from_str_const("A7hAgCzFw14fejgCp387JUJRMNyz4j89JKnhtKU8piqW"),
];

const BUY_EXACT_QUOTE_IN_V3: [u8; 8] = [225, 247, 80, 30, 213, 179, 132, 136];
const SELL_V3: [u8; 8] = [28, 146, 222, 119, 38, 196, 105, 213];
const BONDING_CURVE_ACCOUNT: [u8; 8] = [23, 183, 248, 55, 96, 216, 172, 96];
const FEE_CONFIG_ACCOUNT: [u8; 8] = [143, 52, 146, 187, 219, 123, 76, 155];

fn pda(seeds: &[&[u8]], program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}

static GLOBAL: LazyLock<Pubkey> = LazyLock::new(|| pda(&[b"global"], &PUMP_PROGRAM));
static EVENT_AUTHORITY: LazyLock<Pubkey> = LazyLock::new(|| pda(&[b"__event_authority"], &PUMP_PROGRAM));

/// Returns the pump.fun fee config account, derived from the fees program.
pub fn fee_config() -> Pubkey {
    static FEE_CONFIG: LazyLock<Pubkey> = LazyLock::new(|| pda(&[b"fee_config", PUMP_PROGRAM.as_ref()], &PUMP_FEES_PROGRAM));
    *FEE_CONFIG
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Derived {
    Ata { owner: Pubkey, mint: Pubkey, token_program: Pubkey },
    Curve(Pubkey),
    Volume(Pubkey),
}

const MAX_DERIVED: usize = 50_000;

/// A program address costs a hash and a curve check per bump tried, so every derived address is
/// kept: the same mint is read, bought, and later sold.
#[derive(Default)]
struct DerivedCache {
    addresses: HashMap<Derived, Pubkey>,
    order: VecDeque<Derived>,
}

static DERIVED: LazyLock<Mutex<DerivedCache>> = LazyLock::new(|| Mutex::new(DerivedCache::default()));

fn cached(key: Derived) -> Pubkey {
    let found = DERIVED.lock().unwrap_or_else(|p| p.into_inner()).addresses.get(&key).copied();
    if let Some(address) = found {
        return address;
    }
    let address = match key {
        Derived::Ata { owner, mint, token_program } => pda(&[owner.as_ref(), token_program.as_ref(), mint.as_ref()], &ATA_PROGRAM),
        Derived::Curve(mint) => pda(&[b"bonding-curve", mint.as_ref()], &PUMP_PROGRAM),
        Derived::Volume(user) => pda(&[b"user_volume_accumulator", user.as_ref()], &PUMP_PROGRAM),
    };
    let mut cache = DERIVED.lock().unwrap_or_else(|p| p.into_inner());
    if cache.addresses.insert(key, address).is_none() {
        cache.order.push_back(key);
        if cache.addresses.len() > MAX_DERIVED
            && let Some(oldest) = cache.order.pop_front()
        {
            cache.addresses.remove(&oldest);
        }
    }
    address
}

/// Returns the associated token account of `owner` for `mint` under `token_program`.
pub fn associated_token_address(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    cached(Derived::Ata { owner: *owner, mint: *mint, token_program: *token_program })
}

/// Returns the bonding curve account of a mint.
pub fn bonding_curve_address(mint: &Pubkey) -> Pubkey {
    cached(Derived::Curve(*mint))
}

/// Derives a buy's remaining addresses while the mint read is in flight. New pump.fun coins use
/// Token-2022, so that program is assumed; an older SPL coin derives its own on use.
pub fn warm_buy_accounts(mint: &Pubkey, user: &Pubkey) {
    let curve = bonding_curve_address(mint);
    associated_token_address(user, mint, &TOKEN_2022_PROGRAM);
    associated_token_address(&curve, mint, &TOKEN_2022_PROGRAM);
    associated_token_address(&curve, &WSOL_MINT, &TOKEN_PROGRAM);
}

/// The `BondingCurve` account (`idl/pump.json` `types.BondingCurve`). Older curves have a shorter
/// layout, so fields after `complete` are read only when present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Curve {
    /// Virtual token reserves.
    pub virtual_token_reserves: u64,
    /// Virtual quote reserves, in the quote mint's smallest unit.
    pub virtual_quote_reserves: u64,
    /// Real token reserves.
    pub real_token_reserves: u64,
    /// Real quote reserves.
    pub real_quote_reserves: u64,
    /// Total token supply.
    pub token_total_supply: u64,
    /// The curve finished and the coin moved to an AMM.
    pub complete: bool,
    /// Mayhem mode.
    pub mayhem: bool,
    /// Cashback coin; v3 trades reject it.
    pub cashback: bool,
    /// The quote mint is the default key, which means SOL.
    pub sol_paired: bool,
    /// Account length. Older curves are shorter, and a trade tops their rent up to the full layout.
    pub data_length: usize,
}

fn u64_at(data: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// Parses a bonding curve account, or returns `None` for any other account.
pub fn parse_curve(data: &[u8]) -> Option<Curve> {
    if data.len() < 49 || data[..8] != BONDING_CURVE_ACCOUNT {
        return None;
    }
    let quote_mint = if data.len() >= 115 { Some(&data[83..115]) } else { None };
    let sol_paired = quote_mint.is_none_or(|q| q.iter().all(|b| *b == 0));
    Some(Curve {
        virtual_token_reserves: u64_at(data, 8),
        virtual_quote_reserves: u64_at(data, 16),
        real_token_reserves: u64_at(data, 24),
        real_quote_reserves: u64_at(data, 32),
        token_total_supply: u64_at(data, 40),
        complete: data[48] == 1,
        mayhem: data.len() > 81 && data[81] == 1,
        cashback: data.len() > 82 && data[82] == 1,
        sol_paired,
        data_length: data.len(),
    })
}

/// Returns why the local builder cannot trade this live curve: it must be quoted in SOL, and
/// cashback coins reject v3 trades (error 6094).
pub fn unsupported_curve(curve: &Curve) -> Option<&'static str> {
    if !curve.sol_paired {
        return Some("the curve is quoted in a token other than SOL");
    }
    if curve.cashback {
        return Some("cashback coin: v3 trades are not supported");
    }
    None
}

/// One pump.fun fee tier: from this market cap (in lamports) up, these fees apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeTier {
    /// Lowest market cap of the tier, in lamports.
    pub threshold: u128,
    /// Protocol fee in basis points.
    pub protocol_bps: u64,
    /// Creator fee in basis points.
    pub creator_bps: u64,
}

/// pump.fun's fee tiers, never empty, in account order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeTiers {
    first: FeeTier,
    rest: Vec<FeeTier>,
}

impl FeeTiers {
    /// Returns the tiers, or `None` for an empty list.
    pub fn new(mut tiers: Vec<FeeTier>) -> Option<FeeTiers> {
        if tiers.is_empty() {
            return None;
        }
        let first = tiers.remove(0);
        Some(FeeTiers { first, rest: tiers })
    }

    /// Returns the tier at `index` in account order.
    pub fn get(&self, index: usize) -> Option<&FeeTier> {
        if index == 0 { Some(&self.first) } else { self.rest.get(index - 1) }
    }

    /// Returns one tier that charges the highest fee at every market cap. Quotes made with it come
    /// out lower, so a minimum-out never assumes a fee cut that may have ended.
    pub fn strictest(&self) -> FeeTiers {
        let mut highest = self.first;
        for tier in &self.rest {
            if tier.protocol_bps + tier.creator_bps > highest.protocol_bps + highest.creator_bps {
                highest = *tier;
            }
        }
        FeeTiers { first: FeeTier { threshold: 0, ..highest }, rest: Vec::new() }
    }
}

/// Parses the `FeeConfig` account (`idl/pump.json`): discriminator, bump u8, admin pubkey, flat fees
/// (3 x u64), then `fee_tiers`: a u32 count and, per tier, a u128 market-cap threshold in lamports
/// and lp, protocol, and creator fees in basis points (3 x u64).
pub fn parse_fee_config(data: &[u8]) -> Option<FeeTiers> {
    if data.len() < 69 || data[..8] != FEE_CONFIG_ACCOUNT {
        return None;
    }
    let count = u32::from_le_bytes([data[65], data[66], data[67], data[68]]) as usize;
    if count == 0 || 69 + count * 40 > data.len() {
        return None;
    }
    let mut tiers = Vec::new();
    for i in 0..count {
        let at = 69 + i * 40;
        let threshold = u128::from(u64_at(data, at)) | (u128::from(u64_at(data, at + 8)) << 64);
        tiers.push(FeeTier { threshold, protocol_bps: u64_at(data, at + 24), creator_bps: u64_at(data, at + 32) });
    }
    FeeTiers::new(tiers)
}

/// Returns the protocol plus creator fee for a curve, chosen by market cap in lamports (virtual SOL
/// x supply / virtual tokens), as `calculateFeeTier` in `FEE_PROGRAM_README.md` does.
pub fn curve_fee_bps(tiers: &FeeTiers, curve: &Curve) -> u64 {
    let cap =
        u128::from(curve.virtual_quote_reserves) * u128::from(curve.token_total_supply) / u128::from(curve.virtual_token_reserves.max(1));
    let (first, rest) = (&tiers.first, &tiers.rest);
    let mut tier = first;
    if cap >= tier.threshold {
        for candidate in rest.iter().rev().chain(std::iter::once(first)) {
            if cap >= candidate.threshold {
                tier = candidate;
                break;
            }
        }
    }
    tier.protocol_bps + tier.creator_bps
}

/// Returns the tokens a buy of `lamports` gets after fees: the buy spends `lamports` including fees,
/// so the curve sees `lamports / (1 + fee)`.
///
/// # Examples
///
/// ```
/// use solana_sniper_bot::execute::pump::{Curve, tokens_for_lamports};
/// let curve = Curve { virtual_token_reserves: 1_000_000_000_000_000, virtual_quote_reserves: 32_000_000_000, real_token_reserves: 0,
///     real_quote_reserves: 0, token_total_supply: 0, complete: false, mayhem: false, cashback: false, sol_paired: true,
///     data_length: 115 };
/// assert_eq!(tokens_for_lamports(&curve, 10_000_000, 125), 308_546_738_277);
/// ```
pub fn tokens_for_lamports(curve: &Curve, lamports: u64, fee_bps: u64) -> u64 {
    let net = u128::from(lamports) * 10_000 / (10_000 + u128::from(fee_bps));
    let out = u128::from(curve.virtual_token_reserves) * net / (u128::from(curve.virtual_quote_reserves) + net);
    u64::try_from(out).unwrap_or(u64::MAX)
}

/// Returns the lamports a sale of `tokens` gets after fees: the fee, rounded up, comes out of what
/// the curve returns.
pub fn lamports_for_tokens(curve: &Curve, tokens: u64, fee_bps: u64) -> u64 {
    let gross =
        u128::from(curve.virtual_quote_reserves) * u128::from(tokens) / (u128::from(curve.virtual_token_reserves) + u128::from(tokens));
    let fee = (gross * u128::from(fee_bps)).div_ceil(10_000);
    u64::try_from(gross.saturating_sub(fee)).unwrap_or(u64::MAX)
}

/// Returns `amount` reduced by `pct` percent, rounded down, in basis-point steps.
pub fn with_slippage(amount: u64, pct: f64) -> u64 {
    let keep_bps = ((100.0 - pct) * 100.0).round().max(0.0) as u128;
    u64::try_from(u128::from(amount) * keep_bps / 10_000).unwrap_or(u64::MAX)
}

/// The 17 accounts of `buy_v3`, `buy_exact_quote_in_v3`, and `sell_v3`, in IDL order.
fn trade_keys(user: &Pubkey, mint: &Pubkey, base_token_program: &Pubkey) -> Vec<AccountMeta> {
    let curve = bonding_curve_address(mint);
    let volume = cached(Derived::Volume(*user));
    let buyback = BUYBACK_FEE_RECIPIENTS[rand::rng().random_range(0..BUYBACK_FEE_RECIPIENTS.len())];
    vec![
        AccountMeta::new_readonly(*GLOBAL, false),
        AccountMeta::new_readonly(*mint, false),
        AccountMeta::new_readonly(WSOL_MINT, false),
        AccountMeta::new_readonly(*base_token_program, false),
        AccountMeta::new_readonly(TOKEN_PROGRAM, false),
        AccountMeta::new(curve, false),
        AccountMeta::new(associated_token_address(&curve, mint, base_token_program), false),
        AccountMeta::new(associated_token_address(&curve, &WSOL_MINT, &TOKEN_PROGRAM), false),
        AccountMeta::new(*user, true),
        AccountMeta::new(associated_token_address(user, mint, base_token_program), false),
        AccountMeta::new(associated_token_address(user, &WSOL_MINT, &TOKEN_PROGRAM), false),
        AccountMeta::new(volume, false),
        AccountMeta::new_readonly(fee_config(), false),
        AccountMeta::new(buyback, false),
        AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
        AccountMeta::new_readonly(*EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(PUMP_PROGRAM, false),
    ]
}

fn trade_data(discriminator: [u8; 8], amount: u64, limit: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&discriminator);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&limit.to_le_bytes());
    data
}

/// The trade does not create the buyer's token account, so create it first. The idempotent form
/// succeeds when the account already exists.
fn create_ata_idempotent(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Instruction {
    Instruction {
        program_id: ATA_PROGRAM,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(associated_token_address(owner, mint, token_program), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

/// Returns the instructions of a buy that spends exactly `lamports` (fees included) and accepts no
/// fewer than `min_tokens_out`.
pub fn buy_instructions(user: &Pubkey, mint: &Pubkey, base_token_program: &Pubkey, lamports: u64, min_tokens_out: u64) -> Vec<Instruction> {
    vec![
        create_ata_idempotent(user, user, mint, base_token_program),
        Instruction {
            program_id: PUMP_PROGRAM,
            accounts: trade_keys(user, mint, base_token_program),
            data: trade_data(BUY_EXACT_QUOTE_IN_V3, lamports, min_tokens_out),
        },
    ]
}

/// Returns the instruction of a sale of `tokens` for no fewer than `min_lamports_out`.
pub fn sell_instruction(user: &Pubkey, mint: &Pubkey, base_token_program: &Pubkey, tokens: u64, min_lamports_out: u64) -> Instruction {
    Instruction {
        program_id: PUMP_PROGRAM,
        accounts: trade_keys(user, mint, base_token_program),
        data: trade_data(SELL_V3, tokens, min_lamports_out),
    }
}
