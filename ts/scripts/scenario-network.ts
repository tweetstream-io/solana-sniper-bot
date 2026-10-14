// Answers every call the bot makes from a scenario's "chain" block, so the demo, the benchmark, and
// the trade tests run the real pipeline with no network. Account bytes use the on-chain layouts the
// bot parses. Signature statuses, debits, token balances, and expired blockhashes start empty; tests fill them to play the chain.

import bs58 from "bs58";
import { bondingCurveAddress, curveFeeBps, FEE_CONFIG, lamportsForTokens, parseCurve, parseFeeConfig, PUMP_PROGRAM, TOKEN_2022_PROGRAM, TOKEN_PROGRAM } from "../src/execute/pump.ts";
import type { AccountData, JupiterExecute, JupiterOrder, MarketSnapshot, Network, RugcheckSummary } from "../src/network.ts";

export type MintSpec = {
  tokenProgram: "spl" | "token-2022";
  mintAuthority: boolean;
  freezeAuthority: boolean;
  supply: string;
  decimals: number;
  curve: null | { virtualTokenReserves: string; virtualQuoteReserves: string; realTokenReserves: string; realQuoteReserves: string; tokenTotalSupply: string; complete: boolean };
};
export type ScenarioChain = {
  solUsd: number;
  priorityFeeMicroLamports: number;
  tipFloorSol: number;
  blockHeight: number;
  epoch: number;
  feeTiers: Array<{ marketCapLamportsThreshold: string; protocolFeeBps: number; creatorFeeBps: number }>;
  mints: Record<string, MintSpec>;
};

const CURVE_DISCRIMINATOR = [23, 183, 248, 55, 96, 216, 172, 96];

// Account bytes in the on-chain layouts the gate parses (SPL Mint, pump.fun BondingCurve).
function mintBytes(m: MintSpec): Uint8Array {
  const data = Buffer.alloc(82);
  data.writeUInt32LE(m.mintAuthority ? 1 : 0, 0);
  data.writeBigUInt64LE(BigInt(m.supply), 36);
  data[44] = m.decimals;
  data[45] = 1;
  data.writeUInt32LE(m.freezeAuthority ? 1 : 0, 46);
  return data;
}

// pump.fun FeeConfig: discriminator, bump, admin, flat fees, then the tier list (see execute/pump.ts).
function feeConfigBytes(tiers: ScenarioChain["feeTiers"]): Uint8Array {
  const data = Buffer.alloc(69 + tiers.length * 40);
  Buffer.from([143, 52, 146, 187, 219, 123, 76, 155]).copy(data, 0);
  data.writeUInt32LE(tiers.length, 65);
  tiers.forEach((t, i) => {
    const at = 69 + i * 40;
    const threshold = BigInt(t.marketCapLamportsThreshold);
    data.writeBigUInt64LE(threshold & ((1n << 64n) - 1n), at);
    data.writeBigUInt64LE(threshold >> 64n, at + 8);
    data.writeBigUInt64LE(BigInt(t.protocolFeeBps), at + 24);
    data.writeBigUInt64LE(BigInt(t.creatorFeeBps), at + 32);
  });
  return data;
}

function curveBytes(c: NonNullable<MintSpec["curve"]>): Uint8Array {
  const data = Buffer.alloc(115);
  Buffer.from(CURVE_DISCRIMINATOR).copy(data, 0);
  [c.virtualTokenReserves, c.virtualQuoteReserves, c.realTokenReserves, c.realQuoteReserves, c.tokenTotalSupply].forEach((v, i) => data.writeBigUInt64LE(BigInt(v), 8 + i * 8));
  data[48] = c.complete ? 1 : 0;
  return data;
}

export class ScenarioNetwork implements Network {
  readonly accounts = new Map<string, AccountData>();
  readonly statuses = new Map<string, { confirmed: boolean; err: unknown }>(); // signature -> status
  readonly debits = new Map<string, bigint>(); // signature -> lamports taken from the wallet
  readonly balances = new Map<string, bigint>(); // mint -> token balance of the wallet
  readonly expiredBlockhashes = new Set<string>(); // isBlockhashValid answers false for these
  walletLamports = 10_000_000_000n;
  simulatedDebit = 0n; // what a simulated transaction takes from the wallet
  blockHeight: number;
  private readonly chain: ScenarioChain;

  constructor(chain: ScenarioChain) {
    this.chain = chain;
    this.blockHeight = chain.blockHeight;
    this.accounts.set(FEE_CONFIG.toBase58(), { owner: "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ", data: feeConfigBytes(chain.feeTiers) });
    for (const [mint, spec] of Object.entries(chain.mints)) {
      const owner = (spec.tokenProgram === "spl" ? TOKEN_PROGRAM : TOKEN_2022_PROGRAM).toBase58();
      this.accounts.set(mint, { owner, data: mintBytes(spec) });
      if (spec.curve) this.accounts.set(bondingCurveAddress(mint).toBase58(), { owner: PUMP_PROGRAM.toBase58(), data: curveBytes(spec.curve) });
    }
  }

  async getAccounts(addresses: string[]) {
    return addresses.map((a) => this.accounts.get(a) ?? null);
  }
  async latestBlockhash() {
    return { blockhash: bs58.encode(Buffer.alloc(32, 7)), lastValidBlockHeight: this.blockHeight + 150 };
  }
  async epochInfo() {
    return { epoch: this.chain.epoch, blockHeight: this.blockHeight };
  }
  async signatureStatus(signature: string) {
    return this.statuses.get(signature) ?? null;
  }
  async isBlockhashValid(blockhash: string) {
    return !this.expiredBlockhashes.has(blockhash);
  }
  async balance() {
    return this.walletLamports;
  }
  async simulate() {
    return { err: null, walletLamportsAfter: this.walletLamports - this.simulatedDebit };
  }
  async transactionDebit(signature: string) {
    return this.debits.get(signature) ?? null;
  }
  async jupiterOrder(_q: Parameters<Network["jupiterOrder"]>[0]): Promise<JupiterOrder> {
    throw new Error("the scenario has no Jupiter route");
  }
  async jupiterExecute(): Promise<JupiterExecute> {
    throw new Error("the scenario has no Jupiter route");
  }
  async recentPriorityFees() {
    return [this.chain.priorityFeeMicroLamports];
  }
  async jitoTipFloorSol() {
    return this.chain.tipFloorSol;
  }
  async solUsdPrice() {
    return this.chain.solUsd;
  }
  async marketSnapshot(): Promise<MarketSnapshot> {
    return {};
  }
  async rugcheck(): Promise<RugcheckSummary | null> {
    return null;
  }
  async quoteSellLamports(mint: string, tokens: bigint) {
    const curve = this.curve(mint);
    const tiers = parseFeeConfig(this.accounts.get(FEE_CONFIG.toBase58())!.data);
    return curve && tiers ? lamportsForTokens(curve, tokens, curveFeeBps(tiers, curve)) : null;
  }
  async tokenBalance(_owner: string, mint: string) {
    return this.balances.get(mint) ?? 0n;
  }
  private curve(mint: string) {
    const account = this.accounts.get(bondingCurveAddress(mint).toBase58());
    return account ? parseCurve(account.data) : null;
  }
}
