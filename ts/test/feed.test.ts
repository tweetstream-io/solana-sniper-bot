// Feed client against the local demo feed (real WebSocket, real subprotocol auth), backoff bounds,
// and publish times from X ids.

import { test } from "node:test";
import assert from "node:assert/strict";
import { backoffMs, FeedClient } from "../src/feed/client.ts";
import { snowflakeMs } from "../src/protocol.ts";
import { loadScenario, startDemoFeed, type Scenario } from "../scripts/demo-feed.ts";
import { Keypair } from "@solana/web3.js";
import { parseConfig } from "../src/setup/config.ts";
import { TradeBook } from "../src/execute/trades.ts";
import { startBot } from "../src/pipeline.ts";
import { ScenarioNetwork, type ScenarioChain } from "../scripts/scenario-network.ts";

const scenario: Scenario = { description: "", keys: { standard: "good-key", ultra: "ultra-key" }, ultraLeadMs: 0, chain: {}, steps: [] };

test("a rejected key is fatal and names the dashboard", async () => {
  const feed = await startDemoFeed({ port: 0, scenario, clients: 1, speed: 1 });
  const reason = await new Promise<string>((resolve) => {
    const client = new FeedClient({ name: "standard", apiKey: "wrong-key", url: `ws://127.0.0.1:${feed.port}/ws`, heartbeatMs: 30_000, onEvent: () => {}, onFatal: (r) => (client.stop(), resolve(r)) });
    client.start();
  });
  feed.close();
  assert.match(reason, /rejected \(401\)/);
  assert.match(reason, /tweetstream\.io\/dashboard/);
});

test("a valid key authenticates and receives auth_pong", async () => {
  const feed = await startDemoFeed({ port: 0, scenario, clients: 1, speed: 1 });
  const pair = await new Promise<string>((resolve, reject) => {
    const client = new FeedClient({
      name: "standard",
      apiKey: "good-key",
      url: `ws://127.0.0.1:${feed.port}/ws`,
      heartbeatMs: 30_000,
      onEvent: (e) => (client.stop(), resolve(`${e.t}/${e.op}`)),
      onFatal: reject,
    });
    client.start();
  });
  feed.close();
  assert.equal(pair, "control/auth_pong");
});

test("backoff doubles from 0.5 s, caps at 30 s, and jitters within the upper half", () => {
  assert.equal(backoffMs(0, 0), 250);
  assert.equal(backoffMs(0, 0.999), 499);
  assert.equal(backoffMs(3, 0), 2_000);
  assert.equal(backoffMs(10, 0), 15_000);
  assert.equal(backoffMs(10, 0.999), 29_985);
});

test("snowflakeMs uses BigInt, so the largest id keeps its exact time", () => {
  assert.equal(snowflakeMs("2026541002650550272"), 1_772_000_001_000);
  // Number arithmetic rounds this id up and returns ...209.
  assert.equal(snowflakeMs("9223372036854775807"), 3_487_858_230_208);
  assert.equal(snowflakeMs("not-a-number"), null);
});

test("a refused Ultra key leaves the standard connection running; a refused standard key stops the bot", async () => {
  const demo = loadScenario();
  // One post, sent on the standard connection once it has authenticated.
  const post = { v: 1, t: "tweet", op: "content", ts: "{{now}}", d: { tweetId: "{{tweet:p}}", text: "gm", createdAt: "{{now}}", kind: "post", author: { id: "111", handle: "@dev_one" } } };
  const scenario: Scenario = { ...demo, steps: [{ afterMs: 50, to: "standard", note: "a post", envelope: post }] };
  const run = async (feeds: Array<{ name: string; apiKey: string }>) => {
    const feed = await startDemoFeed({ port: 0, scenario, clients: 1, speed: 1 });
    const fatal: string[] = [];
    const bot = await startBot({
      config: parseConfig({ rules: [{ name: "scan", mode: "ca_scanner", accounts: ["111"] }] }),
      network: new ScenarioNetwork(demo.chain as ScenarioChain),
      book: TradeBook.open(null, "dry_run"),
      feeds,
      wsUrl: `ws://127.0.0.1:${feed.port}/ws`,
      dryRun: true,
      wallet: Keypair.generate(),
      senders: [],
      rpcWsUrl: "",
      onFatal: (reason) => fatal.push(reason),
    });
    await Promise.race([feed.done, new Promise((r) => setTimeout(r, 3_000))]);
    bot.stop();
    feed.close();
    return { fatal, summary: bot.report.summary().join("\n") };
  };
  const ultraRefused = await run([
    { name: "ultra", apiKey: "not-a-key" },
    { name: "standard", apiKey: demo.keys.standard },
  ]);
  assert.deepEqual(ultraRefused.fatal, []);
  assert.match(ultraRefused.summary, /first copy of an event came from standard/);
  const standardRefused = await run([
    { name: "ultra", apiKey: demo.keys.ultra },
    { name: "standard", apiKey: "not-a-key" },
  ]);
  assert.equal(standardRefused.fatal.length, 1);
  assert.match(standardRefused.fatal[0]!, /standard API key rejected/);
});
