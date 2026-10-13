// Durable bot state in state/ledger.json: mints already traded, snipes used per rule, SOL spent,
// and open positions. Reservations are synchronous and in memory, so two triggers for one mint in
// the same millisecond cannot both buy; the file write follows a few ms later.

import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { rename, writeFile } from "node:fs/promises";
import { dirname } from "node:path";

export type MintStatus = "bought" | "dry_run" | "rejected" | "failed" | "unknown" | "exited";

type MintRecord = { status: MintStatus; buys: number; firstSeenAt: number; eventRef: string };

export type Position = {
  mint: string;
  route: "pump" | "jupiter";
  tokenProgram: string;
  tokenAmount: string | null; // raw token units; null when not known yet
  costLamports: string;
  openedAt: number;
  postKey?: string;
  rule: string;
  dryRun: boolean;
};

type State = {
  version: 1;
  mints: Record<string, MintRecord>;
  snipes: Record<string, number>; // rule name -> snipes used
  spentLamportsTotal: string;
  recentSpends: Array<{ at: number; lamports: string }>;
  positions: Record<string, Position>;
};

type SpendCaps = { maxPerTradeSol: number; maxPerHourSol: number; maxTotalSol: number };

const HOUR_MS = 3_600_000;
export const solToLamports = (sol: number): bigint => BigInt(Math.round(sol * 1e9));
// Statuses that count as "already bought": a send with an unknown outcome may have landed.
const HELD: MintStatus[] = ["bought", "dry_run", "unknown"];

export class Ledger {
  private readonly inflight = new Set<string>(); // mints with a trade being built or sent
  private writing: Promise<void> = Promise.resolve();
  private queued = false;

  private readonly path: string | null;
  private readonly state: State;

  private constructor(path: string | null, state: State) {
    this.path = path;
    this.state = state;
  }

  // A null path keeps everything in memory (tests and the demo).
  static open(path: string | null): Ledger {
    const empty = (): State => ({ version: 1, mints: {}, snipes: {}, spentLamportsTotal: "0", recentSpends: [], positions: {} });
    if (!path) return new Ledger(null, empty());
    mkdirSync(dirname(path), { recursive: true });
    let raw: string;
    try {
      raw = readFileSync(path, "utf8");
    } catch {
      writeFileSync(path, JSON.stringify(empty()));
      return new Ledger(path, empty());
    }
    try {
      return new Ledger(path, { ...empty(), ...(JSON.parse(raw) as State) });
    } catch {
      // Never reset a damaged ledger silently: it is what stops a restart from buying again.
      throw new Error(`${path} is not valid JSON. Fix or move it; deleting it lets the bot buy the same mints again.`);
    }
  }

  // First trigger per mint wins. Check and record happen with no await in between.
  tryReserveMint(mint: string, skipIfAlreadyBought: boolean, eventRef: string, now: number): string | null {
    if (this.inflight.has(mint)) return "another trigger for this mint is already in flight";
    const record = this.state.mints[mint];
    if (skipIfAlreadyBought && record && HELD.includes(record.status)) return `already ${record.status} (${record.eventRef})`;
    this.inflight.add(mint);
    if (!record) this.state.mints[mint] = { status: "rejected", buys: 0, firstSeenAt: now, eventRef };
    return null;
  }

  finishMint(mint: string, status: MintStatus): void {
    this.inflight.delete(mint);
    const record = this.state.mints[mint];
    if (!record) return;
    record.status = status;
    if (HELD.includes(status)) record.buys += 1;
    this.save();
  }

  tryReserveSnipe(rule: string, maxSnipes: number): boolean {
    const used = this.state.snipes[rule] ?? 0;
    if (maxSnipes > 0 && used >= maxSnipes) return false;
    this.state.snipes[rule] = used + 1;
    this.save();
    return true;
  }

  // Give a snipe back when the filters rejected the trade or nothing was sent.
  releaseSnipe(rule: string): void {
    this.state.snipes[rule] = Math.max(0, (this.state.snipes[rule] ?? 1) - 1);
    this.save();
  }

  // Spend caps are reserved before a transaction is built and refunded only if nothing was sent.
  trySpend(lamports: bigint, caps: SpendCaps, now: number): string | null {
    if (lamports > solToLamports(caps.maxPerTradeSol)) return `amount is above spend.maxPerTradeSol (${caps.maxPerTradeSol} SOL)`;
    this.state.recentSpends = this.state.recentSpends.filter((s) => now - s.at < HOUR_MS);
    const hour = this.state.recentSpends.reduce((sum, s) => sum + BigInt(s.lamports), 0n);
    if (hour + lamports > solToLamports(caps.maxPerHourSol)) return `spend.maxPerHourSol (${caps.maxPerHourSol} SOL) reached`;
    const total = BigInt(this.state.spentLamportsTotal);
    if (total + lamports > solToLamports(caps.maxTotalSol)) return `spend.maxTotalSol (${caps.maxTotalSol} SOL) reached`;
    this.state.recentSpends.push({ at: now, lamports: lamports.toString() });
    this.state.spentLamportsTotal = (total + lamports).toString();
    this.save();
    return null;
  }

  refundSpend(lamports: bigint, at: number): void {
    const i = this.state.recentSpends.findIndex((s) => s.at === at && s.lamports === lamports.toString());
    if (i >= 0) this.state.recentSpends.splice(i, 1);
    this.state.spentLamportsTotal = (BigInt(this.state.spentLamportsTotal) - lamports).toString();
    this.save();
  }

  openPosition(position: Position): void {
    this.state.positions[position.mint] = position;
    this.save();
  }

  closePosition(mint: string): void {
    delete this.state.positions[mint];
    const record = this.state.mints[mint];
    if (record) record.status = "exited";
    this.save();
  }

  position(mint: string): Position | undefined {
    return this.state.positions[mint];
  }

  positions(): Position[] {
    return Object.values(this.state.positions);
  }

  mintsForPost(postKey: string): string[] {
    return this.positions().filter((p) => p.postKey === postKey).map((p) => p.mint);
  }

  // Serialized, atomic writes (temp file then rename): a crash never leaves half a file.
  private save(): void {
    if (!this.path || this.queued) return;
    this.queued = true;
    const path = this.path;
    this.writing = this.writing
      .then(async () => {
        this.queued = false;
        await writeFile(`${path}.tmp`, JSON.stringify(this.state, null, 2));
        await rename(`${path}.tmp`, path);
      })
      .catch((error: Error) => console.error(`ledger write failed, will retry on the next change: ${error.message}`));
  }

  flushSync(): void {
    if (!this.path) return;
    writeFileSync(`${this.path}.tmp`, JSON.stringify(this.state, null, 2));
    renameSync(`${this.path}.tmp`, this.path);
  }
}
