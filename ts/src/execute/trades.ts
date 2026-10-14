// The trade state machine and the only owner of durable bot state: snipes used, SOL spent, buys
// made, open positions, and every trade in flight. Format: shared/ledger.schema.json. Rules:
// shared/README.md, section Trade state. execute/trade.ts drives it; nothing else writes state.
//
//   reserved --> sending --> confirmed
//      |            |----> failed        (the chain proved it failed)
//      |            '----> unknown --> reconciled (landed or expired, found later)
//      '--> failed (nothing was sent)
//
// Reading order: load, reservations, transitions, queries, persistence.

import { closeSync, fsyncSync, mkdirSync, openSync, readFileSync, renameSync, writeSync } from "node:fs";
import { dirname } from "node:path";
import { Ajv2020 } from "ajv/dist/2020.js";
import type { Actor } from "../detect/types.ts";

export type TradeState = "reserved" | "sending" | "confirmed" | "failed" | "unknown" | "reconciled";
export type Route = "pump" | "pumpportal" | "jupiter" | "dry_run";

// Why a trade exists: the rule that fired, the event, and the account that triggered it. Kept on
// the position so a delete of that post, or that caller or author leaving, can find it.
export type Trigger = { rule: string; fireKey: string; eventRef: string; postKey?: string; actor: Actor };

export type Trade = {
  id: string;
  side: "buy" | "sell";
  mint: string;
  route: Route;
  state: TradeState;
  createdAt: number;
  trigger: Trigger;
  reservedLamports: string; // largest possible wallet debit, held against the spend caps
  feeBudgetLamports: string; // what a transaction that lands and fails can still cost
  spentLamports?: string;
  signature?: string;
  lastValidBlockHeight?: number;
  tx?: string; // signed bytes, base64, kept until resolved so a restart can rebroadcast
  senders?: string[]; // where it was sent; a rebroadcast uses the same senders
  sentAt?: number;
  landed?: boolean;
  error?: string;
  tokenAmount?: string;
  exitIntents: Array<{ code: string; at: number; detail: string }>;
};

export type Position = {
  mint: string;
  route: "pump" | "jupiter";
  tokenProgram: string;
  openedAt: number;
  buyTradeId: string;
  costLamports: string;
  tokenAmount: string | null; // null until a balance read succeeds
  trigger: Trigger;
  sellTradeId?: string;
};

type State = {
  version: 1;
  mode: "live" | "dry_run";
  snipes: Record<string, number>;
  spend: { totalLamports: string; recent: Array<{ tradeId: string; at: number; lamports: string }> };
  purchases: Record<string, { firstAt: number; tradeId: string }>;
  trades: Record<string, Trade>;
  positions: Record<string, Position>;
};

export type SpendCaps = { maxPerTradeSol: number; maxPerHourSol: number; maxTotalSol: number };

// How a sent trade ended, as the sender, Jupiter, or the reconciler learned it.
export type Outcome =
  | { kind: "landed"; tokenAmount: bigint | null; spentLamports: bigint | null; signature?: string }
  | { kind: "failed"; error: string; spentLamports: bigint | null } // landed with an error
  | { kind: "expired" } // never landed and its blockhash expired
  | { kind: "rejected"; error: string } // refused before it could land (Jupiter /execute said so)
  | { kind: "unknown"; error: string };

const HOUR_MS = 3_600_000;
const KEEP_RESOLVED = 1_000;
export const solToLamports = (sol: number): bigint => BigInt(Math.round(sol * 1e9));
const ACTIVE: TradeState[] = ["reserved", "sending", "unknown"];
const isActive = (t: Trade) => ACTIVE.includes(t.state);

const SCHEMA = JSON.parse(readFileSync(new URL("../../../shared/ledger.schema.json", import.meta.url), "utf8"));
const validate = new Ajv2020({ allErrors: false, strict: true, strictTypes: false }).compile(SCHEMA);

export class StateError extends Error {}

export class TradeBook {
  private readonly path: string | null;
  private readonly state: State;
  private readonly snipes: Map<string, number>; // a Map, so a rule named "constructor" is just a name
  private readonly active = new Map<string, Trade>(); // "buy:<mint>" -> the trade in flight, for O(1) checks
  private nextId = 0;

  private constructor(path: string | null, state: State) {
    this.path = path;
    this.state = state;
    this.snipes = new Map(Object.entries(state.snipes));
    for (const t of Object.values(state.trades)) if (isActive(t)) this.active.set(`${t.side}:${t.mint}`, t);
  }

  // ---- load ----

  // A null path keeps everything in memory (tests, the demo). A file is created only when it does
  // not exist; any other read or validation problem stops startup, because a reset file would let
  // the bot buy the same mints again.
  static open(path: string | null, mode: State["mode"]): TradeBook {
    const empty: State = { version: 1, mode, snipes: {}, spend: { totalLamports: "0", recent: [] }, purchases: {}, trades: {}, positions: {} };
    if (!path) return new TradeBook(null, empty);
    let text: string;
    try {
      text = readFileSync(path, "utf8");
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw new StateError(`cannot read ${path}: ${(error as Error).message}`);
      mkdirSync(dirname(path), { recursive: true });
      const book = new TradeBook(path, empty);
      book.save();
      return book;
    }
    let json: unknown;
    try {
      json = JSON.parse(text);
    } catch {
      throw new StateError(`${path} is not valid JSON. Fix or move it; deleting it lets the bot buy the same mints again.`);
    }
    if (!validate(json)) {
      const e = validate.errors?.[0];
      throw new StateError(`${path} does not match shared/ledger.schema.json (${e?.instancePath || "/"} ${e?.message}). Fix or move it.`);
    }
    const state = json as State;
    if (state.mode !== mode) throw new StateError(`${path} holds ${state.mode} state, but the bot runs in ${mode} mode.`);
    return new TradeBook(path, state);
  }

  // A trade still "reserved" after a restart never reached "sending", and "sending" is written
  // before anything goes on the wire, so nothing was sent. Returns the trades it closed.
  recoverAfterRestart(): Trade[] {
    const closed = Object.values(this.state.trades).filter((t) => t.state === "reserved");
    for (const t of closed) this.failBeforeSend(t.id, "the bot stopped before sending");
    return closed;
  }

  // ---- reservations (synchronous: nothing can interleave between check and record) ----

  reserveSnipe(rule: string, maxSnipes: number): boolean {
    const used = this.snipes.get(rule) ?? 0;
    if (maxSnipes > 0 && used >= maxSnipes) return false;
    this.snipes.set(rule, used + 1);
    return true;
  }

  releaseSnipe(rule: string): void {
    this.snipes.set(rule, Math.max(0, (this.snipes.get(rule) ?? 1) - 1));
  }

  // A buy is refused when the mint was bought before (and the rule says skip), when another buy of
  // it is in flight, when the bot still holds it, or when the spend caps would be passed.
  reserveBuy(o: {
    mint: string;
    trigger: Trigger;
    route: Route;
    maxDebit: bigint;
    feeBudget: bigint;
    caps: SpendCaps;
    skipIfAlreadyBought: boolean;
    skipIfAlreadyHolding: boolean;
    now: number;
  }): Trade | string {
    if (this.activeTrade(o.mint, "buy")) return "another buy of this mint is in flight";
    if (o.skipIfAlreadyBought && this.state.purchases[o.mint]) return `already bought (trade ${this.state.purchases[o.mint]!.tradeId})`;
    if (o.skipIfAlreadyHolding && this.state.positions[o.mint]) return "the bot still holds this token";
    const capped = this.checkSpend(o.maxDebit, o.caps, o.now);
    if (capped) return capped;
    const trade = this.newTrade("buy", o.mint, o.route, o.trigger, o.maxDebit, o.feeBudget, o.now);
    this.state.spend.recent.push({ tradeId: trade.id, at: o.now, lamports: o.maxDebit.toString() });
    this.state.spend.totalLamports = (BigInt(this.state.spend.totalLamports) + o.maxDebit).toString();
    return trade;
  }

  // One sell at a time per position. A sell that is sending or unknown must resolve first; the
  // reconciler rebroadcasts it meanwhile.
  reserveSell(mint: string, trigger: Trigger, route: Route, now: number): Trade | string {
    const position = this.state.positions[mint];
    if (!position) return "no position";
    const pending = position.sellTradeId ? this.state.trades[position.sellTradeId] : undefined;
    if (pending && isActive(pending)) return `sell ${pending.id} is still ${pending.state}`;
    const trade = this.newTrade("sell", mint, route, trigger, 0n, 0n, now);
    position.sellTradeId = trade.id;
    return trade;
  }

  // ---- transitions ----

  // Written to disk, with fsync, before the transaction goes on the wire. If this throws the
  // caller must not send, and must call failBeforeSend.
  markSending(id: string, sent: { signature: string; lastValidBlockHeight: number; tx: string; route: Route; senders: string[] }, now: number): void {
    const t = this.expect(id, ["reserved"]);
    const before = { ...t };
    Object.assign(t, { state: "sending", sentAt: now, ...sent });
    try {
      this.save();
    } catch (error) {
      for (const key of Object.keys(t) as Array<keyof Trade>) if (!(key in before)) delete t[key];
      Object.assign(t, before); // still "reserved": the caller fails it and sends nothing
      throw error;
    }
  }

  // Nothing was sent: the filters said no, a build failed, or the pre-send write failed.
  // Everything reserved for the trade comes back.
  failBeforeSend(id: string, reason: string): void {
    const t = this.expect(id, ["reserved"]);
    Object.assign(t, { state: "failed", landed: false, error: reason });
    if (t.side === "buy") this.settleSpend(t, 0n);
    else this.clearSell(t);
    this.trySave();
  }

  // The result of a sent trade. Returns the exit intents to act on now (a buy that landed while
  // exits were requested for it). "unknown" holds every reservation until the reconciler finds out.
  resolve(id: string, outcome: Outcome, now: number): Trade["exitIntents"] {
    const t = this.expect(id, ["sending", "unknown"]);
    if (outcome.kind === "unknown") {
      Object.assign(t, { state: "unknown", error: outcome.error });
      // A buy that may have landed counts as bought, so a repeat signal cannot buy it twice.
      if (t.side === "buy") this.state.purchases[t.mint] ??= { firstAt: now, tradeId: t.id };
      this.save();
      return [];
    }
    const final: TradeState =
      t.state === "unknown" ? "reconciled" : outcome.kind === "landed" ? "confirmed" : outcome.kind === "expired" ? "reconciled" : "failed";
    t.state = final;
    t.landed = outcome.kind === "landed" || outcome.kind === "failed";
    let intents: Trade["exitIntents"] = [];
    if (outcome.kind === "landed") {
      if (outcome.signature) t.signature = outcome.signature;
      if (outcome.tokenAmount !== null) t.tokenAmount = outcome.tokenAmount.toString();
      if (t.side === "buy") {
        this.settleSpend(t, outcome.spentLamports);
        this.state.purchases[t.mint] ??= { firstAt: now, tradeId: t.id };
        intents = t.exitIntents;
      } else {
        delete this.state.positions[t.mint];
      }
    } else {
      if (outcome.kind === "failed" || outcome.kind === "rejected") t.error = outcome.error;
      if (t.side === "buy") {
        // A failed transaction still paid its fees; an expired one paid nothing.
        this.settleSpend(t, outcome.kind === "failed" ? (outcome.spentLamports ?? BigInt(t.feeBudgetLamports)) : 0n);
        if (this.state.purchases[t.mint]?.tradeId === t.id) delete this.state.purchases[t.mint];
      } else {
        this.clearSell(t);
      }
    }
    delete t.tx;
    this.save();
    return intents;
  }

  // Dry run: a trade goes straight from reserved to confirmed, with its reservation as its cost.
  confirmDryRun(id: string, now: number): Trade["exitIntents"] {
    const t = this.expect(id, ["reserved"]);
    if (t.route !== "dry_run") throw new StateError(`trade ${id} is live; only dry-run trades confirm without sending`);
    Object.assign(t, { state: "confirmed", landed: true, spentLamports: t.reservedLamports });
    if (t.side === "buy") this.state.purchases[t.mint] ??= { firstAt: now, tradeId: t.id };
    else delete this.state.positions[t.mint];
    this.trySave();
    return t.exitIntents;
  }

  // A dry run or a landed buy opens a position. tokenAmount null means "not read yet".
  openPosition(p: Omit<Position, "sellTradeId">): void {
    this.state.positions[p.mint] = p;
    this.save();
  }

  setTokenAmount(mint: string, amount: bigint): void {
    const p = this.state.positions[mint];
    if (!p) return;
    p.tokenAmount = amount.toString();
    this.save();
  }

  // An exit asked for while the buy is still pending is kept on the buy and applied when it lands.
  addExitIntent(mint: string, code: string, detail: string, now: number): "pending" | "position" | "none" {
    const buy = this.activeTrade(mint, "buy");
    if (buy) {
      buy.exitIntents.push({ code, at: now, detail });
      this.trySave();
      return "pending";
    }
    return this.state.positions[mint] ? "position" : "none";
  }

  // ---- queries ----

  trade(id: string): Trade | undefined {
    return this.state.trades[id];
  }

  position(mint: string): Position | undefined {
    return this.state.positions[mint];
  }

  positions(): Position[] {
    return Object.values(this.state.positions);
  }

  // Sent trades whose outcome the reconciler still has to find.
  unresolved(): Trade[] {
    return Object.values(this.state.trades).filter((t) => t.state === "sending" || t.state === "unknown");
  }

  // Positions and pending buys an exit applies to: the ones bought on a post (delete), or the ones
  // a rule bought because of this account (caller and author exits).
  exitTargets(match: { postKey: string } | { rule: string; actor: Actor }): string[] {
    const triggers = [
      ...this.positions().map((p) => ({ mint: p.mint, trigger: p.trigger })),
      ...Object.values(this.state.trades).filter((t) => t.side === "buy" && isActive(t)).map((t) => ({ mint: t.mint, trigger: t.trigger })),
    ];
    const hits = triggers.filter(({ trigger }) =>
      "postKey" in match ? trigger.postKey === match.postKey : trigger.rule === match.rule && sameActor(trigger.actor, match.actor),
    );
    return [...new Set(hits.map((h) => h.mint))];
  }

  // ---- internals ----

  private newTrade(side: Trade["side"], mint: string, route: Route, trigger: Trigger, reserved: bigint, feeBudget: bigint, now: number): Trade {
    const trade: Trade = {
      id: `${now.toString(36)}-${(this.nextId++).toString(36)}`,
      side,
      mint,
      route,
      state: "reserved",
      createdAt: now,
      trigger,
      reservedLamports: reserved.toString(),
      feeBudgetLamports: feeBudget.toString(),
      exitIntents: [],
    };
    this.state.trades[trade.id] = trade;
    this.active.set(`${side}:${mint}`, trade);
    return trade;
  }

  private activeTrade(mint: string, side: Trade["side"]): Trade | undefined {
    const t = this.active.get(`${side}:${mint}`);
    if (t && !isActive(t)) this.active.delete(`${side}:${mint}`); // resolved since: forget it
    return t && isActive(t) ? t : undefined;
  }

  private expect(id: string, states: TradeState[]): Trade {
    const t = this.state.trades[id];
    if (!t || !states.includes(t.state)) throw new StateError(`trade ${id} is ${t?.state ?? "missing"}, expected ${states.join(" or ")}`);
    return t;
  }

  private checkSpend(lamports: bigint, caps: SpendCaps, now: number): string | null {
    if (lamports > solToLamports(caps.maxPerTradeSol)) return `the largest possible debit is above spend.maxPerTradeSol (${caps.maxPerTradeSol} SOL)`;
    this.state.spend.recent = this.state.spend.recent.filter((s) => now - s.at < HOUR_MS);
    const hour = this.state.spend.recent.reduce((sum, s) => sum + BigInt(s.lamports), 0n);
    if (hour + lamports > solToLamports(caps.maxPerHourSol)) return `spend.maxPerHourSol (${caps.maxPerHourSol} SOL) reached`;
    if (BigInt(this.state.spend.totalLamports) + lamports > solToLamports(caps.maxTotalSol)) return `spend.maxTotalSol (${caps.maxTotalSol} SOL) reached`;
    return null;
  }

  // Replaces the reservation with what the trade actually cost. Unknown actual cost keeps it.
  private settleSpend(t: Trade, actual: bigint | null): void {
    if (actual === null) return;
    const reserved = BigInt(t.spentLamports ?? t.reservedLamports);
    t.spentLamports = actual.toString();
    this.state.spend.totalLamports = (BigInt(this.state.spend.totalLamports) - reserved + actual).toString();
    const entry = this.state.spend.recent.find((s) => s.tradeId === t.id);
    if (entry) entry.lamports = actual.toString();
  }

  private clearSell(t: Trade): void {
    const p = this.state.positions[t.mint];
    if (p?.sellTradeId === t.id) delete p.sellTradeId;
  }

  // Temp file, fsync, rename, fsync the directory: after a crash the file is either the old state
  // or the new one, never half of each.
  private save(): void {
    this.prune();
    if (!this.path) return;
    this.state.snipes = Object.fromEntries(this.snipes);
    const tmp = `${this.path}.tmp`;
    const fd = openSync(tmp, "w");
    try {
      writeSync(fd, JSON.stringify(this.state, null, 2));
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

  // For transitions after the money question is settled: a failed write is retried by the next one.
  private trySave(): void {
    try {
      this.save();
    } catch (error) {
      console.error(`state write failed, the next change retries it: ${(error as Error).message}`);
    }
  }

  flush(): void {
    this.trySave();
  }

  // Keeps the newest resolved trades and any a position still points to; active trades always stay.

  private prune(): void {
    const resolved = Object.values(this.state.trades).filter((t) => !isActive(t));
    const keep = new Set(this.positions().flatMap((p) => [p.buyTradeId, p.sellTradeId ?? ""]));
    for (const t of resolved.sort((a, b) => a.createdAt - b.createdAt).slice(0, Math.max(0, resolved.length - KEEP_RESOLVED))) {
      if (!keep.has(t.id)) delete this.state.trades[t.id];
    }
  }
}

function sameActor(a: Actor, b: Actor): boolean {
  if (a.namespace !== b.namespace) return false;
  if (a.id && b.id) return a.id === b.id;
  return !!a.handle && !!b.handle && a.handle.toLowerCase() === b.handle.toLowerCase();
}
