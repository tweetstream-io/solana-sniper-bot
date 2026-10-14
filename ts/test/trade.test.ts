// Trades end to end through Trader: chain reads from ScenarioNetwork, sends to a local stand-in RPC
// over HTTP, confirmations from a local stand-in PubSub WebSocket, state in a real file.

import { test } from "node:test";
import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, readFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { WebSocketServer } from "ws";
import bs58 from "bs58";
import { Keypair, PublicKey, SystemProgram, TransactionMessage, VersionedTransaction } from "@solana/web3.js";
import { parseConfig } from "../src/setup/config.ts";
import { TradeBook } from "../src/execute/trades.ts";
import { pumpBuyReservation, Trader } from "../src/execute/trade.ts";
import { ChainCache } from "../src/execute/cache.ts";
import { Lander, SignatureWatcher, type SenderTarget } from "../src/land/land.ts";
import { LatencyReport, type Trace } from "../src/report/latency.ts";
import type { Fire } from "../src/rules/modes.ts";
import type { JupiterExecute, JupiterOrder } from "../src/network.ts";
import { ScenarioNetwork, type MintSpec, type ScenarioChain } from "../scripts/scenario-network.ts";
import { ExitWatcher } from "../src/exits/exits.ts";

const PUMP_MINT = "FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA";
const PLAIN_MINT = "H6oB2ziYZe3QqwHAyxhGLDWusSdktBY15gxoi8crru29";
const AUTHORITY_MINT = "7nHgAcf7tM2MuUF29mismBW2gKHBrXkGhJtU8LpqB6vN";
const TOKEN_2022 = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const curve = { virtualTokenReserves: "1000000000000000", virtualQuoteReserves: "32000000000", realTokenReserves: "720000000000000", realQuoteReserves: "2000000000", tokenTotalSupply: "1000000000000000", complete: false };
const mintSpec = (over: Partial<MintSpec>): MintSpec => ({ tokenProgram: "token-2022", mintAuthority: false, freezeAuthority: false, supply: "1000000000000000", decimals: 6, curve: null, ...over });
const chain = (): ScenarioChain => ({
  solUsd: 150,
  priorityFeeMicroLamports: 10_000,
  tipFloorSol: 0.00002,
  blockHeight: 1_000,
  epoch: 850,
  feeTiers: [{ marketCapLamportsThreshold: "0", protocolFeeBps: 95, creatorFeeBps: 30 }],
  mints: { [PUMP_MINT]: mintSpec({ curve }), [PLAIN_MINT]: mintSpec({ tokenProgram: "spl" }), [AUTHORITY_MINT]: mintSpec({ mintAuthority: true }) },
});

type Seen = { base64: string; signature: string; stateAtSend: { side: string } | undefined };

// Stand-in for an RPC's sendTransaction. Records each transaction and the trade's state on disk at
// the moment it arrived; onSend lets a test play the chain (debits, balances).
async function rpcStandIn(statePath: string, onSend: (signature: string) => void = () => {}) {
  const seen: Seen[] = [];
  const server = createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      const msg = JSON.parse(body) as { id: number; params: [string] };
      const base64 = msg.params[0];
      const signature = bs58.encode(VersionedTransaction.deserialize(Buffer.from(base64, "base64")).signatures[0]!);
      const pending = Object.values(JSON.parse(readFileSync(statePath, "utf8")).pending) as Array<{ signature: string | null; side: string }>;
      if (!seen.some((s) => s.signature === signature)) seen.push({ base64, signature, stateAtSend: pending.find((t) => t.signature === signature) });
      onSend(signature);
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: signature }));
    });
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  return { url: `http://127.0.0.1:${(server.address() as { port: number }).port}`, seen, close: () => server.close() };
}

// Stand-in for signatureSubscribe. answer(signature) is the err to report, or "silent".
async function pubsubStandIn(answer: (signature: string) => unknown) {
  const wss = new WebSocketServer({ port: 0, host: "127.0.0.1" });
  await new Promise((r) => wss.on("listening", r));
  let sub = 1;
  wss.on("connection", (socket) =>
    socket.on("message", (data) => {
      const msg = JSON.parse(data.toString()) as { id: number; params: [string] };
      const id = sub++;
      socket.send(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: id }));
      setTimeout(() => {
        const err = answer(msg.params[0]);
        if (err !== "silent") socket.send(JSON.stringify({ jsonrpc: "2.0", method: "signatureNotification", params: { subscription: id, result: { context: { slot: 1 }, value: { err } } } }));
      }, 100);
    }),
  );
  return { url: `ws://127.0.0.1:${(wss.address() as { port: number }).port}`, close: () => wss.close() };
}

async function setup(o: { answer?: (signature: string) => unknown; fees?: object; network?: ScenarioNetwork; onSend?: (n: ScenarioNetwork, sig: string) => void } = {}) {
  const statePath = join(mkdtempSync(join(tmpdir(), "trade-")), "live.json");
  const network = o.network ?? new ScenarioNetwork(chain());
  const wallet = Keypair.generate();
  const rpc = await rpcStandIn(statePath, (sig) => o.onSend?.(network, sig));
  const jito = await rpcStandIn(statePath);
  const pubsub = await pubsubStandIn(o.answer ?? (() => null));
  const config = parseConfig({
    rules: [{ name: "r", mode: "ca_scanner", accounts: ["1"], token: { amountSol: 0.01 }, fees: { antiMev: false, priorityFeeSol: 0.0001, tipSol: 0.0001, ...o.fees } }],
    execution: { confirmTimeoutMs: 1_000, rebroadcastMs: 10_000 },
  });
  const book = TradeBook.open(statePath, "live");
  const cache = new ChainCache(network);
  cache.start(60_000);
  await cache.ready();
  const watcher = new SignatureWatcher(pubsub.url);
  watcher.start();
  await new Promise((r) => setTimeout(r, 50));
  const senders: SenderTarget[] = [
    { name: "rpc", url: rpc.url, kind: "rpc" },
    { name: "jito", url: jito.url, kind: "jito" },
  ];
  const lander = new Lander(watcher, { rebroadcastMs: 10_000, confirmTimeoutMs: 1_000, currentBlockHeight: () => cache.blockHeight(), statusRpcUrl: null });
  const released: Fire[] = [];
  const trader = new Trader({ config, network, book, cache, report: new LatencyReport(), wallet, dryRun: false, lander, senders, releaseFire: (f) => released.push(f) });
  const fire = (mint: string): Fire => ({ kind: "fire", rule: config.rules[0]!, mint, signal: null, detail: "test", fireKey: `r|${mint}` });
  const trace = (mint: string): Trace => ({ mint, rule: "r", connection: "test", eventRef: "tweet/content twitter:1", publishedAtWall: null, receivedWall: Date.now(), receivedNs: process.hrtime.bigint(), decidedNs: process.hrtime.bigint() });
  const close = () => {
    book.close();
    cache.stop();
    watcher.stop();
    rpc.close();
    jito.close();
    pubsub.close();
  };
  return { statePath, network, wallet, rpc, jito, book, trader, released, fire, trace, close, config, cache };
}

const state = (path: string) => JSON.parse(readFileSync(path, "utf8"));
const onlyPending = (path: string) => Object.values(state(path).pending) as Array<{ signature: string; blockhash: string; reservedLamports: string; side: string }>;

test("a buy is pending on disk, with the signature the RPC receives, before the RPC sees it; confirmation opens the position", async () => {
  const t = await setup({ onSend: (n, sig) => (n.debits.set(sig, 12_345_678n), n.balances.set(PUMP_MINT, 300_000_000_000n)) });
  try {
    await t.trader.buy(t.fire(PUMP_MINT), t.trace(PUMP_MINT));
    assert.equal(t.rpc.seen.length, 1);
    assert.equal(t.rpc.seen[0]!.stateAtSend?.side, "buy");
    assert.deepEqual(state(t.statePath).pending, {});
    assert.equal(state(t.statePath).spend.totalLamports, "12345678", "spend settles to the actual debit");
    assert.equal(t.book.position(PUMP_MINT)?.tokenAmount, null, "balance not read yet");
    await t.trader.reconcile();
    assert.equal(t.book.position(PUMP_MINT)?.tokenAmount, "300000000000");
    assert.equal(t.released.length, 0);
  } finally {
    t.close();
  }
});

test("a pump.fun buy reserves its largest possible debit", async () => {
  // Hand-derived: 0.01 SOL + 5,000 signature + 100,000 priority (0.0001 SOL) + 2,074,080 token
  // account + 1,844,400 volume accumulator, no tip, a full-length curve.
  assert.equal(pumpBuyReservation(10_000_000n, 100_000n, 0n, 115), 14_023_480n);
  // A 49-byte curve adds (115 - 49) x 6,960 = 459,360 of rent.
  assert.equal(pumpBuyReservation(10_000_000n, 100_000n, 0n, 49), 14_482_840n);
  const t = await setup({ answer: () => "silent" });
  try {
    await t.trader.buy(t.fire(PUMP_MINT), t.trace(PUMP_MINT));
    assert.equal(onlyPending(t.statePath)[0]!.reservedLamports, "14023480");
  } finally {
    t.close();
  }
});

test("no confirmation leaves the buy pending with the spend held; the reconciler opens the position later", async () => {
  const t = await setup({ answer: () => "silent" });
  try {
    await t.trader.buy(t.fire(PUMP_MINT), t.trace(PUMP_MINT));
    const [p] = onlyPending(t.statePath);
    assert.equal(state(t.statePath).spend.totalLamports, p!.reservedLamports);
    assert.equal(t.book.position(PUMP_MINT), undefined);
    assert.equal(t.released.length, 0, "a sent fire is never given back");
    t.network.statuses.set(p!.signature, { confirmed: true, err: null });
    t.network.balances.set(PUMP_MINT, 5n);
    await t.trader.reconcile();
    assert.deepEqual(state(t.statePath).pending, {});
    assert.equal(t.book.position(PUMP_MINT)?.tokenAmount, "5");
  } finally {
    t.close();
  }
});

test("a pending buy expires only when its signature is unknown and its blockhash is proven invalid", async () => {
  const t = await setup({ answer: () => "silent" });
  try {
    await t.trader.buy(t.fire(PUMP_MINT), t.trace(PUMP_MINT));
    const [p] = onlyPending(t.statePath);
    t.network.blockHeight += 1_000; // far past lastValidBlockHeight: not proof on its own
    await t.trader.reconcile();
    assert.equal(onlyPending(t.statePath).length, 1, "a valid blockhash proves nothing");
    t.network.expiredBlockhashes.add(p!.blockhash);
    await t.trader.reconcile();
    assert.deepEqual(state(t.statePath).pending, {});
    assert.equal(state(t.statePath).spend.totalLamports, "0");
    assert.deepEqual(state(t.statePath).purchases, {});
  } finally {
    t.close();
  }
});

test("a pending trade without a signature waits for manual review", async () => {
  const t = await setup();
  try {
    const entry = { side: "buy" as const, mint: PLAIN_MINT, route: "jupiter" as const, signature: null, blockhash: "h", lastValidBlockHeight: null, reservedLamports: "1", amountLamports: "1", rule: "r", trigger: { eventRef: "e" }, tokenProgram: "t", sentAt: 0 };
    t.book.recordPending("j1", entry, t.config.spend, Date.now());
    t.network.expiredBlockhashes.add("h");
    await t.trader.reconcile();
    assert.equal(onlyPending(t.statePath).length, 1);
  } finally {
    t.close();
  }
});

test("a failed state write sends nothing and gives the fire back once", async () => {
  const t = await setup();
  const dir = join(t.statePath, "..");
  chmodSync(dir, 0o500);
  try {
    await t.trader.buy(t.fire(PUMP_MINT), t.trace(PUMP_MINT));
    assert.equal(t.rpc.seen.length, 0);
    assert.equal(t.released.length, 1);
  } finally {
    chmodSync(dir, 0o700);
    t.close();
  }
});

test("antiMev sends only through Jito with its tip; otherwise only to the RPC", async () => {
  const protectedRun = await setup({ fees: { antiMev: true } });
  try {
    await protectedRun.trader.buy(protectedRun.fire(PUMP_MINT), protectedRun.trace(PUMP_MINT));
    assert.equal(protectedRun.jito.seen.length, 1);
    assert.equal(protectedRun.rpc.seen.length, 0);
    const tx = VersionedTransaction.deserialize(Buffer.from(protectedRun.jito.seen[0]!.base64, "base64"));
    const tipAccounts = new Set(JSON.parse(readFileSync(new URL("../../shared/trading.json", import.meta.url), "utf8")).jitoTipAccounts);
    assert.ok(tx.message.staticAccountKeys.some((k) => tipAccounts.has(k.toBase58())), "the transaction pays a Jito tip account");
  } finally {
    protectedRun.close();
  }
  const open = await setup({ fees: { antiMev: false } });
  try {
    await open.trader.buy(open.fire(PUMP_MINT), open.trace(PUMP_MINT));
    assert.equal(open.rpc.seen.length, 1);
    assert.equal(open.jito.seen.length, 0);
  } finally {
    open.close();
  }
});

test("a token the safety checks reject gives the fire back and spends nothing", async () => {
  const t = await setup();
  try {
    await t.trader.buy(t.fire(AUTHORITY_MINT), t.trace(AUTHORITY_MINT));
    assert.equal(t.released.length, 1);
    assert.equal(state(t.statePath).spend.totalLamports, "0");
  } finally {
    t.close();
  }
});

// Jupiter answers from the test: an order for the wallet, and a chosen /execute result.
class JupiterScenario extends ScenarioNetwork {
  execute: () => Promise<JupiterExecute> = async () => ({ status: "Success", code: 0, signature: "landed-sig" });
  override async jupiterOrder(q: { taker: string }): Promise<JupiterOrder> {
    const payer = new PublicKey(q.taker);
    const message = new TransactionMessage({
      payerKey: payer,
      recentBlockhash: bs58.encode(Buffer.alloc(32, 3)),
      instructions: [SystemProgram.transfer({ fromPubkey: payer, toPubkey: Keypair.generate().publicKey, lamports: 1 })],
    }).compileToV0Message();
    return { transaction: Buffer.from(new VersionedTransaction(message).serialize()).toString("base64"), requestId: "req", lastValidBlockHeight: 1_150 };
  }
  override async jupiterExecute(): Promise<JupiterExecute> {
    return this.execute();
  }
}

test("Jupiter /execute that times out or answers -1001 leaves the buy pending; -1002 refunds it", async () => {
  for (const [result, stillPending] of [
    [() => Promise.reject(Object.assign(new Error("timed out"), { name: "TimeoutError" })), true],
    [async () => ({ status: "Failed", code: -1001 }), true],
    [async () => ({ status: "Failed", code: -1002 }), false],
  ] as Array<[() => Promise<JupiterExecute>, boolean]>) {
    const network = new JupiterScenario(chain());
    network.execute = result;
    const t = await setup({ network });
    try {
      await t.trader.buy(t.fire(PLAIN_MINT), t.trace(PLAIN_MINT));
      const pending = onlyPending(t.statePath);
      assert.equal(pending.length, stillPending ? 1 : 0);
      assert.equal(state(t.statePath).spend.totalLamports, stillPending ? pending[0]!.reservedLamports : "0");
    } finally {
      t.close();
    }
  }
});

test("a Jupiter buy whose simulated debit is above its reservation is not sent", async () => {
  // Reservation: 0.01 SOL + 5,000 signature + 2,074,080 token account + 0.005 SOL fee ceiling
  // (execution.maxAutoPriorityFeeSol) = 17,079,080. The order's message declares one signature
  // and no compute budget, so 5,000 lamports of fees go on top of the simulated balance drop.
  for (const [drop, sent] of [
    [17_074_080n, true],
    [17_074_081n, false],
  ] as Array<[bigint, boolean]>) {
    const network = new JupiterScenario(chain());
    network.simulatedDebit = drop;
    const t = await setup({ network });
    try {
      await t.trader.buy(t.fire(PLAIN_MINT), t.trace(PLAIN_MINT));
      assert.equal(t.book.position(PLAIN_MINT) !== undefined, sent);
      assert.equal(t.released.length, sent ? 0 : 1);
    } finally {
      t.close();
    }
  }
});

test("an exit survives a failed sell, and the reconciler sells again", async () => {
  let answers = 0;
  const t = await setup({
    answer: () => (++answers === 2 ? { InstructionError: [2, { Custom: 6003 }] } : null), // the first sell fails
    onSend: (n) => n.balances.set(PUMP_MINT, 300_000_000_000n),
  });
  try {
    await t.trader.buy(t.fire(PUMP_MINT), t.trace(PUMP_MINT));
    await t.trader.exit(PUMP_MINT, "post_deleted");
    assert.deepEqual(state(t.statePath).positions[PUMP_MINT].pendingExits.map((e: { code: string }) => e.code), ["post_deleted"]);
    await t.trader.reconcile();
    assert.equal(t.book.position(PUMP_MINT), undefined);
    assert.equal(answers, 3, "the buy, the failed sell, the sell");
  } finally {
    t.close();
  }
});

test("the exit watcher sells a position that reached take profit, once", async () => {
  const t = await setup({ onSend: (n) => n.balances.set(PUMP_MINT, 300_000_000_000n) });
  const watcher = new ExitWatcher({ ...t.config, exits: { ...t.config.exits, pollMs: 100 } }, t.book, t.network, t.cache, t.trader);
  try {
    // 300 billion tokens are worth about 9.4 million lamports on this curve after fees; the buy cost 10 million.
    // A 1 million cost makes it a take profit: record the position the way a dry run would.
    const held = { mint: PUMP_MINT, route: "pump" as const, reservedLamports: "1000000", amountLamports: "1000000", rule: "r", trigger: { eventRef: "e" }, tokenProgram: TOKEN_2022 };
    assert.equal(t.book.recordDryRunBuy("held", held, 300_000_000_000n, t.config.spend, Date.now()), null);
    watcher.start();
    await new Promise((r) => setTimeout(r, 900));
    assert.equal(t.rpc.seen.length, 1, "one sell, though the watcher polled several times");
    assert.equal(t.book.position(PUMP_MINT), undefined);
  } finally {
    watcher.stop();
    t.close();
  }
});

test("a reconcile that starts while another runs returns at once", async () => {
  const t = await setup({ answer: () => "silent" });
  try {
    await t.trader.buy(t.fire(PUMP_MINT), t.trace(PUMP_MINT));
    let lookups = 0;
    t.network.signatureStatus = async () => {
      lookups++;
      await new Promise((r) => setTimeout(r, 50));
      return null;
    };
    await Promise.all([t.trader.reconcile(), t.trader.reconcile()]);
    assert.equal(lookups, 1);
  } finally {
    t.close();
  }
});

test("a cached epoch older than 60 s and a blockhash older than 10 s are not used; stale fee tiers charge the highest fee", async (ctx) => {
  ctx.mock.timers.enable({ apis: ["Date"], now: 1_000_000 });
  const spec = chain();
  spec.feeTiers = [
    { marketCapLamportsThreshold: "0", protocolFeeBps: 95, creatorFeeBps: 30 },
    { marketCapLamportsThreshold: "100000000000", protocolFeeBps: 20, creatorFeeBps: 5 },
  ];
  const cache = new ChainCache(new ScenarioNetwork(spec));
  cache.start(3_600_000); // loads once; the next refresh is far beyond this test
  try {
    await cache.ready();
    assert.equal(cache.epoch(), 850);
    assert.equal(cache.feeTiers()!.length, 2);
    ctx.mock.timers.tick(10_001);
    assert.equal(cache.freshBlockhash(), null);
    assert.equal(cache.epoch(), 850);
    ctx.mock.timers.tick(50_000);
    assert.equal(cache.epoch(), null, "the gate then charges the higher transfer fee");
    ctx.mock.timers.tick(240_000);
    assert.deepEqual(cache.feeTiers()!.map((t) => [t.threshold, t.protocolBps + t.creatorBps]), [[0n, 125n]]);
  } finally {
    cache.stop();
  }
});
