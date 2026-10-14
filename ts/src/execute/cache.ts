// Values the buy path needs, refreshed in the background so it never waits for them: the latest
// blockhash, the current block height and epoch, priority fee and Jito tip levels, the SOL price,
// and pump.fun's fee tiers. Read by execute/trade.ts, filters/gate.ts, exits/exits.ts, and
// land/land.ts. A stale value is never used as if it were fresh: see each getter.

import type { Network } from "../network.ts";
import { FEE_CONFIG, parseFeeConfig, type FeeTier } from "./pump.ts";
import { log } from "../log.ts";

const BLOCKHASH_MAX_AGE_MS = 10_000;
const EPOCH_MAX_AGE_MS = 60_000;
const FEE_TIERS_MAX_AGE_MS = 300_000;

export class ChainCache {
  private blockhash: { blockhash: string; lastValidBlockHeight: number; at: number } | null = null;
  private height: { blockHeight: number; epoch: number; at: number } | null = null;
  private tiers: { tiers: FeeTier[]; at: number } | null = null;
  priorityMicroLamports: number | null = null; // 75th percentile of recent nonzero fees, per compute unit
  tipFloorSol: number | null = null;
  solUsd: number | null = null;
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
      const tiers = account ? parseFeeConfig(account.data) : null;
      if (tiers) this.tiers = { tiers, at: Date.now() };
    });
  }

  stop(): void {
    for (const t of this.timers) clearInterval(t);
  }

  // Resolves once every value has had one chance to load, so a fresh start does not build on nothing.
  async ready(): Promise<void> {
    for (let i = 0; i < 50 && (!this.blockhash || !this.height || !this.tiers); i++) await new Promise((r) => setTimeout(r, 100));
  }

  // An old blockhash means the refresh has stopped; a transaction built on it may be near expiry.
  freshBlockhash(): { blockhash: string; lastValidBlockHeight: number } | null {
    return this.blockhash && Date.now() - this.blockhash.at <= BLOCKHASH_MAX_AGE_MS ? this.blockhash : null;
  }

  blockHeight(): number | null {
    return this.height?.blockHeight ?? null;
  }

  // Null once stale, so the gate charges the higher of a Token-2022 mint's two transfer fees.
  epoch(): number | null {
    return this.height && Date.now() - this.height.at <= EPOCH_MAX_AGE_MS ? this.height.epoch : null;
  }

  // When stale, the highest tier's fee applies at every market cap: quotes come out lower, so a
  // minimum-out never assumes a fee cut that may have ended.
  feeTiers(): FeeTier[] | null {
    if (!this.tiers) return null;
    if (Date.now() - this.tiers.at <= FEE_TIERS_MAX_AGE_MS) return this.tiers.tiers;
    const highest = this.tiers.tiers.reduce((a, b) => (a.protocolBps + a.creatorBps >= b.protocolBps + b.creatorBps ? a : b));
    return [{ ...highest, threshold: 0n }];
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
