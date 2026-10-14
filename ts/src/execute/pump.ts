// Builds pump.fun bonding-curve trades locally, with no HTTP round trip to a trade API, and quotes
// them with the program's fee tiers. Layouts follow pump-fun/pump-public-docs (idl/pump.json,
// docs/instructions/TRADE_V3.md, docs/FEE_RECIPIENTS.md, docs/FEE_PROGRAM_README.md).
// Used by execute/trade.ts, which signs and sends.

import { PublicKey, TransactionInstruction } from "@solana/web3.js";

export const PUMP_PROGRAM = new PublicKey("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
const PUMP_FEES_PROGRAM = new PublicKey("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
export const TOKEN_PROGRAM = new PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
export const TOKEN_2022_PROGRAM = new PublicKey("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const ATA_PROGRAM = new PublicKey("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
export const SYSTEM_PROGRAM = new PublicKey("11111111111111111111111111111111");
export const WSOL_MINT = new PublicKey("So11111111111111111111111111111111111111112");

// Any of these 8 wallets is a valid buyback fee recipient for SOL-paired coins (FEE_RECIPIENTS.md).
const BUYBACK_FEE_RECIPIENTS = [
  "5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD",
  "9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7",
  "GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL",
  "3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR",
  "5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6",
  "EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL",
  "5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD",
  "A7hAgCzFw14fejgCp387JUJRMNyz4j89JKnhtKU8piqW",
].map((a) => new PublicKey(a));

// Anchor discriminators from idl/pump.json.
const BUY_EXACT_QUOTE_IN_V3 = Buffer.from([225, 247, 80, 30, 213, 179, 132, 136]);
const SELL_V3 = Buffer.from([28, 146, 222, 119, 38, 196, 105, 213]);
const BONDING_CURVE_ACCOUNT = Buffer.from([23, 183, 248, 55, 96, 216, 172, 96]);
const FEE_CONFIG_ACCOUNT = Buffer.from([143, 52, 146, 187, 219, 123, 76, 155]);

const pda = (seeds: Array<Buffer | Uint8Array>, program = PUMP_PROGRAM) => PublicKey.findProgramAddressSync(seeds, program)[0];

// Derived once at startup: they do not depend on the mint or the wallet.
const GLOBAL = pda([Buffer.from("global")]);
const EVENT_AUTHORITY = pda([Buffer.from("__event_authority")]);
export const FEE_CONFIG = pda([Buffer.from("fee_config"), PUMP_PROGRAM.toBuffer()], PUMP_FEES_PROGRAM);

// A program address costs 100 to 200 us to derive (hashing plus an ed25519 curve check per bump
// tried), so every derived address is kept: the same mint is read, bought, and later sold.
const derived = new Map<string, PublicKey>();
function cached(key: string, derive: () => PublicKey): PublicKey {
  let address = derived.get(key);
  if (!address) {
    address = derive();
    derived.set(key, address);
    if (derived.size > 50_000) derived.delete(derived.keys().next().value!);
  }
  return address;
}

export function associatedTokenAddress(owner: PublicKey, mint: PublicKey, tokenProgram: PublicKey): PublicKey {
  return cached(`ata|${owner.toBase58()}|${mint.toBase58()}|${tokenProgram.toBase58()}`, () =>
    pda([owner.toBuffer(), tokenProgram.toBuffer(), mint.toBuffer()], ATA_PROGRAM),
  );
}

export function bondingCurveAddress(mint: string): PublicKey {
  return cached(`curve|${mint}`, () => pda([Buffer.from("bonding-curve"), new PublicKey(mint).toBuffer()]));
}

// Derives a buy's remaining addresses while the mint read is in flight. New pump.fun coins use
// Token-2022, so that program is assumed; an older SPL coin derives its own on use.
export function warmBuyAccounts(mint: string, user: PublicKey): void {
  const mintKey = new PublicKey(mint);
  const curve = bondingCurveAddress(mint);
  associatedTokenAddress(user, mintKey, TOKEN_2022_PROGRAM);
  associatedTokenAddress(curve, mintKey, TOKEN_2022_PROGRAM);
  associatedTokenAddress(curve, WSOL_MINT, TOKEN_PROGRAM);
}


// BondingCurve account (idl/pump.json types.BondingCurve). Older curves have a shorter layout,
// so fields after `complete` are read only when present.
export type Curve = {
  virtualTokenReserves: bigint;
  virtualQuoteReserves: bigint;
  realTokenReserves: bigint;
  realQuoteReserves: bigint;
  tokenTotalSupply: bigint;
  complete: boolean;
  mayhem: boolean;
  cashback: boolean;
  solPaired: boolean; // quote_mint is the default key, which means SOL
};

export function parseCurve(data: Uint8Array): Curve | null {
  if (data.length < 49 || !Buffer.from(data.subarray(0, 8)).equals(BONDING_CURVE_ACCOUNT)) return null;
  const view = Buffer.from(data.buffer, data.byteOffset, data.byteLength);
  const quoteMint = data.length >= 115 ? data.subarray(83, 115) : null;
  return {
    virtualTokenReserves: view.readBigUInt64LE(8),
    virtualQuoteReserves: view.readBigUInt64LE(16),
    realTokenReserves: view.readBigUInt64LE(24),
    realQuoteReserves: view.readBigUInt64LE(32),
    tokenTotalSupply: view.readBigUInt64LE(40),
    complete: data[48] === 1,
    mayhem: data.length > 81 && data[81] === 1,
    cashback: data.length > 82 && data[82] === 1,
    solPaired: quoteMint === null || quoteMint.every((b) => b === 0),
  };
}

// The local builder handles live, SOL-paired curves. Cashback coins reject v3 trades (error 6094).
export function localBuildBlocker(curve: Curve): string | null {
  if (curve.complete) return "bonding curve is complete (graduated)";
  if (!curve.solPaired) return "curve is not SOL-paired";
  if (curve.cashback) return "cashback coin: v3 trades are not supported";
  return null;
}

// FeeConfig account (idl/pump.json): discriminator, bump u8, admin pubkey, flat fees (3 x u64),
// then fee_tiers: a u32 count and, per tier, a u128 market-cap threshold in lamports and
// lp, protocol, and creator fees in basis points (3 x u64).
export type FeeTier = { threshold: bigint; protocolBps: bigint; creatorBps: bigint };

export function parseFeeConfig(data: Uint8Array): FeeTier[] | null {
  if (data.length < 69 || !Buffer.from(data.subarray(0, 8)).equals(FEE_CONFIG_ACCOUNT)) return null;
  const view = Buffer.from(data.buffer, data.byteOffset, data.byteLength);
  const count = view.readUInt32LE(65);
  if (count === 0 || 69 + count * 40 > data.length) return null;
  const tiers: FeeTier[] = [];
  for (let i = 0, at = 69; i < count; i++, at += 40) {
    const threshold = view.readBigUInt64LE(at) + (view.readBigUInt64LE(at + 8) << 64n);
    tiers.push({ threshold, protocolBps: view.readBigUInt64LE(at + 24), creatorBps: view.readBigUInt64LE(at + 32) });
  }
  return tiers;
}

// The protocol plus creator fee for a curve, chosen by market cap in lamports
// (virtual SOL x supply / virtual tokens), as calculateFeeTier in FEE_PROGRAM_README.md does.
export function curveFeeBps(tiers: FeeTier[], c: Curve): bigint {
  const cap = (c.virtualQuoteReserves * c.tokenTotalSupply) / c.virtualTokenReserves;
  const tier = cap < tiers[0]!.threshold ? tiers[0]! : ([...tiers].reverse().find((t) => cap >= t.threshold) ?? tiers[0]!);
  return tier.protocolBps + tier.creatorBps;
}

// Constant-product quotes on the virtual reserves, after fees. A buy spends `lamports` including
// fees, so the curve sees lamports / (1 + fee); a sell pays the fee out of what the curve returns.
export const tokensForLamports = (c: Curve, lamports: bigint, feeBps: bigint): bigint => {
  const net = (lamports * 10_000n) / (10_000n + feeBps);
  return (c.virtualTokenReserves * net) / (c.virtualQuoteReserves + net);
};
export const lamportsForTokens = (c: Curve, tokens: bigint, feeBps: bigint): bigint => {
  const gross = (c.virtualQuoteReserves * tokens) / (c.virtualTokenReserves + tokens);
  return gross - (gross * feeBps + 9_999n) / 10_000n;
};

export const withSlippage = (amount: bigint, pct: number): bigint => (amount * BigInt(Math.round((100 - pct) * 100))) / 10_000n;

function tradeKeys(user: PublicKey, mint: PublicKey, baseTokenProgram: PublicKey) {
  const curve = bondingCurveAddress(mint.toBase58());
  const volumeAccumulator = cached(`volume|${user.toBase58()}`, () => pda([Buffer.from("user_volume_accumulator"), user.toBuffer()]));
  const buyback = BUYBACK_FEE_RECIPIENTS[Math.floor(Math.random() * BUYBACK_FEE_RECIPIENTS.length)]!;
  const k = (pubkey: PublicKey, isWritable = false, isSigner = false) => ({ pubkey, isWritable, isSigner });
  // The 17 accounts of buy_v3 / buy_exact_quote_in_v3 / sell_v3, in IDL order.
  return [
    k(GLOBAL),
    k(mint),
    k(WSOL_MINT),
    k(baseTokenProgram),
    k(TOKEN_PROGRAM),
    k(curve, true),
    k(associatedTokenAddress(curve, mint, baseTokenProgram), true),
    k(associatedTokenAddress(curve, WSOL_MINT, TOKEN_PROGRAM), true),
    k(user, true, true),
    k(associatedTokenAddress(user, mint, baseTokenProgram), true),
    k(associatedTokenAddress(user, WSOL_MINT, TOKEN_PROGRAM), true),
    k(volumeAccumulator, true),
    k(FEE_CONFIG),
    k(buyback, true),
    k(SYSTEM_PROGRAM),
    k(EVENT_AUTHORITY),
    k(PUMP_PROGRAM),
  ];
}

const u64 = (value: bigint) => {
  const out = Buffer.alloc(8);
  out.writeBigUInt64LE(value);
  return out;
};

// The trade does not create the buyer's token account, so create it first. The idempotent form
// succeeds when the account already exists.
function createAtaIdempotent(payer: PublicKey, owner: PublicKey, mint: PublicKey, tokenProgram: PublicKey): TransactionInstruction {
  return new TransactionInstruction({
    programId: ATA_PROGRAM,
    keys: [
      { pubkey: payer, isSigner: true, isWritable: true },
      { pubkey: associatedTokenAddress(owner, mint, tokenProgram), isSigner: false, isWritable: true },
      { pubkey: owner, isSigner: false, isWritable: false },
      { pubkey: mint, isSigner: false, isWritable: false },
      { pubkey: SYSTEM_PROGRAM, isSigner: false, isWritable: false },
      { pubkey: tokenProgram, isSigner: false, isWritable: false },
    ],
    data: Buffer.from([1]),
  });
}

// Spend exactly `lamports` (fees included) and accept no fewer than `minTokensOut`.
export function buyInstructions(user: PublicKey, mint: PublicKey, baseTokenProgram: PublicKey, lamports: bigint, minTokensOut: bigint): TransactionInstruction[] {
  return [
    createAtaIdempotent(user, user, mint, baseTokenProgram),
    new TransactionInstruction({
      programId: PUMP_PROGRAM,
      keys: tradeKeys(user, mint, baseTokenProgram),
      data: Buffer.concat([BUY_EXACT_QUOTE_IN_V3, u64(lamports), u64(minTokensOut)]),
    }),
  ];
}

export function sellInstruction(user: PublicKey, mint: PublicKey, baseTokenProgram: PublicKey, tokens: bigint, minLamportsOut: bigint): TransactionInstruction {
  return new TransactionInstruction({
    programId: PUMP_PROGRAM,
    keys: tradeKeys(user, mint, baseTokenProgram),
    data: Buffer.concat([SELL_V3, u64(tokens), u64(minLamportsOut)]),
  });
}
