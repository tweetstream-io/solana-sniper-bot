// npm run latency:probe: measures round trips from this machine to every endpoint the bot uses.
// Read-only: RPCs get getHealth/getSlot, Jito gets getTipAccounts, and TweetStream gets auth_ping
// only when TWEETSTREAM_API_KEY is set. It never builds, signs, or sends a transaction.

import WebSocket from "ws";
import { productLink, readEnv, trialLink } from "../src/setup/env.ts";
import { loadConfig } from "../src/setup/config.ts";
import { percentile } from "../src/report/latency.ts";

const env = readEnv();
const rows: string[] = [];
const fmt = (ms: number) => `${ms.toFixed(1)} ms`;

async function timeHttp(name: string, url: string, body: object, samples: number, gapMs = 0): Promise<void> {
  const times: number[] = [];
  let status = "";
  for (let i = 0; i < samples; i++) {
    if (i && gapMs) await new Promise((r) => setTimeout(r, gapMs));
    const started = performance.now();
    try {
      const response = await fetch(url, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body), signal: AbortSignal.timeout(5_000) });
      await response.arrayBuffer();
      status = `HTTP ${response.status}`;
      times.push(performance.now() - started);
    } catch (error) {
      status = (error as Error).message;
    }
  }
  if (!times.length) return void rows.push(`${name.padEnd(28)} failed: ${status}`);
  const [first, ...warm] = times;
  const sorted = [...warm].sort((a, b) => a - b);
  rows.push(`${name.padEnd(28)} first ${fmt(first!)} (includes TLS), warm p50 ${warm.length ? fmt(percentile(sorted, 0.5)!) : "n/a"}, ${status}`);
}

function timeWs(name: string, url: string, protocols: string[], ping: string | null): Promise<void> {
  return new Promise((resolve) => {
    const started = performance.now();
    const socket = new WebSocket(url, protocols, { perMessageDeflate: false, handshakeTimeout: 5_000 });
    const pongs: number[] = [];
    let sentAt = 0;
    const send = () => {
      sentAt = performance.now();
      socket.send(ping!);
    };
    socket.on("open", () => {
      const connectMs = performance.now() - started;
      if (!ping) {
        rows.push(`${name.padEnd(28)} connect ${fmt(connectMs)}`);
        socket.close();
        return resolve();
      }
      socket.on("message", (data) => {
        if (!String(data).includes("auth_pong")) return;
        pongs.push(performance.now() - sentAt);
        if (pongs.length < 5) return send();
        rows.push(`${name.padEnd(28)} connect ${fmt(connectMs)}, auth_ping round trip p50 ${fmt(percentile([...pongs].sort((a, b) => a - b), 0.5)!)}`);
        socket.close();
        resolve();
      });
      send();
    });
    socket.on("unexpected-response", (_req, res) => {
      rows.push(`${name.padEnd(28)} refused: HTTP ${res.statusCode}`);
      resolve();
    });
    socket.on("error", (error) => {
      rows.push(`${name.padEnd(28)} failed: ${error.message}`);
      resolve();
    });
  });
}

for (const url of env.rpcUrls) {
  await timeHttp(`rpc ${new URL(url).host}`, url, { jsonrpc: "2.0", id: 1, method: "getHealth" }, 6);
}
await timeWs(`rpc websocket ${new URL(env.rpcWsUrl).host}`, env.rpcWsUrl, [], null);
// Jito allows about one request per second per IP and region by default, so the samples are spaced.
if (env.jitoUrl) await timeHttp("jito block engine", `${env.jitoUrl}/api/v1/getTipAccounts`, { jsonrpc: "2.0", id: 1, method: "getTipAccounts", params: [] }, 3, 1_100);
try {
  const config = loadConfig(env.configPath);
  for (const s of config.senders) {
    const url = process.env[s.urlEnv];
    if (url) await timeHttp(`sender ${s.name}`, url, { jsonrpc: "2.0", id: 1, method: "getHealth" }, 4);
  }
} catch {
  rows.push("senders                      skipped: no readable config.json");
}
const key = env.ultraApiKey ?? env.apiKey;
if (key) await timeWs(`tweetstream ${new URL(env.wsUrl).host}`, env.wsUrl, ["tweetstream.v1", `tweetstream.auth.token.${key}`], '{"op":"auth_ping"}');
else rows.push(`tweetstream                  skipped: set TWEETSTREAM_API_KEY to measure the feed (${trialLink("cli-latency-probe")})`);

console.log(rows.join("\n"));
console.log(`\nRound trips from this machine only. They do not include TweetStream's time from post to delivery; see ${productLink("/speedcheck", "cli-latency-probe")}`);
process.exit(0);
