//! Answers every call the bot makes from a scenario's `chain` block, so the demo, the benchmark, and
//! the trade tests run the real pipeline with no network. Account bytes use the on-chain layouts the
//! bot parses. Signature statuses, debits, and balances start empty; tests fill them to play the
//! chain.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde::Deserialize;
use solana_pubkey::Pubkey;

use crate::execute::pump::{
    PUMP_PROGRAM, TOKEN_2022_PROGRAM, TOKEN_PROGRAM, bonding_curve_address, curve_fee_bps, fee_config, lamports_for_tokens, parse_curve,
    parse_fee_config,
};
use crate::network::{
    AccountData, Blockhash, EpochInfo, JupiterExecute, JupiterOrder, MarketSnapshot, NetError, NetResult, Network, OrderRequest,
    RugcheckSummary, SignatureStatus,
};

/// A bonding curve in a scenario. Amounts are decimal strings.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurveSpec {
    /// Virtual token reserves.
    pub virtual_token_reserves: String,
    /// Virtual quote reserves.
    pub virtual_quote_reserves: String,
    /// Real token reserves.
    pub real_token_reserves: String,
    /// Real quote reserves.
    pub real_quote_reserves: String,
    /// Total supply.
    pub token_total_supply: String,
    /// Whether the curve finished.
    pub complete: bool,
}

/// A mint in a scenario.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MintSpec {
    /// `spl` or `token-2022`.
    pub token_program: String,
    /// A mint authority is set.
    pub mint_authority: bool,
    /// A freeze authority is set.
    pub freeze_authority: bool,
    /// Raw supply.
    pub supply: String,
    /// Decimals.
    pub decimals: u8,
    /// The curve, or `None` when the mint is not on one.
    pub curve: Option<CurveSpec>,
}

/// One pump.fun fee tier in a scenario.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeTierSpec {
    /// Lowest market cap in lamports, as a decimal string.
    pub market_cap_lamports_threshold: String,
    /// Protocol fee in basis points.
    pub protocol_fee_bps: u64,
    /// Creator fee in basis points.
    pub creator_fee_bps: u64,
}

/// The `chain` block of `shared/demo/scenario.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainSpec {
    /// SOL price.
    pub sol_usd: f64,
    /// Priority fee level.
    pub priority_fee_micro_lamports: u64,
    /// Jito tip level.
    pub tip_floor_sol: f64,
    /// Block height; each blockhash is valid for 150 blocks after it.
    pub block_height: u64,
    /// Current epoch.
    pub epoch: u64,
    /// pump.fun fee tiers.
    pub fee_tiers: Vec<FeeTierSpec>,
    /// Mints by address.
    pub mints: HashMap<String, MintSpec>,
}

fn amount(text: &str) -> u64 {
    text.parse().unwrap_or(0)
}

/// An SPL Mint account: 82 bytes.
fn mint_bytes(spec: &MintSpec) -> Vec<u8> {
    let mut data = vec![0u8; 82];
    data[0..4].copy_from_slice(&u32::from(spec.mint_authority).to_le_bytes());
    data[36..44].copy_from_slice(&amount(&spec.supply).to_le_bytes());
    data[44] = spec.decimals;
    data[45] = 1;
    data[46..50].copy_from_slice(&u32::from(spec.freeze_authority).to_le_bytes());
    data
}

/// A pump.fun `BondingCurve` account with a SOL quote mint: 115 bytes.
fn curve_bytes(curve: &CurveSpec) -> Vec<u8> {
    let mut data = vec![0u8; 115];
    data[..8].copy_from_slice(&[23, 183, 248, 55, 96, 216, 172, 96]);
    let fields = [
        &curve.virtual_token_reserves,
        &curve.virtual_quote_reserves,
        &curve.real_token_reserves,
        &curve.real_quote_reserves,
        &curve.token_total_supply,
    ];
    for (i, value) in fields.into_iter().enumerate() {
        data[8 + i * 8..16 + i * 8].copy_from_slice(&amount(value).to_le_bytes());
    }
    data[48] = u8::from(curve.complete);
    data
}

/// pump.fun `FeeConfig`: discriminator, bump, admin, flat fees, then the tier list.
fn fee_config_bytes(tiers: &[FeeTierSpec]) -> Vec<u8> {
    let mut data = vec![0u8; 69 + tiers.len() * 40];
    data[..8].copy_from_slice(&[143, 52, 146, 187, 219, 123, 76, 155]);
    data[65..69].copy_from_slice(&(tiers.len() as u32).to_le_bytes());
    for (i, tier) in tiers.iter().enumerate() {
        let at = 69 + i * 40;
        let threshold: u128 = tier.market_cap_lamports_threshold.parse().unwrap_or(0);
        data[at..at + 16].copy_from_slice(&threshold.to_le_bytes());
        data[at + 24..at + 32].copy_from_slice(&tier.protocol_fee_bps.to_le_bytes());
        data[at + 32..at + 40].copy_from_slice(&tier.creator_fee_bps.to_le_bytes());
    }
    data
}

/// Returns the account bytes a chain block describes: the fee config, each mint, and each curve.
///
/// # Panics
///
/// Panics when a scenario mint is not a base58 public key.
pub fn chain_accounts(chain: &ChainSpec) -> HashMap<String, AccountData> {
    let mut accounts = HashMap::new();
    accounts.insert(
        fee_config().to_string(),
        AccountData { owner: "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ".to_string(), data: fee_config_bytes(&chain.fee_tiers) },
    );
    for (mint, spec) in &chain.mints {
        let owner = if spec.token_program == "spl" { TOKEN_PROGRAM } else { TOKEN_2022_PROGRAM };
        accounts.insert(mint.clone(), AccountData { owner: owner.to_string(), data: mint_bytes(spec) });
        if let Some(curve) = &spec.curve {
            let key: Pubkey = mint.parse().expect("scenario mints are base58 public keys");
            accounts
                .insert(bonding_curve_address(&key).to_string(), AccountData { owner: PUMP_PROGRAM.to_string(), data: curve_bytes(curve) });
        }
    }
    accounts
}

/// A [`Network`] that answers from a scenario. Tests change its accounts, statuses, debits,
/// balances, and block height to play the chain.
#[derive(Debug)]
pub struct ScenarioNetwork {
    chain: ChainSpec,
    accounts: Mutex<HashMap<String, AccountData>>,
    statuses: Mutex<HashMap<String, SignatureStatus>>,
    debits: Mutex<HashMap<String, i128>>,
    balances: Mutex<HashMap<String, u64>>,
    block_height: AtomicU64,
    blockhash_valid: AtomicBool,
    read_delay: Option<Duration>,
}

fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

impl ScenarioNetwork {
    /// Builds the account bytes for the fee config and every mint and curve in the scenario.
    pub fn new(chain: ChainSpec) -> ScenarioNetwork {
        let accounts = chain_accounts(&chain);
        let block_height = AtomicU64::new(chain.block_height);
        ScenarioNetwork {
            chain,
            accounts: Mutex::new(accounts),
            statuses: Mutex::default(),
            debits: Mutex::default(),
            balances: Mutex::default(),
            block_height,
            blockhash_valid: AtomicBool::new(true),
            read_delay: None,
        }
    }

    /// Makes every account read wait this long, standing in for a nearby RPC.
    pub fn with_read_delay(mut self, delay: Duration) -> ScenarioNetwork {
        self.read_delay = Some(delay);
        self
    }

    /// Sets or replaces an account.
    pub fn set_account(&self, address: &str, account: AccountData) {
        guard(&self.accounts).insert(address.to_string(), account);
    }

    /// Sets what `getSignatureStatuses` reports for a signature.
    pub fn set_status(&self, signature: &str, status: SignatureStatus) {
        guard(&self.statuses).insert(signature.to_string(), status);
    }

    /// Sets what a transaction took from the wallet.
    pub fn set_debit(&self, signature: &str, lamports: i128) {
        guard(&self.debits).insert(signature.to_string(), lamports);
    }

    /// Sets the wallet's balance of a token.
    pub fn set_balance(&self, mint: &str, amount: u64) {
        guard(&self.balances).insert(mint.to_string(), amount);
    }

    /// Sets what `isBlockhashValid` answers for every blockhash (true until set).
    pub fn set_blockhash_valid(&self, valid: bool) {
        self.blockhash_valid.store(valid, Ordering::Relaxed);
    }

    /// Moves the chain to a block height.
    pub fn set_block_height(&self, height: u64) {
        self.block_height.store(height, Ordering::Relaxed);
    }
}

impl Network for ScenarioNetwork {
    async fn get_accounts(&self, addresses: &[String]) -> NetResult<Vec<Option<AccountData>>> {
        if let Some(delay) = self.read_delay {
            // The runtime's timer wheel rounds sleeps up to the next millisecond tick, which would
            // double a 1 ms stand-in; a blocking-pool thread sleeps for the time asked.
            let _ = tokio::task::spawn_blocking(move || std::thread::sleep(delay)).await;
        }
        let accounts = guard(&self.accounts);
        let mut out = Vec::new();
        for address in addresses {
            out.push(accounts.get(address).cloned());
        }
        Ok(out)
    }

    async fn latest_blockhash(&self) -> NetResult<Blockhash> {
        let last_valid_block_height = self.block_height.load(Ordering::Relaxed) + 150;
        Ok(Blockhash { blockhash: bs58::encode([7u8; 32]).into_string(), last_valid_block_height })
    }

    async fn epoch_info(&self) -> NetResult<EpochInfo> {
        Ok(EpochInfo { epoch: self.chain.epoch, block_height: self.block_height.load(Ordering::Relaxed) })
    }

    async fn signature_status(&self, signature: &str) -> NetResult<Option<SignatureStatus>> {
        Ok(guard(&self.statuses).get(signature).cloned())
    }

    async fn is_blockhash_valid(&self, _blockhash: &str) -> NetResult<bool> {
        Ok(self.blockhash_valid.load(Ordering::Relaxed))
    }

    async fn transaction_debit(&self, signature: &str, _owner: &str) -> NetResult<Option<i128>> {
        Ok(guard(&self.debits).get(signature).copied())
    }

    async fn jupiter_order(&self, _request: OrderRequest) -> NetResult<JupiterOrder> {
        Err(NetError("the scenario has no Jupiter route".to_string()))
    }

    async fn jupiter_execute(&self, _signed_tx: &str, _request_id: &str, _timeout: Duration) -> NetResult<JupiterExecute> {
        Err(NetError("the scenario has no Jupiter route".to_string()))
    }

    async fn recent_priority_fees(&self) -> NetResult<Vec<u64>> {
        Ok(vec![self.chain.priority_fee_micro_lamports])
    }

    async fn jito_tip_floor_sol(&self) -> NetResult<Option<f64>> {
        Ok(Some(self.chain.tip_floor_sol))
    }

    async fn sol_usd_price(&self) -> NetResult<Option<f64>> {
        Ok(Some(self.chain.sol_usd))
    }

    async fn market_snapshot(&self, _mint: &str) -> NetResult<MarketSnapshot> {
        Ok(MarketSnapshot::default())
    }

    async fn rugcheck(&self, _mint: &str) -> NetResult<Option<RugcheckSummary>> {
        Ok(None)
    }

    async fn quote_sell_lamports(&self, mint: &str, tokens: u64) -> NetResult<Option<u64>> {
        let accounts = guard(&self.accounts);
        let tiers = accounts.get(&fee_config().to_string()).and_then(|a| parse_fee_config(&a.data));
        let curve = mint
            .parse::<Pubkey>()
            .ok()
            .and_then(|m| accounts.get(&bonding_curve_address(&m).to_string()))
            .and_then(|a| parse_curve(&a.data));
        Ok(match (curve, tiers) {
            (Some(curve), Some(tiers)) => Some(lamports_for_tokens(&curve, tokens, curve_fee_bps(&tiers, &curve))),
            _ => None,
        })
    }

    async fn token_balance(&self, _owner: &str, mint: &str) -> NetResult<u64> {
        Ok(guard(&self.balances).get(mint).copied().unwrap_or(0))
    }
}
