//! On-chain layouts and pump.fun instruction encoding, checked against bytes built by hand from the
//! published layouts (SPL Mint, Token-2022 TLV, pump `idl/pump.json`) and the addresses in pump's
//! docs.

use solana_pubkey::Pubkey;
use solana_sniper_bot::execute::pump::{
    Curve, PUMP_PROGRAM, TOKEN_2022_PROGRAM, bonding_curve_address, buy_instructions, curve_fee_bps, fee_config, lamports_for_tokens,
    parse_curve, parse_fee_config, tokens_for_lamports, with_slippage,
};
use solana_sniper_bot::filters::gate::{parse_mint, transfer_fee_bps};

fn mint_bytes(mint_authority: bool, freeze_authority: bool, extensions: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut data = vec![0u8; 82];
    data[0..4].copy_from_slice(&u32::from(mint_authority).to_le_bytes());
    data[36..44].copy_from_slice(&1_000_000_000_000_000u64.to_le_bytes());
    data[44] = 6;
    data[45] = 1;
    data[46..50].copy_from_slice(&u32::from(freeze_authority).to_le_bytes());
    if extensions.is_empty() {
        return data;
    }
    data.resize(165, 0);
    data.push(1);
    for (kind, value) in extensions {
        data.extend_from_slice(&kind.to_le_bytes());
        data.extend_from_slice(&(value.len() as u16).to_le_bytes());
        data.extend_from_slice(value);
    }
    data
}

/// TransferFeeConfig: two authorities (64), withheld amount (8), then older and newer fees, each
/// `{ epoch u64, maximum_fee u64, basis_points u16 }`.
fn fee_config_value(older: (u64, u16), newer: (u64, u16)) -> Vec<u8> {
    let mut value = vec![0u8; 108];
    value[72..80].copy_from_slice(&older.0.to_le_bytes());
    value[88..90].copy_from_slice(&older.1.to_le_bytes());
    value[90..98].copy_from_slice(&newer.0.to_le_bytes());
    value[106..108].copy_from_slice(&newer.1.to_le_bytes());
    value
}

#[test]
fn parse_mint_reads_authorities_supply_and_decimals() {
    let info = parse_mint(&mint_bytes(true, false, &[])).unwrap();
    assert!(info.mint_authority);
    assert!(!info.freeze_authority);
    assert_eq!(info.supply, 1_000_000_000_000_000);
    assert_eq!(info.decimals, 6);
    assert!(parse_mint(&mint_bytes(false, true, &[])).unwrap().freeze_authority);
    assert!(parse_mint(&[0u8; 81]).is_none());
}

#[test]
fn the_token_2022_transfer_fee_in_force_depends_on_the_epoch() {
    let info = parse_mint(&mint_bytes(false, false, &[(1, fee_config_value((0, 100), (900, 250))), (12, vec![0u8; 32])])).unwrap();
    assert_eq!(info.extensions, vec![1, 12]);
    assert_eq!(transfer_fee_bps(info.transfer_fee.as_ref(), Some(850)), 100);
    assert_eq!(transfer_fee_bps(info.transfer_fee.as_ref(), Some(900)), 250);
    assert_eq!(transfer_fee_bps(info.transfer_fee.as_ref(), None), 250, "unknown epoch: the higher fee");
}

#[test]
fn parse_curve_reads_the_bonding_curve_and_rejects_other_accounts() {
    let mut data = vec![0u8; 115];
    data[..8].copy_from_slice(&[23, 183, 248, 55, 96, 216, 172, 96]);
    data[8..16].copy_from_slice(&1_000_000_000_000_000u64.to_le_bytes());
    data[16..24].copy_from_slice(&32_000_000_000u64.to_le_bytes());
    data[48] = 1;
    data[82] = 1;
    let curve = parse_curve(&data).unwrap();
    assert_eq!(curve.virtual_token_reserves, 1_000_000_000_000_000);
    assert_eq!(curve.virtual_quote_reserves, 32_000_000_000);
    assert!(curve.complete && curve.cashback && curve.sol_paired);
    data[90] = 7;
    assert!(!parse_curve(&data).unwrap().sol_paired, "a non-default quote mint is not SOL");
    data[0] = 0;
    assert!(parse_curve(&data).is_none());
}

#[test]
fn fee_config_derives_to_the_address_published_in_trade_v3() {
    assert_eq!(fee_config().to_string(), "8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt");
}

#[test]
fn bonding_curve_address_uses_the_bump_pumps_faq_gives_for_its_example_mint() {
    let mint: Pubkey = "Coyj3LtKn1BNSgWc9HsGK5SKoGfEoDaymig4wrN6pump".parse().unwrap();
    let expected = Pubkey::create_program_address(&[b"bonding-curve", mint.as_ref(), &[255]], &PUMP_PROGRAM).unwrap();
    assert_eq!(bonding_curve_address(&mint), expected);
}

#[test]
fn buy_exact_quote_in_v3_encodes_discriminator_spend_and_minimum_out() {
    let user: Pubkey = "H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29".parse().unwrap();
    let mint: Pubkey = "FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA".parse().unwrap();
    let instructions = buy_instructions(&user, &mint, &TOKEN_2022_PROGRAM, 10_000_000, 249_921_899_406);
    assert_eq!(instructions[0].data, vec![1]);
    let buy = &instructions[1];
    assert_eq!(buy.data, vec![225, 247, 80, 30, 213, 179, 132, 136, 128, 150, 152, 0, 0, 0, 0, 0, 142, 139, 129, 48, 58, 0, 0, 0]);
    assert_eq!(buy.accounts.len(), 17);
    assert_eq!(buy.accounts[12].pubkey.to_string(), "8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt");
    assert_eq!(buy.accounts[8].pubkey, user);
    assert!(buy.accounts[8].is_signer);
}

#[test]
fn curve_quotes_take_pumps_fee_before_the_curve_price() {
    let curve = Curve {
        virtual_token_reserves: 1_000_000_000_000_000,
        virtual_quote_reserves: 32_000_000_000,
        real_token_reserves: 0,
        real_quote_reserves: 0,
        token_total_supply: 0,
        complete: false,
        mayhem: false,
        cashback: false,
        sol_paired: true,
    };
    // Buy 0.01 SOL at 125 bps: 1e7 x 10000 / 10125 = 9876543 reaches the curve;
    // 1e15 x 9876543 / (3.2e10 + 9876543) = 308546738277.
    assert_eq!(tokens_for_lamports(&curve, 10_000_000, 125), 308_546_738_277);
    // Sell 312402374258 tokens: the curve returns 9993753 lamports, the fee (rounded up) is 124922.
    assert_eq!(lamports_for_tokens(&curve, 312_402_374_258, 125), 9_868_831);
    // 312402374258 x 8000 / 10000 = 249921899406.4
    assert_eq!(with_slippage(312_402_374_258, 20.0), 249_921_899_406);
}

#[test]
fn parse_fee_config_reads_the_tiers_and_curve_fee_bps_picks_one_by_market_cap() {
    let tiers: [(u128, u64, u64); 3] = [(0, 95, 30), (20_000_000_000, 90, 5), (10u128.pow(20), 50, 5)];
    let mut data = vec![0u8; 69 + tiers.len() * 40];
    data[..8].copy_from_slice(&[143, 52, 146, 187, 219, 123, 76, 155]);
    data[65..69].copy_from_slice(&(tiers.len() as u32).to_le_bytes());
    for (i, (threshold, protocol, creator)) in tiers.iter().enumerate() {
        let at = 69 + i * 40;
        data[at..at + 16].copy_from_slice(&threshold.to_le_bytes());
        data[at + 24..at + 32].copy_from_slice(&protocol.to_le_bytes());
        data[at + 32..at + 40].copy_from_slice(&creator.to_le_bytes());
    }
    let parsed = parse_fee_config(&data).unwrap();
    assert_eq!(parsed[2].threshold, 10u128.pow(20), "u128 thresholds above 64 bits survive");
    let curve = |quote: u64| Curve {
        virtual_token_reserves: 1_000_000_000_000_000,
        virtual_quote_reserves: quote,
        real_token_reserves: 0,
        real_quote_reserves: 0,
        token_total_supply: 1_000_000_000_000_000,
        complete: false,
        mayhem: false,
        cashback: false,
        sol_paired: true,
    };
    assert_eq!(curve_fee_bps(&parsed, &curve(10_000_000_000)), 125, "10 SOL market cap: first tier");
    assert_eq!(curve_fee_bps(&parsed, &curve(32_000_000_000)), 95, "32 SOL market cap: second tier");
}
