// Feed client against the local demo feed (real WebSocket, real subprotocol auth), backoff bounds,
// and publish times from X ids.

import { test } from "node:test";
import assert from "node:assert/strict";
import { backoffMs, FeedClient } from "../src/feed/client.ts";
import { snowflakeMs } from "../src/protocol.ts";
import { startDemoFeed, type Scenario } from "../scripts/demo-feed.ts";

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
