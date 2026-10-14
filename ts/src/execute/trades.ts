// The bot's durable state, and the only code that writes it: mints bought, SOL spent, positions
// held, and trades whose outcome is not known yet. Format: shared/ledger.schema.json (version 2).
// Rules: shared/README.md, section Trade state. execute/trade.ts drives it.
//
//   buy:  begin -> recordPending (written before sending) -> resolve (one write) -> position
//   sell: begin -> recordPending -> resolve -> position removed
//
// Reading order: load and lock, buys and sells, outcomes, positions, persistence.

import { closeSync, fsyncSync, mkdirSync, openSync, readFileSync, renameSync, rmSync, writeFileSync, writeSync } from "node:fs";
import { dirname } from "node:path";
import { Ajv2020 } from "ajv/dist/2020.js";

export type Trigger = { eventRef: string; postKey?: string };
export type PendingExit = { code: string; at: number };

export type Position = {
  rule: string;
  trigger: Trigger;
  route: "pump" | "jupiter";
  tokenProgram: string;
  openedAt: number;
  costLamports: string;
  tokenAmount: string | null; // null until a balance read returns more than 0
  pendingExits: PendingExit[]; // kept until a sell confirms
};

export type Pending = {
  side: "buy" | "sell";
  mint: string;
  route: "pump" | "jupiter";
  signature: string | null; // null: Jupiter gave none and the transaction had none of ours yet
  blockhash: string;
  lastValidBlockHeight: number | null;
  reservedLamports: string; // largest possible wallet debit, held against the spend caps
  amountLamports: string; // what a buy spends on the token; the position's cost
  rule: string;
  trigger: Trigger;
  tokenProgram: string;
  sentAt: number;
};

// What a dry-run buy records: a pending entry without the parts only a sent transaction has.
export type DryRunBuy = Omit<Pending, "side" | "signature" | "blockhash" | "lastValidBlockHeight" | "sentAt">;

type State = {
  version: 2;
  mode: "live" | "dry_run";
  purchases: Record<string, { rule: string; at: number }>;
  spend: { totalLamports: string; recent: Array<{ tradeId: string; at: number; lamports: string }> };
  positions: Record<string, Position>;
  pending: Record<string, Pending>;
};

export type SpendCaps = { maxPerTradeSol: number; maxPerHourSol: number; maxTotalSol: number };

// How a sent trade ended. Unknown changes nothing except a signature learned meanwhile.
export type Outcome =
  | { kind: "landed"; spentLamports: bigint | null; tokenAmount: bigint | null }
  | { kind: "failed"; error: string; spentLamports: bigint | null } // landed with an error
  | { kind: "expired" } // not found, and its blockhash is no longer valid
  | { kind: "rejected"; error: string } // Jupiter refused it before it could land
  | { kind: "unknown"; error: string; signature?: string };

const HOUR_MS = 3_600_000;
export const solToLamports = (sol: number): bigint => BigInt(Math.round(sol * 1e9));

const SCHEMA = JSON.parse(readFileSync(new URL("../../../shared/ledger.schema.json", import.meta.url), "utf8"));
const validate = new Ajv2020({ allErrors: false, strict: true, strictTypes: false }).compile(SCHEMA);

export class StateError extends Error {}

export class TradeBook {
  private readonly path: string | null;
  private readonly state: State;
  private readonly inProgress = new Set<string>(); // mints this process is buying or selling now
  private readonly claims = new Map<string, number>(); // rule -> fires not yet recorded as a buy
  private nextId = 0;

  private constructor(path: string | null, state: State) {
    this.path = path;
    this.state = state;
  }

  // ---- load and lock ----

  // A null path keeps everything in memory (tests, the demo). Otherwise the lock comes first, then
  // the file: created only when missing; anything unreadable, invalid, or of the other mode stops
  // startup, because a reset file would let the bot buy the same mints again.
  static open(path: string | null, mode: State["mode"]): TradeBook {
    const empty: State = { version: 2, mode, purchases: {}, spend: { totalLamports: "0", recent: [] }, positions: {}, pending: {} };
    if (!path) return new TradeBook(null, empty);
    mkdirSync(dirname(path), { recursive: true });
    lock(lockPath(path));
    try {
      const book = new TradeBook(path, load(path, mode) ?? empty);
      book.save();
      return book;
    } catch (error) {
      rmSync(lockPath(path), { force: true });
      throw error;
    }
  }

  close(): void {
    if (this.path) rmSync(lockPath(this.path), { force: true });
  }

  // ---- buys and sells ----

  // A rule's snipes are its purchases, its pending buys, and its fires not recorded yet. The
  // engine claims one when a rule fires; recording the buy uses it up, releaseSnipe gives it back.
  claimSnipe(rule: string, maxSnipes: number): boolean {
    const claimed = this.claims.get(rule) ?? 0;
    const bought = Object.values(this.state.purchases).filter((p) => p.rule === rule).length;
    const pending = Object.values(this.state.pending).filter((p) => p.side === "buy" && p.rule === rule).length;
    if (maxSnipes > 0 && claimed + bought + pending >= maxSnipes) return false;
    this.claims.set(rule, claimed + 1);
    return true;
  }

  releaseSnipe(rule: string): void {
    const claimed = this.claims.get(rule) ?? 0;
    if (claimed > 0) this.claims.set(rule, claimed - 1);
  }

  // One buy per mint, ever, and one trade per mint at a time. Returns why not, or null after
  // marking the mint as in progress until end().
  begin(mint: string, side: "buy" | "sell"): string | null {
    if (side === "buy" && this.state.purchases[mint]) return "already bought";
    if (side === "sell" && !this.state.positions[mint]) return "no position";
    if (this.inProgress.has(mint) || this.pendingFor(mint)) return "a trade of this mint is in progress";
    this.inProgress.add(mint);
    return null;
  }

  end(mint: string): void {
    this.inProgress.delete(mint);
  }

  newTradeId(now: number): string {
    return `${now.toString(36)}-${(this.nextId++).toString(36)}`;
  }

  // Written before the transaction goes out. A buy first checks its reservation against the spend
  // caps (returning why it does not fit) and reserves it in the same write. Throws when the write
  // fails, with nothing changed, so the caller sends nothing.
  recordPending(id: string, p: Pending, caps: SpendCaps, now: number): string | null {
    if (p.side === "buy") {
      const capped = this.checkSpend(BigInt(p.reservedLamports), caps, now);
      if (capped) return capped;
    }
    const spendBefore = structuredClone(this.state.spend);
    this.state.pending[id] = p;
    if (p.side === "buy") this.addSpend(id, BigInt(p.reservedLamports), now);
    try {
      this.save();
    } catch (error) {
      delete this.state.pending[id];
      this.state.spend = spendBefore;
      throw error;
    }
    if (p.side === "buy") this.releaseSnipe(p.rule); // counted from the pending buy from now on
    return null;
  }

  // Dry run: the buy is recorded as if it landed for its reservation, so limits behave as live.
  recordDryRunBuy(id: string, p: DryRunBuy, tokenAmount: bigint | null, caps: SpendCaps, now: number): string | null {
    const capped = this.checkSpend(BigInt(p.reservedLamports), caps, now);
    if (capped) return capped;
    this.addSpend(id, BigInt(p.reservedLamports), now);
    this.openPosition(p, tokenAmount, now);
    this.releaseSnipe(p.rule);
    this.trySave();
    return null;
  }

  recordDryRunSell(mint: string): void {
    delete this.state.positions[mint];
    this.trySave();
  }

  // ---- outcomes ----

  // One write per outcome; the table is in shared/README.md, section Trade state.
  resolve(id: string, outcome: Outcome, now: number): void {
    const p = this.state.pending[id];
    if (!p) throw new StateError(`trade ${id} is not pending`);
    if (outcome.kind === "unknown") {
      if (outcome.signature && outcome.signature !== p.signature) {
        p.signature = outcome.signature;
        this.trySave();
      }
      return;
    }
    delete this.state.pending[id];
    if (p.side === "buy") {
      if (outcome.kind === "landed") this.openPosition(p, outcome.tokenAmount, now);
      // A transaction that landed paid what it paid; one that never landed paid nothing.
      this.settleSpend(id, outcome.kind === "landed" || outcome.kind === "failed" ? outcome.spentLamports : 0n);
    } else if (outcome.kind === "landed") {
      delete this.state.positions[p.mint];
    }
    this.save();
  }

  pending(): Array<[string, Pending]> {
    return Object.entries(this.state.pending);
  }

  // ---- positions ----

  position(mint: string): Position | undefined {
    return this.state.positions[mint];
  }

  positions(): Array<[string, Position]> {
    return Object.entries(this.state.positions);
  }

  // Mints bought on this post, for delete exits.
  boughtOnPost(postKey: string): string[] {
    return this.positions()
      .filter(([, p]) => p.trigger.postKey === postKey)
      .map(([mint]) => mint);
  }

  // Kept on the position until a sell of it confirms. False when there is no position.
  addExit(mint: string, exit: PendingExit): boolean {
    const p = this.state.positions[mint];
    if (!p) return false;
    if (!p.pendingExits.some((e) => e.code === exit.code)) p.pendingExits.push(exit);
    this.trySave();
    return true;
  }

  // Mints to sell now: an exit waits, the balance is known, and no trade of the mint is running.
  positionsToExit(): string[] {
    return this.positions()
      .filter(([mint, p]) => p.pendingExits.length > 0 && p.tokenAmount !== null && !this.inProgress.has(mint) && !this.pendingFor(mint))
      .map(([mint]) => mint);
  }

  // A balance read right after a buy can still show 0 at the RPC; only a positive one is kept.
  setTokenAmount(mint: string, amount: bigint): void {
    const p = this.state.positions[mint];
    if (!p || amount <= 0n) return;
    p.tokenAmount = amount.toString();
    this.trySave();
  }

  flush(): void {
    this.trySave();
  }

  // ---- internals ----

  private pendingFor(mint: string): boolean {
    return Object.values(this.state.pending).some((p) => p.mint === mint);
  }

  private openPosition(p: DryRunBuy, tokenAmount: bigint | null, now: number): void {
    this.state.purchases[p.mint] = { rule: p.rule, at: now };
    this.state.positions[p.mint] = {
      rule: p.rule,
      trigger: p.trigger,
      route: p.route,
      tokenProgram: p.tokenProgram,
      openedAt: now,
      costLamports: p.amountLamports,
      tokenAmount: tokenAmount !== null && tokenAmount > 0n ? tokenAmount.toString() : null,
      pendingExits: [],
    };
  }

  private checkSpend(lamports: bigint, caps: SpendCaps, now: number): string | null {
    if (lamports > solToLamports(caps.maxPerTradeSol)) return `the largest possible debit is above spend.maxPerTradeSol (${caps.maxPerTradeSol} SOL)`;
    this.state.spend.recent = this.state.spend.recent.filter((s) => now - s.at < HOUR_MS);
    const hour = this.state.spend.recent.reduce((sum, s) => sum + BigInt(s.lamports), 0n);
    if (hour + lamports > solToLamports(caps.maxPerHourSol)) return `spend.maxPerHourSol (${caps.maxPerHourSol} SOL) reached`;
    if (BigInt(this.state.spend.totalLamports) + lamports > solToLamports(caps.maxTotalSol)) return `spend.maxTotalSol (${caps.maxTotalSol} SOL) reached`;
    return null;
  }

  private addSpend(tradeId: string, lamports: bigint, now: number): void {
    this.state.spend.recent.push({ tradeId, at: now, lamports: lamports.toString() });
    this.state.spend.totalLamports = (BigInt(this.state.spend.totalLamports) + lamports).toString();
  }

  // Replaces a buy's reservation with what it cost. An unknown cost keeps the reservation; a buy
  // older than the hour window is no longer in `recent` and keeps it too.
  private settleSpend(tradeId: string, actual: bigint | null): void {
    const entry = this.state.spend.recent.find((s) => s.tradeId === tradeId);
    if (actual === null || !entry) return;
    this.state.spend.totalLamports = (BigInt(this.state.spend.totalLamports) - BigInt(entry.lamports) + actual).toString();
    entry.lamports = actual.toString();
  }

  // ---- persistence ----

  // Temp file, fsync, rename, fsync the directory: after a crash the file is either the old state
  // or the new one, never half of each.
  private save(): void {
    if (!this.path) return;
    const tmp = `${this.path}.tmp`;
    const fd = openSync(tmp, "w");
    try {
      writeSync(fd, `${JSON.stringify(this.state, null, 2)}\n`);
      fsyncSync(fd);
    } finally {
      closeSync(fd);
    }
    renameSync(tmp, this.path);
    const dir = openSync(dirname(this.path), "r");
    try {
      fsyncSync(dir);
    } finally {
      closeSync(dir);
    }
  }

  // For writes after the money question is settled: a failed one is retried by the next write.
  private trySave(): void {
    try {
      this.save();
    } catch (error) {
      console.error(`state write failed, the next change retries it: ${(error as Error).message}`);
    }
  }
}

const lockPath = (statePath: string) => statePath.replace(/\.json$/, ".lock");

// null when the file does not exist yet.
function load(path: string, mode: State["mode"]): State | null {
  let text: string;
  try {
    text = readFileSync(path, "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return null;
    throw new StateError(`cannot read ${path}: ${(error as Error).message}`);
  }
  let json: { version?: unknown };
  try {
    json = JSON.parse(text);
  } catch {
    throw new StateError(`${path} is not valid JSON. Fix or move it; deleting it lets the bot buy the same mints again.`);
  }
  if (json.version === 1) throw new StateError(`${path} is a version 1 state file, which this bot does not read. Sell any tokens it lists, then move or delete it.`);
  if (!validate(json)) {
    const e = validate.errors?.[0];
    throw new StateError(`${path} does not match shared/ledger.schema.json (${e?.instancePath || "/"} ${e?.message}). Fix or move it.`);
  }
  const state = json as State;
  if (state.mode !== mode) throw new StateError(`${path} holds ${state.mode} state, but the bot runs in ${mode} mode.`);
  return state;
}

// Exclusive create. A lock whose process is gone, or that cannot be read, is taken over once.
function lock(path: string): void {
  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      writeFileSync(path, JSON.stringify({ pid: process.pid, startedAt: Date.now() }), { flag: "wx" });
      return;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw new StateError(`cannot create ${path}: ${(error as Error).message}`);
    }
    let pid: number | undefined;
    try {
      pid = (JSON.parse(readFileSync(path, "utf8")) as { pid?: number }).pid;
    } catch {
      pid = undefined;
    }
    if (pid !== undefined && alive(pid)) throw new StateError(`${path} is held by running process ${pid}. Stop it first: two bots on one state file could buy twice.`);
    rmSync(path, { force: true });
  }
  throw new StateError(`could not take ${path}: another process is starting at the same time.`);
}

function alive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM"; // the process exists under another user
  }
}
