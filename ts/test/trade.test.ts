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
import { Trader } from "../src/execute/trade.ts";
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

type Seen = { base64: string; signature: string; stateAtSend: { state?: string; tx?: string } | undefined };

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
      const trades = Object.values(JSON.parse(readFileSync(statePath, "utf8")).trades) as Array<{ signature?: string; state: string; tx?: string }>;
      if (!seen.some((s) => s.signature === signature)) seen.push({ base64, signature, stateAtSend: trades.find((t) => t.signature === signature) });
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
  while (!cache.feeTiers) await new Promise((r) => setTimeout(r, 10));
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
  const fire = (mint: string): Fire => ({ kind: "fire", rule: config.rules[0]!, mint, signal: null, detail: "test", fireKey: `r|${mint}`, actor: { namespace: "x", id: "1" } });
  const trace = (mint: string): Trace => ({ mint, rule: "r", connection: "test", eventRef: "tweet/content twitter:1", publishedAtWall: null, receivedWall: Date.now(), receivedNs: process.hrtime.bigint(), decidedNs: process.hrtime.bigint() });
  const close = () => {
    cache.stop();
    watcher.stop();
    rpc.close();
    jito.close();
    pubsub.close();
  };
  return { statePath, network, wallet, rpc, jito, book, trader, released, fire, trace, close, config, cache };
}

const state = (path: string) => JSON.parse(readFileSync(path, "utf8"));

test("a buy is on disk as 'sending', with the exact bytes the RPC receives, before the RPC sees it", async () => {
  const t = await setup({ onSend: (n, sig) => (n.debits.set(sig, 12_345_678n), n.balances.set(PUMP_MINT, 300_000_000_000n)) });
  try {
    await t.trader.buy(t.fire(PUMP_MINT), t.trace(PUMP_MINT));
    assert.equal(t.rpc.seen.length, 1);
    assert.equal(t.rpc.seen[0]!.stateAtSend?.state, "sending");
    assert.equal(t.rpc.seen[0]!.stateAtSend?.tx, t.rpc.seen[0]!.base64);
    const trade = Object.values(state(t.statePath).trades)[0] as { state: string; spentLamports: string };
    assert.equal(trade.state, "confirmed");
    assert.equal(trade.spentLamports, "12345678", "spend settles to the actual debit");
    assert.equal(t.book.position(PUMP_MINT)?.tokenAmount, null, "balance not read yet");
    await t.trader.reconcile();
    assert.equal(t.book.position(PUMP_MINT)?.tokenAmount, "300000000000");
    assert.equal(t.released.length, 0);
  } finally {
    t.close();
  }
});

test("no confirmation leaves the buy unknown with the spend held; the reconciler opens the position later", async () => {
  const t = await setup({ answer: () => "silent" });
  try {
    await t.trader.buy(t.fire(PUMP_MINT), t.trace(PUMP_MINT));
    const [trade] = Object.values(state(t.statePath).trades) as Array<{ state: string; signature: string; reservedLamports: string }>;
    assert.equal(trade!.state, "unknown");
    assert.equal(state(t.statePath).spend.totalLamports, trade!.reservedLamports);
    assert.equal(t.book.position(PUMP_MINT), undefined);
    assert.equal(t.released.length, 0, "a sent fire is never given back");
    t.network.statuses.set(trade!.signature, { confirmed: true, err: null });
    t.network.balances.set(PUMP_MINT, 5n);
    await t.trader.reconcile();
    assert.equal(t.book.position(PUMP_MINT)?.tokenAmount, "5");
    assert.equal(state(t.statePath).trades[Object.keys(state(t.statePath).trades)[0]!].state, "reconciled");
  } finally {
    t.close();
  }
});

test("an exit asked for while the buy is pending sells once the buy lands", async () => {
  const t = await setup({ onSend: (n) => n.balances.set(PUMP_MINT, 300_000_000_000n) });
  try {
    const buying = t.trader.buy(t.fire(PUMP_MINT), t.trace(PUMP_MINT));
    while (!t.rpc.seen.length) await new Promise((r) => setTimeout(r, 5));
    await t.trader.exit(PUMP_MINT, "post_deleted", "deleted");
    await buying;
    assert.equal(t.rpc.seen.length, 2, "the buy, then the sell");
    assert.equal(t.book.position(PUMP_MINT), undefined, "the sell landed and closed the position");
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

test("antiMev sends only through Jito with its tip; otherwise only to RPCs", async () => {
  const protectedRun = await setup({ fees: { antiMev: true } });
  try {
    await protectedRun.trader.buy(protectedRun.fire(PUMP_MINT), protectedRun.trace(PUMP_MINT));
    assert.equal(protectedRun.jito.seen.length, 1);
    assert.equal(protectedRun.rpc.seen.length, 0);
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

test("a rejected token gives the fire back once and spends nothing", async () => {
  const t = await setup();
  try {
    await t.trader.buy(t.fire(AUTHORITY_MINT), t.trace(AUTHORITY_MINT));
    assert.equal(t.released.length, 1);
    assert.equal(state(t.statePath).spend.totalLamports, "0");
    assert.equal(await t.trader.sell(PLAIN_MINT, "rule"), "no_position");
  } finally {
    t.close();
  }
});

// Jupiter answers from the test: an order for the wallet, and a chosen /execute result.
class JupiterScenario extends ScenarioNetwork {
  execute: () => Promise<JupiterExecute> = async () => ({ status: "Success", code: 0 });
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

test("Jupiter /execute that times out or answers -1001 leaves the buy unknown; -1002 is a definite failure", async () => {
  for (const [result, expected] of [
    [() => Promise.reject(Object.assign(new Error("timed out"), { name: "TimeoutError" })), "unknown"],
    [async () => ({ status: "Failed", code: -1001 }), "unknown"],
    [async () => ({ status: "Failed", code: -1002 }), "failed"],
  ] as Array<[() => Promise<JupiterExecute>, string]>) {
    const network = new JupiterScenario(chain());
    network.execute = result;
    const t = await setup({ network });
    try {
      await t.trader.buy(t.fire(PLAIN_MINT), t.trace(PLAIN_MINT));
      const [trade] = Object.values(state(t.statePath).trades) as Array<{ state: string; reservedLamports: string }>;
      assert.equal(trade!.state, expected);
      assert.equal(state(t.statePath).spend.totalLamports, expected === "unknown" ? trade!.reservedLamports : "0");
    } finally {
      t.close();
    }
  }
});

test("a curve that graduates mid-buy retries through Jupiter within the same fire", async () => {
  const network = new JupiterScenario(chain());
  const t = await setup({
    network,
    answer: () => ({ InstructionError: [2, { Custom: 6005 }] }),
    onSend: (n) => {
      // The curve completes while the buy is in flight.
      const done = { ...curve, complete: true };
      const spec = chain();
      spec.mints[PUMP_MINT] = mintSpec({ curve: done });
      for (const [k, v] of new ScenarioNetwork(spec).accounts) n.accounts.set(k, v);
    },
  });
  try {
    await t.trader.buy(t.fire(PUMP_MINT), t.trace(PUMP_MINT));
    const trades = Object.values(state(t.statePath).trades) as Array<{ state: string; route: string }>;
    assert.deepEqual(trades.map((x) => [x.route, x.state]), [["pump", "failed"], ["jupiter", "confirmed"]]);
    assert.equal(t.released.length, 0);
  } finally {
    t.close();
  }
});

test("the exit watcher sells a position that reached take profit, once", async () => {
  const t = await setup();
  const watcher = new ExitWatcher({ ...t.config, exits: { ...t.config.exits, pollMs: 100 } }, t.book, t.network, t.cache, t.trader);
  try {
    // 300 billion tokens are worth about 9.4 million lamports on this curve after fees; bought for 1 million.
    const trigger = { rule: "r", fireKey: "r|x", eventRef: "e", actor: { namespace: "x" as const, id: "1" } };
    t.book.openPosition({ mint: PUMP_MINT, route: "pump", tokenProgram: "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb", openedAt: Date.now(), buyTradeId: "b", costLamports: "1000000", tokenAmount: "300000000000", trigger });
    watcher.start();
    await new Promise((r) => setTimeout(r, 900));
    assert.equal(t.rpc.seen.length, 1, "one sell, though the watcher polled several times");
    assert.equal(t.book.position(PUMP_MINT), undefined);
  } finally {
    watcher.stop();
    t.close();
  }
});
