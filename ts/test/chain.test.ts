// On-chain layouts and pump.fun instruction encoding, checked against bytes built by hand from the
// published layouts (SPL Mint, Token-2022 TLV, pump idl/pump.json) and the addresses in pump's docs.

import { test } from "node:test";
import assert from "node:assert/strict";
import { PublicKey } from "@solana/web3.js";
import { parseMint } from "../src/filters/gate.ts";
import { bondingCurveAddress, buyInstructions, FEE_CONFIG, parseCurve, PUMP_PROGRAM, TOKEN_2022_PROGRAM, tokensForLamports, withSlippage } from "../src/execute/pump.ts";

function mintBytes({ mintAuthority = false, freezeAuthority = false, extensions = [] as Array<[number, Buffer]> } = {}) {
  const base = Buffer.alloc(82);
  base.writeUInt32LE(mintAuthority ? 1 : 0, 0);
  base.writeBigUInt64LE(1_000_000_000_000_000n, 36);
  base[44] = 6;
  base[45] = 1;
  base.writeUInt32LE(freezeAuthority ? 1 : 0, 46);
  if (!extensions.length) return base;
  const tlv = extensions.map(([type, value]) => {
    const header = Buffer.alloc(4);
    header.writeUInt16LE(type, 0);
    header.writeUInt16LE(value.length, 2);
    return Buffer.concat([header, value]);
  });
  // Token-2022 pads the mint to 165 bytes, then writes the account type (1 = mint) and the TLVs.
  return Buffer.concat([base, Buffer.alloc(165 - 82), Buffer.from([1]), ...tlv]);
}

test("parseMint reads authorities, supply and decimals from the SPL layout", () => {
  const info = parseMint(mintBytes({ mintAuthority: true }))!;
  assert.equal(info.mintAuthority, true);
  assert.equal(info.freezeAuthority, false);
  assert.equal(info.supply, 1_000_000_000_000_000n);
  assert.equal(info.decimals, 6);
  assert.equal(parseMint(mintBytes({ freezeAuthority: true }))!.freezeAuthority, true);
  assert.equal(parseMint(Buffer.alloc(81)), null);
});

test("parseMint lists Token-2022 extensions and the newer transfer fee", () => {
  const feeConfig = Buffer.alloc(108);
  feeConfig.writeUInt16LE(100, 32 + 32 + 8 + 16); // older fee: 1%
  feeConfig.writeUInt16LE(250, 106); // newer fee: 2.5%
  const info = parseMint(mintBytes({ extensions: [[1, feeConfig], [12, Buffer.alloc(32)]] }))!;
  assert.deepEqual(info.extensions, [1, 12]);
  assert.equal(info.transferFeeBps, 250);
});

test("parseCurve reads the BondingCurve account and rejects other accounts", () => {
  const data = Buffer.alloc(115);
  Buffer.from([23, 183, 248, 55, 96, 216, 172, 96]).copy(data, 0);
  data.writeBigUInt64LE(1_000_000_000_000_000n, 8);
  data.writeBigUInt64LE(32_000_000_000n, 16);
  data[48] = 1;
  data[82] = 1;
  const curve = parseCurve(data)!;
  assert.equal(curve.virtualTokenReserves, 1_000_000_000_000_000n);
  assert.equal(curve.virtualQuoteReserves, 32_000_000_000n);
  assert.equal(curve.complete, true);
  assert.equal(curve.cashback, true);
  assert.equal(curve.solPaired, true);
  data[0] = 0;
  assert.equal(parseCurve(data), null);
});

test("fee_config derives to the address published in pump's TRADE_V3.md", () => {
  assert.equal(FEE_CONFIG.toBase58(), "8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt");
});

test("bonding curve address uses the bump pump's FAQ gives for its example mint", () => {
  const mint = "Coyj3LtKn1BNSgWc9HsGK5SKoGfEoDaymig4wrN6pump";
  const expected = PublicKey.createProgramAddressSync([Buffer.from("bonding-curve"), new PublicKey(mint).toBuffer(), Buffer.from([255])], PUMP_PROGRAM);
  assert.equal(bondingCurveAddress(mint).toBase58(), expected.toBase58());
});

test("buy_exact_quote_in_v3 encodes discriminator, spend and minimum out", () => {
  const user = new PublicKey("H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29");
  const mint = new PublicKey("FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA");
  const [createAta, buy] = buyInstructions(user, mint, TOKEN_2022_PROGRAM, 10_000_000n, 249_921_899_406n);
  assert.deepEqual([...createAta!.data], [1]);
  assert.deepEqual([...buy!.data], [225, 247, 80, 30, 213, 179, 132, 136, 128, 150, 152, 0, 0, 0, 0, 0, 142, 139, 129, 48, 58, 0, 0, 0]);
  assert.equal(buy!.keys.length, 17);
  assert.equal(buy!.keys[12]!.pubkey.toBase58(), "8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt");
  assert.equal(buy!.keys[8]!.pubkey.toBase58(), user.toBase58());
  assert.equal(buy!.keys[8]!.isSigner, true);
});

test("curve quote and slippage match a hand calculation", () => {
  const curve = { virtualTokenReserves: 10n ** 15n, virtualQuoteReserves: 32n * 10n ** 9n } as Parameters<typeof tokensForLamports>[0];
  // 1e15 * 1e7 / (3.2e10 + 1e7) = 312402374258.04...
  assert.equal(tokensForLamports(curve, 10_000_000n), 312_402_374_258n);
  assert.equal(withSlippage(312_402_374_258n, 20), 249_921_899_406n);
});
