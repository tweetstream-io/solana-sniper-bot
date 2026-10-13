// Landing against local stand-ins for an RPC (HTTP sendTransaction) and its PubSub WebSocket
// (signatureSubscribe), so fan-out and confirmation run over real sockets without a chain.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer, type Server } from "node:http";
import { WebSocketServer } from "ws";
import { Keypair, SystemProgram, TransactionMessage, VersionedTransaction } from "@solana/web3.js";
import bs58 from "bs58";
import { Lander, SignatureWatcher } from "../src/land/land.ts";

function signedTx(): VersionedTransaction {
  const payer = Keypair.generate();
  const message = new TransactionMessage({
    payerKey: payer.publicKey,
    recentBlockhash: bs58.encode(Buffer.alloc(32, 9)),
    instructions: [SystemProgram.transfer({ fromPubkey: payer.publicKey, toPubkey: Keypair.generate().publicKey, lamports: 1 })],
  }).compileToV0Message();
  const tx = new VersionedTransaction(message);
  tx.sign([payer]);
  return tx;
}

async function rpcServer(status: number, received: string[]): Promise<{ url: string; server: Server }> {
  const server = createServer((req, res) => {
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => {
      const msg = JSON.parse(body) as { id: number; method: string; params: [string] };
      received.push(msg.method);
      res.writeHead(status, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: "sig" }));
    });
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  const address = server.address() as { port: number };
  return { url: `http://127.0.0.1:${address.port}`, server };
}

// Answers signatureSubscribe, then reports the signature with the given error after delayMs.
async function pubsub(err: unknown, delayMs: number): Promise<{ url: string; close(): void }> {
  const wss = new WebSocketServer({ port: 0, host: "127.0.0.1" });
  await new Promise((r) => wss.on("listening", r));
  wss.on("connection", (socket) =>
    socket.on("message", (data) => {
      const msg = JSON.parse(data.toString()) as { id: number; method: string };
      if (msg.method !== "signatureSubscribe") return;
      socket.send(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: 42 }));
      setTimeout(() => socket.send(JSON.stringify({ jsonrpc: "2.0", method: "signatureNotification", params: { subscription: 42, result: { context: { slot: 1 }, value: { err } } } })), delayMs);
    }),
  );
  return { url: `ws://127.0.0.1:${(wss.address() as { port: number }).port}`, close: () => wss.close() };
}

async function landOnce(err: unknown) {
  const goodCalls: string[] = [];
  const badCalls: string[] = [];
  const good = await rpcServer(200, goodCalls);
  const bad = await rpcServer(500, badCalls);
  const ps = await pubsub(err, 50);
  const watcher = new SignatureWatcher(ps.url);
  watcher.start();
  await new Promise((r) => setTimeout(r, 50));
  const lander = new Lander(
    [
      { name: "down", url: bad.url, kind: "rpc" },
      { name: "up", url: good.url, kind: "rpc" },
    ],
    watcher,
    { rebroadcastMs: 10_000, confirmTimeoutMs: 2_000, currentBlockHeight: () => 0 },
  );
  const tx = signedTx();
  const result = await lander.land(tx, 1_000);
  watcher.stop();
  ps.close();
  good.server.close();
  bad.server.close();
  return { result, tx, goodCalls, badCalls };
}

test("the same transaction goes to every sender; the first to accept is recorded; confirmation is pushed", async () => {
  const { result, tx, goodCalls, badCalls } = await landOnce(null);
  assert.equal(result.status, "confirmed");
  assert.equal(result.firstAckBy, "up");
  assert.equal(result.signature, bs58.encode(tx.signatures[0]!));
  assert.deepEqual(goodCalls, ["sendTransaction"]);
  assert.deepEqual(badCalls, ["sendTransaction"]);
  assert.ok(result.confirmedNs! > result.sentNs);
});

test("a transaction that lands with an error is reported as failed", async () => {
  const { result } = await landOnce({ InstructionError: [2, { Custom: 6002 }] });
  assert.equal(result.status, "failed");
  assert.match(result.error!, /6002/);
});
