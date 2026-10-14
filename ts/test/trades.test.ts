// The trade state machine through its public methods: reservations, transitions, what survives a
// restart, and what the state file must look like.

import { test } from "node:test";
import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { TradeBook, type Trade, type Trigger } from "../src/execute/trades.ts";

const MINT = "FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA";
const caps = { maxPerTradeSol: 0.1, maxPerHourSol: 0.25, maxTotalSol: 0.25 };
const trigger: Trigger = { rule: "r", fireKey: "r|x", eventRef: "tweet/content twitter:1", postKey: "twitter:1", actor: { namespace: "x", id: "111" } };
const statePath = () => join(mkdtempSync(join(tmpdir(), "trades-")), "live.json");
const sent = (id: string) => ({ signature: `sig-${id}`, lastValidBlockHeight: 1_000, tx: "AAAA", route: "pump" as const, senders: ["rpc"] });

function buy(book: TradeBook, opts: { maxDebit?: bigint; skipIfAlreadyBought?: boolean; now?: number } = {}): Trade {
  const trade = book.reserveBuy({
    mint: MINT,
    trigger,
    route: "pump",
    maxDebit: opts.maxDebit ?? 60_000_000n,
    feeBudget: 10_000_000n,
    caps,
    skipIfAlreadyBought: opts.skipIfAlreadyBought ?? true,
    skipIfAlreadyHolding: true,
    now: opts.now ?? 0,
  });
  assert.equal(typeof trade, "object", String(trade));
  return trade as Trade;
}

const spentTotal = (path: string) => JSON.parse(readFileSync(path, "utf8")).spend.totalLamports;

test("an unknown outcome keeps the spend and blocks a repeat buy; expiry later refunds and frees the mint", () => {
  const path = statePath();
  const book = TradeBook.open(path, "live");
  const t = buy(book);
  book.markSending(t.id, sent(t.id), 1);
  book.resolve(t.id, { kind: "unknown", error: "timeout" }, 2);
  assert.equal(spentTotal(path), "60000000");
  assert.match(String(book.reserveBuy({ mint: MINT, trigger, route: "pump", maxDebit: 1n, feeBudget: 0n, caps, skipIfAlreadyBought: false, skipIfAlreadyHolding: true, now: 3 })), /in flight/);
  book.resolve(t.id, { kind: "expired" }, 4);
  assert.equal(book.trade(t.id)!.state, "reconciled");
  assert.equal(spentTotal(path), "0");
  buy(book, { now: 5 }); // the mint is free again
});

test("'sending' is on disk, with signature and signed bytes, before markSending returns", () => {
  const path = statePath();
  const book = TradeBook.open(path, "live");
  const t = buy(book);
  book.markSending(t.id, sent(t.id), 1);
  const onDisk = JSON.parse(readFileSync(path, "utf8")).trades[t.id];
  assert.equal(onDisk.state, "sending");
  assert.equal(onDisk.signature, `sig-${t.id}`);
  assert.equal(onDisk.tx, "AAAA");
  const reopened = TradeBook.open(path, "live");
  assert.deepEqual(reopened.unresolved().map((x) => x.id), [t.id], "a restart finds the trade to reconcile");
});

test("a reserved trade found after a restart was never sent: it fails and gives the spend back", () => {
  const path = statePath();
  const book = TradeBook.open(path, "live");
  const reserved = book.reserveBuy({ mint: "H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29", trigger, route: "pump", maxDebit: 20_000_000n, feeBudget: 1n, caps, skipIfAlreadyBought: true, skipIfAlreadyHolding: true, now: 0 }) as Trade;
  const sending = buy(book, { now: 1 });
  book.markSending(sending.id, sent(sending.id), 2); // this save also writes the reserved trade
  const reopened = TradeBook.open(path, "live");
  assert.deepEqual(reopened.recoverAfterRestart().map((t) => t.id), [reserved.id]);
  assert.equal(reopened.trade(reserved.id)!.state, "failed");
  assert.equal(reopened.trade(sending.id)!.state, "sending");
  assert.equal(spentTotal(path), "60000000", "only the sending trade's reservation remains");
});

test("a failed state write refuses the send and leaves the trade reserved", () => {
  const path = statePath();
  const book = TradeBook.open(path, "live");
  const t = buy(book);
  chmodSync(join(path, ".."), 0o500);
  try {
    assert.throws(() => book.markSending(t.id, sent(t.id), 1));
  } finally {
    chmodSync(join(path, ".."), 0o700);
  }
  book.failBeforeSend(t.id, "state write failed");
  assert.equal(book.trade(t.id)!.state, "failed");
  assert.equal(spentTotal(path), "0");
});

test("the spend reservation is the largest possible debit, settled to the actual one", () => {
  const book = TradeBook.open(null, "live");
  assert.match(String(book.reserveBuy({ mint: MINT, trigger, route: "pump", maxDebit: 100_000_001n, feeBudget: 1n, caps, skipIfAlreadyBought: true, skipIfAlreadyHolding: true, now: 0 })), /maxPerTradeSol/);
  const t = buy(book, { maxDebit: 100_000_000n });
  book.markSending(t.id, sent(t.id), 1);
  book.resolve(t.id, { kind: "landed", tokenAmount: 5n, spentLamports: 52_000_000n }, 2);
  assert.equal(book.trade(t.id)!.spentLamports, "52000000");
  // 0.25 SOL total cap: 0.052 spent, so a 0.1 SOL reservation still fits and a third does not.
  const second = book.reserveBuy({ mint: "H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29", trigger, route: "pump", maxDebit: 100_000_000n, feeBudget: 1n, caps, skipIfAlreadyBought: true, skipIfAlreadyHolding: true, now: 3 });
  assert.equal(typeof second, "object");
  assert.match(String(book.reserveBuy({ mint: "7nHgAcf7tM2MuUF29mismBW2gKHBrXkGhJtU8LpqB6vN", trigger, route: "pump", maxDebit: 100_000_000n, feeBudget: 1n, caps, skipIfAlreadyBought: true, skipIfAlreadyHolding: true, now: 4 })), /maxPerHourSol|maxTotalSol/);
});

test("a failed transaction keeps only its fee budget as spent", () => {
  const book = TradeBook.open(null, "live");
  const t = buy(book);
  book.markSending(t.id, sent(t.id), 1);
  book.resolve(t.id, { kind: "failed", error: "6002", spentLamports: null }, 2);
  assert.equal(book.trade(t.id)!.spentLamports, "10000000");
  buy(book, { now: 3 }); // a failed buy is not a purchase
});

test("an exited mint stays bought; skipIfAlreadyHolding still blocks while the position is open", () => {
  const book = TradeBook.open(null, "live");
  const t = buy(book);
  book.markSending(t.id, sent(t.id), 1);
  book.resolve(t.id, { kind: "landed", tokenAmount: 5n, spentLamports: null }, 2);
  book.openPosition({ mint: MINT, route: "pump", tokenProgram: "", openedAt: 2, buyTradeId: t.id, costLamports: "1", tokenAmount: "5", trigger });
  assert.match(String(book.reserveBuy({ mint: MINT, trigger, route: "pump", maxDebit: 1n, feeBudget: 0n, caps, skipIfAlreadyBought: false, skipIfAlreadyHolding: true, now: 3 })), /still holds/);
  const sell = book.reserveSell(MINT, trigger, "pump", 4) as Trade;
  book.markSending(sell.id, sent(sell.id), 5);
  book.resolve(sell.id, { kind: "landed", tokenAmount: null, spentLamports: null }, 6);
  assert.equal(book.position(MINT), undefined);
  assert.match(String(book.reserveBuy({ mint: MINT, trigger, route: "pump", maxDebit: 1n, feeBudget: 0n, caps, skipIfAlreadyBought: true, skipIfAlreadyHolding: true, now: 7 })), /already bought/);
});

test("one sell at a time: an unknown sell blocks the next until it resolves", () => {
  const book = TradeBook.open(null, "live");
  book.openPosition({ mint: MINT, route: "pump", tokenProgram: "", openedAt: 0, buyTradeId: "b", costLamports: "1", tokenAmount: "5", trigger });
  const first = book.reserveSell(MINT, trigger, "pump", 1) as Trade;
  book.markSending(first.id, sent(first.id), 2);
  book.resolve(first.id, { kind: "unknown", error: "timeout" }, 3);
  assert.match(String(book.reserveSell(MINT, trigger, "pump", 4)), /still unknown/);
  book.resolve(first.id, { kind: "expired" }, 5);
  assert.equal(typeof book.reserveSell(MINT, trigger, "pump", 6), "object");
});

test("an exit asked for during a pending buy is returned when the buy lands", () => {
  const book = TradeBook.open(null, "live");
  const t = buy(book);
  book.markSending(t.id, sent(t.id), 1);
  assert.equal(book.addExitIntent(MINT, "post_deleted", "deleted", 2), "pending");
  const intents = book.resolve(t.id, { kind: "landed", tokenAmount: null, spentLamports: null }, 3);
  assert.deepEqual(intents.map((i) => i.code), ["post_deleted"]);
});

test("exit targets match the triggering account, not just the rule", () => {
  const book = TradeBook.open(null, "live");
  book.openPosition({ mint: MINT, route: "pump", tokenProgram: "", openedAt: 0, buyTradeId: "b", costLamports: "1", tokenAmount: "5", trigger: { ...trigger, actor: { namespace: "pump_fun", handle: "CallerOne" } } });
  assert.deepEqual(book.exitTargets({ rule: "r", actor: { namespace: "pump_fun", handle: "callerone" } }), [MINT]);
  assert.deepEqual(book.exitTargets({ rule: "r", actor: { namespace: "pump_fun", handle: "OtherCaller" } }), []);
  assert.deepEqual(book.exitTargets({ postKey: "twitter:1" }), [MINT]);
});

test("snipes are counted per rule name, including names that look like object keys", () => {
  const book = TradeBook.open(null, "live");
  assert.equal(book.reserveSnipe("constructor", 1), true);
  assert.equal(book.reserveSnipe("constructor", 1), false);
  book.releaseSnipe("constructor");
  assert.equal(book.reserveSnipe("constructor", 1), true);
});

test("a state file of another version, mode, or shape stops startup; only a missing file is created", () => {
  const path = statePath();
  TradeBook.open(path, "live");
  const valid = JSON.parse(readFileSync(path, "utf8"));
  writeFileSync(path, JSON.stringify({ ...valid, version: 999 }));
  assert.throws(() => TradeBook.open(path, "live"), /ledger.schema.json/);
  writeFileSync(path, JSON.stringify(valid));
  assert.throws(() => TradeBook.open(path, "dry_run"), /live state/);
  writeFileSync(path, "{not json");
  assert.throws(() => TradeBook.open(path, "live"), /buy the same mints again/);
  chmodSync(path, 0o000);
  try {
    assert.throws(() => TradeBook.open(path, "live"), /cannot read/);
  } finally {
    chmodSync(path, 0o600);
  }
});
