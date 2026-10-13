// A local stand-in for the TweetStream WebSocket: same subprotocol auth and v1 envelopes, replaying
// shared/demo/scenario.json. Either bot (TypeScript or Rust) can connect to it.
//
//   node scripts/demo-feed.ts [--port 8787] [--scenario ../shared/demo/scenario.json] [--clients 2] [--speed 1]
//
// Accepts only the API keys listed in the scenario. Starts the script once --clients connections
// have sent auth_ping. Steps with "to": "both" go to the Ultra connection first and to the
// standard one ultraLeadMs later, so the bot sees a duplicate copy it must drop.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { WebSocketServer, type WebSocket } from "ws";

export type Scenario = {
  description: string;
  keys: { standard: string; ultra: string };
  ultraLeadMs: number;
  chain: unknown;
  steps: Array<{ afterMs: number; to: "both" | "standard" | "ultra"; note: string; envelope: unknown }>;
};

export const DEFAULT_SCENARIO = fileURLToPath(new URL("../../shared/demo/scenario.json", import.meta.url));
const TWITTER_EPOCH_MS = 1288834974657n;
const TOKEN_PREFIX = "tweetstream.auth.token.";

export function loadScenario(path = DEFAULT_SCENARIO): Scenario {
  return JSON.parse(readFileSync(path, "utf8")) as Scenario;
}

// "{{now}}" becomes the current time in ms; "{{tweet:name}}" becomes a post id whose embedded
// publish time is the moment the name is first used, so publish -> receipt latency is real.
function render(value: unknown, ids: Map<string, string>): unknown {
  if (value === "{{now}}") return Date.now();
  if (typeof value === "string") {
    return value.replace(/\{\{tweet:([a-z0-9_-]+)\}\}/gi, (_, name: string) => {
      if (!ids.has(name)) ids.set(name, ((BigInt(Date.now()) - TWITTER_EPOCH_MS) << 22n).toString());
      return ids.get(name)!;
    });
  }
  if (Array.isArray(value)) return value.map((v) => render(v, ids));
  if (value && typeof value === "object") return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, render(v, ids)]));
  return value;
}

const offeredKey = (header: string | string[] | undefined) =>
  String(header ?? "")
    .split(",")
    .map((p) => p.trim())
    .find((p) => p.startsWith(TOKEN_PREFIX))
    ?.slice(TOKEN_PREFIX.length);

type DemoFeed = { port: number; done: Promise<void>; close(): void };

export function startDemoFeed(opts: { port: number; scenario: Scenario; clients: number; speed: number }): Promise<DemoFeed> {
  const { scenario } = opts;
  const keyName = new Map([
    [scenario.keys.standard, "standard"],
    [scenario.keys.ultra, "ultra"],
  ]);
  const sockets = new Map<string, WebSocket>();
  let finish: () => void = () => {};
  const done = new Promise<void>((resolve) => (finish = resolve));
  let started = false;

  const server = new WebSocketServer({
    port: opts.port,
    host: "127.0.0.1",
    perMessageDeflate: false,
    handleProtocols: (protocols) => (protocols.has("tweetstream.v1") ? "tweetstream.v1" : false),
    verifyClient: (info, callback) => {
      const header = info.req.headers["sec-websocket-protocol"];
      if (!String(header ?? "").includes("tweetstream.v1")) return callback(false, 400, "missing tweetstream.v1");
      const key = offeredKey(header);
      if (!key || !keyName.has(key)) return callback(false, 401, "unknown API key");
      callback(true);
    },
  });

  const play = async () => {
    const ids = new Map<string, string>();
    for (const [i, step] of scenario.steps.entries()) {
      await new Promise((r) => setTimeout(r, step.afterMs / opts.speed));
      console.log(`feed      step ${i + 1}/${scenario.steps.length}: ${step.note}`);
      const frame = JSON.stringify(render(step.envelope, ids));
      const ultra = sockets.get("ultra");
      const standard = sockets.get("standard");
      if (step.to !== "standard" && ultra) ultra.send(frame);
      if (step.to === "both" && ultra && standard) await new Promise((r) => setTimeout(r, scenario.ultraLeadMs));
      if (step.to !== "ultra" && standard) standard.send(frame);
    }
    setTimeout(finish, 1_500);
  };

  server.on("connection", (socket, request) => {
    sockets.set(keyName.get(offeredKey(request.headers["sec-websocket-protocol"])!)!, socket);
    socket.on("message", (data) => {
      if (!data.toString().includes('"auth_ping"')) return;
      socket.send(JSON.stringify({ v: 1, t: "control", op: "auth_pong", ts: Date.now(), d: {} }));
      if (!started && sockets.size >= opts.clients) {
        started = true;
        void play();
      }
    });
  });

  return new Promise((resolve) => {
    server.on("listening", () => {
      const address = server.address();
      const port = typeof address === "object" && address ? address.port : opts.port;
      resolve({ port, done, close: () => server.close() });
    });
  });
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const arg = (name: string, fallback: string) => {
    const i = process.argv.indexOf(`--${name}`);
    return i > 0 && process.argv[i + 1] ? process.argv[i + 1]! : fallback;
  };
  const feed = await startDemoFeed({
    port: Number(arg("port", "8787")),
    scenario: loadScenario(arg("scenario", DEFAULT_SCENARIO)),
    clients: Number(arg("clients", "2")),
    speed: Number(arg("speed", "1")),
  });
  console.log(`feed      listening on ws://127.0.0.1:${feed.port}/ws`);
  await feed.done;
  feed.close();
  process.exit(0);
}
