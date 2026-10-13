// Answers every read the bot makes from a scenario's "chain" block, so the demo and the benchmark
// run the real pipeline with no network. Account bytes use the on-chain layouts the gate parses.

import bs58 from "bs58";
import { bondingCurveAddress, lamportsForTokens, parseCurve, PUMP_PROGRAM, TOKEN_2022_PROGRAM, TOKEN_PROGRAM, tokensForLamports } from "../src/execute/pump.ts";
import type { AccountData, MarketSnapshot, Network, RugcheckSummary } from "../src/network.ts";

export type MintSpec = {
  tokenProgram: "spl" | "token-2022";
  mintAuthority: boolean;
  freezeAuthority: boolean;
  supply: string;
  decimals: number;
  curve: null | { virtualTokenReserves: string; virtualQuoteReserves: string; realTokenReserves: string; realQuoteReserves: string; tokenTotalSupply: string; complete: boolean };
};
export type Chain = { solUsd: number; priorityFeeMicroLamports: number; tipFloorSol: number; blockHeight: number; mints: Record<string, MintSpec> };

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

function curveBytes(c: NonNullable<MintSpec["curve"]>): Uint8Array {
  const data = Buffer.alloc(115);
  Buffer.from(CURVE_DISCRIMINATOR).copy(data, 0);
  [c.virtualTokenReserves, c.virtualQuoteReserves, c.realTokenReserves, c.realQuoteReserves, c.tokenTotalSupply].forEach((v, i) => data.writeBigUInt64LE(BigInt(v), 8 + i * 8));
  data[48] = c.complete ? 1 : 0;
  return data;
}

export class ScenarioNetwork implements Network {
  private readonly accounts = new Map<string, AccountData>();
  private readonly chain: Chain;

  constructor(chain: Chain) {
    this.chain = chain;
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
    return { blockhash: bs58.encode(Buffer.alloc(32, 7)), lastValidBlockHeight: this.chain.blockHeight + 150 };
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
    return curve ? lamportsForTokens(curve, tokens) : null;
  }
  async quoteBuyTokens(mint: string, lamports: bigint) {
    const curve = this.curve(mint);
    return curve ? tokensForLamports(curve, lamports) : null;
  }
  async tokenBalance() {
    return 0n;
  }
  private curve(mint: string) {
    const account = this.accounts.get(bondingCurveAddress(mint).toBase58());
    return account ? parseCurve(account.data) : null;
  }
}
