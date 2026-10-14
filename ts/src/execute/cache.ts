// Values the buy path needs, refreshed in the background so it never waits for them: the latest
// blockhash, the current block height and epoch, priority fee and Jito tip levels, the SOL price,
// and pump.fun's fee tiers. Read by execute/trade.ts, filters/gate.ts, and land/land.ts.

import type { Network } from "../network.ts";
import { FEE_CONFIG, parseFeeConfig, type FeeTier } from "./pump.ts";
import { log } from "../log.ts";

// A blockhash older than this is not used: the cache has stopped refreshing, and a transaction
// built on it may already be close to expiry.
const MAX_BLOCKHASH_AGE_MS = 10_000;

export class ChainCache {
  private blockhash: { blockhash: string; lastValidBlockHeight: number; at: number } | null = null;
  private height: { blockHeight: number; epoch: number; at: number } | null = null;
  priorityMicroLamports: number | null = null; // 75th percentile of recent nonzero fees, per compute unit
  tipFloorSol: number | null = null;
  solUsd: number | null = null;
  feeTiers: FeeTier[] | null = null;
  private readonly network: Network;
  private readonly timers: NodeJS.Timeout[] = [];

  constructor(network: Network) {
    this.network = network;
  }

  start(refreshMs: number): void {
    this.every(refreshMs, async () => {
      const [hash, info] = await Promise.all([this.network.latestBlockhash(), this.network.epochInfo()]);
      this.blockhash = { ...hash, at: Date.now() };
      this.height = { ...info, at: Date.now() };
    });
    this.every(2_000, async () => {
      const fees = (await this.network.recentPriorityFees()).filter((f) => f > 0).sort((a, b) => a - b);
      if (fees.length) this.priorityMicroLamports = fees[Math.floor(fees.length * 0.75)]!;
    });
    this.every(10_000, async () => void (this.tipFloorSol = await this.network.jitoTipFloorSol()));
    this.every(30_000, async () => void (this.solUsd = await this.network.solUsdPrice()));
    this.every(60_000, async () => {
      const [account] = await this.network.getAccounts([FEE_CONFIG.toBase58()]);
      this.feeTiers = account ? parseFeeConfig(account.data) : null;
    });
  }

  stop(): void {
    for (const t of this.timers) clearInterval(t);
  }

  // Resolves once every value has had one chance to load, so a fresh start does not build on nothing.
  async ready(): Promise<void> {
    for (let i = 0; i < 50 && (!this.blockhash || !this.height); i++) await new Promise((r) => setTimeout(r, 100));
  }

  freshBlockhash(): { blockhash: string; lastValidBlockHeight: number } | null {
    return this.blockhash && Date.now() - this.blockhash.at <= MAX_BLOCKHASH_AGE_MS ? this.blockhash : null;
  }

  blockHeight(): number | null {
    return this.height?.blockHeight ?? null;
  }

  epoch(): number | null {
    return this.height?.epoch ?? null;
  }

  private every(ms: number, task: () => Promise<void>): void {
    let lastError = ""; // log an outage once, not on every refresh
    const run = () =>
      task().then(
        () => void (lastError = ""),
        (e: Error) => e.message !== lastError && log("cache", (lastError = e.message)),
      );
    void run();
    this.timers.push(setInterval(run, ms));
  }
}
