// The state file through TradeBook's public methods: the lock, one buy per mint, pending trades
// and their outcomes, spend caps, sells and exits, and what a state file must look like.

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { TradeBook, type Pending } from "../src/execute/trades.ts";

const MINT = "FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA";
const OTHER = "H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29";
const THIRD = "7nHgAcf7tM2MuUF29mismBW2gKHBrXkGhJtU8LpqB6vN";
const caps = { maxPerTradeSol: 0.1, maxPerHourSol: 0.25, maxTotalSol: 0.3 };
const statePath = () => join(mkdtempSync(join(tmpdir(), "trades-")), "live.json");
const onDisk = (path: string) => JSON.parse(readFileSync(path, "utf8"));

const pending = (over: Partial<Pending> = {}): Pending => ({
  side: "buy",
  mint: MINT,
  route: "pump",
  signature: "sig-1",
  blockhash: "hash-1",
  lastValidBlockHeight: 1_150,
  reservedLamports: "60000000",
  amountLamports: "50000000",
  rule: "r",
  trigger: { eventRef: "tweet/content twitter:1", postKey: "twitter:1" },
  tokenProgram: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
  sentAt: 1,
  ...over,
});

// A buy through the same calls the trader makes, up to the pending write.
function sendBuy(book: TradeBook, id: string, over: Partial<Pending> = {}, now = 1): void {
  const p = pending(over);
  assert.equal(book.begin(p.mint, "buy"), null);
  assert.equal(book.recordPending(id, p, caps, now), null);
  book.end(p.mint);
}

function hold(book: TradeBook, mint = MINT): void {
  sendBuy(book, `buy-${mint}`, { mint });
  book.resolve(`buy-${mint}`, { kind: "landed", spentLamports: null, tokenAmount: 5n }, 2);
}

test("a buy is pending on disk before it is sent; landing removes it and opens the position in the same write", () => {
  const path = statePath();
  const book = TradeBook.open(path, "live");
  sendBuy(book, "b1");
  assert.deepEqual(onDisk(path).pending.b1, pending());
  assert.equal(onDisk(path).spend.totalLamports, "60000000");
  book.resolve("b1", { kind: "landed", spentLamports: 54_000_000n, tokenAmount: 5n }, 2);
  const state = onDisk(path);
  assert.deepEqual(state.pending, {});
  assert.deepEqual(state.purchases[MINT], { rule: "r", at: 2 });
  assert.equal(state.positions[MINT].costLamports, "50000000");
  assert.equal(state.positions[MINT].tokenAmount, "5");
  assert.equal(state.spend.totalLamports, "54000000", "spend settles to the actual debit");
  book.close();
});

test("an unknown outcome keeps the trade pending and the spend held; expiry refunds it and frees the mint", () => {
  const path = statePath();
  const book = TradeBook.open(path, "live");
  sendBuy(book, "b1");
  book.resolve("b1", { kind: "unknown", error: "timeout" }, 2);
  assert.equal(onDisk(path).spend.totalLamports, "60000000");
  assert.match(String(book.begin(MINT, "buy")), /in progress/);
  book.close();

  const reopened = TradeBook.open(path, "live");
  assert.deepEqual(reopened.pending().map(([id]) => id), ["b1"], "a restart finds the trade to reconcile");
  reopened.resolve("b1", { kind: "expired" }, 3);
  assert.equal(onDisk(path).spend.totalLamports, "0");
  assert.deepEqual(onDisk(path).purchases, {});
  assert.equal(reopened.begin(MINT, "buy"), null);
  reopened.close();
});

test("a buy that landed with an error keeps its actual cost, or the whole reservation when unknown, and is not a purchase", () => {
  const book = TradeBook.open(null, "live");
  sendBuy(book, "b1");
  book.resolve("b1", { kind: "failed", error: "6002", spentLamports: 7_000_000n }, 2);
  sendBuy(book, "b2", { mint: OTHER });
  book.resolve("b2", { kind: "failed", error: "6002", spentLamports: null }, 2);
  // 7,000,000 + 60,000,000 held; a 0.3 SOL total cap leaves 233,000,000 for the next buy.
  assert.match(String(book.recordPending("b3", pending({ mint: THIRD, reservedLamports: "233000001" }), { ...caps, maxPerTradeSol: 1, maxPerHourSol: 1 }, 3)), /maxTotalSol/);
  assert.equal(book.begin(MINT, "buy"), null, "the failed mint may be bought again");
});

test("a Jupiter rejection refunds the reservation; an unknown answer may store the signature Jupiter returned", () => {
  const path = statePath();
  const book = TradeBook.open(path, "live");
  sendBuy(book, "j1", { route: "jupiter", signature: null, lastValidBlockHeight: null });
  book.resolve("j1", { kind: "unknown", error: "Jupiter code -1001", signature: "from-jupiter" }, 2);
  assert.equal(onDisk(path).pending.j1.signature, "from-jupiter");
  book.resolve("j1", { kind: "rejected", error: "Jupiter code -2004" }, 3);
  assert.equal(onDisk(path).spend.totalLamports, "0");
  book.close();
});

test("the spend caps refuse a buy before anything is written: per trade, per hour, in total", () => {
  const path = statePath();
  const book = TradeBook.open(path, "live");
  assert.match(String(book.recordPending("x", pending({ reservedLamports: "100000001" }), caps, 0)), /maxPerTradeSol/);
  assert.deepEqual(onDisk(path).pending, {});
  sendBuy(book, "b1", { reservedLamports: "100000000" }, 0);
  sendBuy(book, "b2", { mint: OTHER, reservedLamports: "100000000" }, 1);
  // 0.2 SOL in the last hour; 0.25 allowed.
  assert.match(String(book.recordPending("b3", pending({ mint: THIRD, reservedLamports: "60000000" }), caps, 2)), /maxPerHourSol/);
  // An hour later the window is clear, and the 0.3 SOL total still holds the first two.
  assert.match(String(book.recordPending("b3", pending({ mint: THIRD, reservedLamports: "100000001" }), { ...caps, maxPerTradeSol: 1 }, 3_600_001)), /maxTotalSol/);
  assert.equal(book.recordPending("b3", pending({ mint: THIRD, reservedLamports: "100000000" }), caps, 3_600_001), null);
  book.close();
});

test("a failed pending write throws and leaves nothing behind, so nothing is sent", () => {
  const path = statePath();
  const book = TradeBook.open(path, "live");
  chmodSync(join(path, ".."), 0o500);
  try {
    assert.throws(() => book.recordPending("b1", pending(), caps, 1));
  } finally {
    chmodSync(join(path, ".."), 0o700);
  }
  assert.deepEqual(book.pending(), []);
  book.flush();
  assert.equal(onDisk(path).spend.totalLamports, "0");
  book.close();
});

test("one buy per mint, ever: a bought mint is refused after its sale, and so is one in progress", () => {
  const book = TradeBook.open(null, "live");
  assert.equal(book.begin(MINT, "buy"), null);
  assert.match(String(book.begin(MINT, "buy")), /in progress/);
  book.end(MINT);
  hold(book);
  assert.equal(book.begin(MINT, "sell"), null);
  book.recordPending("s1", pending({ side: "sell", reservedLamports: "0", amountLamports: "0" }), caps, 3);
  book.end(MINT);
  book.resolve("s1", { kind: "landed", spentLamports: null, tokenAmount: null }, 4);
  assert.equal(book.position(MINT), undefined);
  assert.equal(book.begin(MINT, "buy"), "already bought");
});

test("maxSnipes counts purchases, pending buys, and claims not yet written, per rule name", () => {
  const book = TradeBook.open(null, "live");
  assert.equal(book.claimSnipe("constructor", 2), true);
  assert.equal(book.claimSnipe("constructor", 2), true);
  assert.equal(book.claimSnipe("constructor", 2), false);
  book.releaseSnipe("constructor"); // one claim left
  sendBuy(book, "b1", { rule: "constructor" }); // the pending buy replaces the claim
  assert.equal(book.claimSnipe("constructor", 2), true); // one pending, one claimed
  assert.equal(book.claimSnipe("constructor", 2), false);
  book.resolve("b1", { kind: "expired" }, 2);
  assert.equal(book.claimSnipe("constructor", 2), true, "an expired buy gives its snipe back");
  const bought = TradeBook.open(null, "live");
  hold(bought); // rule "r", which then has no claims left over
  assert.equal(bought.claimSnipe("r", 1), false, "a purchase counts against maxSnipes");
});

test("a sell that lands with an error keeps the position and its exits; one sell at a time", () => {
  const path = statePath();
  const book = TradeBook.open(path, "live");
  hold(book);
  assert.equal(book.addExit(MINT, { code: "post_deleted", at: 3 }), true);
  assert.equal(book.addExit(OTHER, { code: "post_deleted", at: 3 }), false);
  assert.deepEqual(book.positionsToExit(), [MINT]);
  assert.equal(book.begin(MINT, "sell"), null);
  book.recordPending("s1", pending({ side: "sell", reservedLamports: "0", amountLamports: "0" }), caps, 4);
  book.end(MINT);
  assert.match(String(book.begin(MINT, "sell")), /in progress/);
  assert.deepEqual(book.positionsToExit(), []);
  book.resolve("s1", { kind: "failed", error: "slippage", spentLamports: 5_000n }, 5);
  assert.deepEqual(onDisk(path).positions[MINT].pendingExits, [{ code: "post_deleted", at: 3 }]);
  assert.deepEqual(book.positionsToExit(), [MINT]);
  book.close();
});

test("a position is sold only once its balance is known; a 0 read is ignored", () => {
  const book = TradeBook.open(null, "live");
  sendBuy(book, "b1");
  book.resolve("b1", { kind: "landed", spentLamports: null, tokenAmount: null }, 2);
  book.addExit(MINT, { code: "post_deleted", at: 3 });
  assert.deepEqual(book.positionsToExit(), []);
  book.setTokenAmount(MINT, 0n);
  assert.equal(book.position(MINT)!.tokenAmount, null);
  book.setTokenAmount(MINT, 42n);
  assert.deepEqual(book.positionsToExit(), [MINT]);
});

test("one process per state file: a live lock refuses, a dead or unreadable one is taken over", () => {
  const path = statePath();
  const lock = path.replace(/\.json$/, ".lock");
  const book = TradeBook.open(path, "live");
  assert.equal(JSON.parse(readFileSync(lock, "utf8")).pid, process.pid);
  assert.throws(() => TradeBook.open(path, "live"), /held by running process/);
  book.close();
  assert.equal(existsSync(lock), false);
  const dead = Number(spawnSync(process.execPath, ["-e", "process.stdout.write(String(process.pid))"]).stdout);
  writeFileSync(lock, JSON.stringify({ pid: dead, startedAt: 0 }));
  TradeBook.open(path, "live").close();
  writeFileSync(lock, "{half");
  TradeBook.open(path, "live").close();
  assert.equal(existsSync(lock), false);
});

test("a version 1, invalid, unreadable, or other-mode file stops startup and leaves no lock; only a missing file is created", () => {
  const path = statePath();
  const lock = path.replace(/\.json$/, ".lock");
  TradeBook.open(path, "live").close();
  const valid = onDisk(path);
  assert.equal(valid.version, 2);
  const refuses = (pattern: RegExp, mode: "live" | "dry_run" = "live") => {
    assert.throws(() => TradeBook.open(path, mode), pattern);
    assert.equal(existsSync(lock), false);
  };
  writeFileSync(path, JSON.stringify({ version: 1, mode: "live", snipes: {}, trades: {} }));
  refuses(/version 1 state file/);
  writeFileSync(path, JSON.stringify({ ...valid, pending: { x: {} } }));
  refuses(/ledger.schema.json/);
  writeFileSync(path, JSON.stringify(valid));
  refuses(/live state/, "dry_run");
  writeFileSync(path, "{not json");
  refuses(/buy the same mints again/);
  chmodSync(path, 0o000);
  try {
    refuses(/cannot read/);
  } finally {
    chmodSync(path, 0o600);
  }
});
