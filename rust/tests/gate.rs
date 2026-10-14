//! Safety checks and filters through `run_gate`, with a hand-built network that answers like the
//! RPC: owned account bytes in the SPL Mint, Token-2022, and pump.fun curve layouts.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use solana_pubkey::Pubkey;
use solana_sniper_bot::execute::pump::{PUMP_PROGRAM, TOKEN_2022_PROGRAM, TOKEN_PROGRAM, bonding_curve_address};
use solana_sniper_bot::filters::gate::{GateCode, GateDeps, GateResult, GateVerdict, run_gate};
use solana_sniper_bot::network::AccountData;
use solana_sniper_bot::scenario::{ChainSpec, ScenarioNetwork};
use solana_sniper_bot::setup::config::{Config, parse_config};

const MINT: &str = "FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA";

fn mint(owner: Pubkey, mint_authority: bool, freeze_authority: bool, fee_bps: u16) -> AccountData {
    let mut data = vec![0u8; 82];
    data[0..4].copy_from_slice(&u32::from(mint_authority).to_le_bytes());
    data[36..44].copy_from_slice(&1_000_000_000_000_000u64.to_le_bytes());
    data[44] = 6;
    data[46..50].copy_from_slice(&u32::from(freeze_authority).to_le_bytes());
    if fee_bps > 0 {
        data.resize(165, 0);
        data.push(1);
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&108u16.to_le_bytes());
        let mut value = vec![0u8; 108];
        value[106..108].copy_from_slice(&fee_bps.to_le_bytes());
        data.extend_from_slice(&value);
    }
    AccountData { owner: owner.to_string(), data }
}

/// 32 SOL virtual reserves against 1e15 virtual tokens and supply, 2 SOL real reserves.
fn live_curve(sol_paired: bool) -> AccountData {
    let mut data = vec![0u8; 115];
    data[..8].copy_from_slice(&[23, 183, 248, 55, 96, 216, 172, 96]);
    data[8..16].copy_from_slice(&1_000_000_000_000_000u64.to_le_bytes());
    data[16..24].copy_from_slice(&32_000_000_000u64.to_le_bytes());
    data[32..40].copy_from_slice(&2_000_000_000u64.to_le_bytes());
    data[40..48].copy_from_slice(&1_000_000_000_000_000u64.to_le_bytes());
    if !sol_paired {
        data[83] = 1;
    }
    AccountData { owner: PUMP_PROGRAM.to_string(), data }
}

fn chain(accounts: Vec<(String, AccountData)>, delay: Duration) -> ScenarioNetwork {
    let spec = ChainSpec {
        sol_usd: 150.0,
        priority_fee_micro_lamports: 0,
        tip_floor_sol: 0.0,
        block_height: 1_000,
        epoch: 850,
        fee_tiers: Vec::new(),
        mints: HashMap::new(),
    };
    let network = ScenarioNetwork::new(spec).with_read_delay(delay);
    for (address, account) in accounts {
        network.set_account(&address, account);
    }
    network
}

fn config(filters: serde_json::Value, budget_ms: u64) -> Config {
    parse_config(json!({ "rules": [{ "name": "r", "mode": "ca_scanner", "accounts": ["1"], "filters": filters }], "safety": { "budgetMs": budget_ms } })).unwrap()
}

async fn gate(net: &ScenarioNetwork, config: &Config) -> GateResult {
    let mint: Pubkey = MINT.parse().unwrap();
    let deps = GateDeps { safety: &config.safety, sol_usd: Some(150.0), epoch: Some(850), wallet: Pubkey::new_unique() };
    run_gate(net, &mint, &config.rules[0], None, &deps).await
}

fn curve_key() -> String {
    bonding_curve_address(&MINT.parse().unwrap()).to_string()
}

fn code(result: &GateResult) -> Option<GateCode> {
    match &result.verdict {
        GateVerdict::Fail { code, detail: _ } => Some(*code),
        GateVerdict::Pass { curve: _, token_program: _ } => None,
    }
}

#[tokio::test]
async fn a_live_curve_passes_without_authority_checks_and_is_priced_from_the_curve() {
    let net = chain(vec![(MINT.to_string(), mint(TOKEN_2022_PROGRAM, true, false, 0)), (curve_key(), live_curve(true))], Duration::ZERO);
    let result = gate(&net, &config(json!({}), 100)).await;
    assert!(matches!(result.verdict, GateVerdict::Pass { token_program: TOKEN_2022_PROGRAM, .. }), "{result:?}");
    // 32 SOL x 1e15 supply / 1e15 virtual tokens = 32 SOL = $4800 at $150; 2 SOL real = $300.
    assert_eq!(result.facts.market_cap_usd, Some(4800.0));
    assert_eq!(result.facts.liquidity_usd, Some(300.0));
}

#[tokio::test]
async fn a_live_curve_quoted_in_another_token_is_skipped_as_unsupported() {
    let net = chain(vec![(MINT.to_string(), mint(TOKEN_2022_PROGRAM, false, false, 0)), (curve_key(), live_curve(false))], Duration::ZERO);
    let result = gate(&net, &config(json!({}), 100)).await;
    assert_eq!(code(&result), Some(GateCode::UnsupportedCurve));
}

#[tokio::test]
async fn a_coin_off_the_curve_is_unsupported() {
    let net = chain(vec![(MINT.to_string(), mint(TOKEN_PROGRAM, false, false, 0))], Duration::ZERO);
    assert_eq!(code(&gate(&net, &config(json!({}), 100)).await), Some(GateCode::UnsupportedCurve));
}

#[tokio::test]
async fn an_address_owned_by_another_program_is_not_a_mint() {
    let net = chain(vec![(MINT.to_string(), AccountData { owner: PUMP_PROGRAM.to_string(), data: vec![0; 200] })], Duration::ZERO);
    assert_eq!(code(&gate(&net, &config(json!({}), 100)).await), Some(GateCode::NotAMint));
}

#[tokio::test]
async fn a_read_slower_than_the_budget_counts_as_unknown_and_rejects() {
    let net = chain(vec![(MINT.to_string(), mint(TOKEN_PROGRAM, false, false, 0))], Duration::from_millis(200));
    let result = gate(&net, &config(json!({}), 20)).await;
    assert_eq!(code(&result), Some(GateCode::CheckUnknown));
    assert!(result.elapsed < Duration::from_millis(150), "{:?}", result.elapsed);
}

#[tokio::test]
async fn an_age_filter_with_no_market_data_rejects_unless_the_rule_allows_it() {
    let net = chain(vec![(MINT.to_string(), mint(TOKEN_PROGRAM, false, false, 0)), (curve_key(), live_curve(true))], Duration::ZERO);
    assert_eq!(code(&gate(&net, &config(json!({ "minTokenAgeSec": 60 }), 100)).await), Some(GateCode::MissingData));
    let lenient = gate(&net, &config(json!({ "minTokenAgeSec": 60, "onMissingData": "allow" }), 100)).await;
    assert_eq!(code(&lenient), None);
}

#[tokio::test]
async fn a_two_and_a_half_percent_transfer_fee_fails_the_default_max_pool_tax_of_zero() {
    let net = chain(vec![(MINT.to_string(), mint(TOKEN_2022_PROGRAM, false, false, 250)), (curve_key(), live_curve(true))], Duration::ZERO);
    let result = gate(&net, &config(json!({}), 100)).await;
    assert_eq!(code(&result), Some(GateCode::FilterTax));
    assert_eq!(result.facts.tax_pct, Some(2.5));
}
