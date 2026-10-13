// npm run bench: times the hot path on this machine with no network. Feeds N posts, each with a
// new mint, through the real Decider and Trader (dry run) and prints per-stage timings. The mint
// read waits 1 ms to stand in for a nearby RPC; address derivation runs during that wait.

import { Keypair } from "@solana/web3.js";
import { parseConfig } from "../src/setup/config.ts";
import { Ledger } from "../src/rules/ledger.ts";
import { Decider } from "../src/pipeline.ts";
import { ChainCache, Trader } from "../src/execute/trade.ts";
import { LatencyReport } from "../src/report/latency.ts";
import { ScenarioNetwork, type Chain } from "./scenario-network.ts";

const N = Number(process.argv[2] ?? 2000);
const AUTHOR = { id: "1000000000000000001", handle: "@bench_dev" };
const mints = Array.from({ length: N }, () => Keypair.generate().publicKey.toBase58());
const curve = { virtualTokenReserves: "1000000000000000", virtualQuoteReserves: "32000000000", realTokenReserves: "720000000000000", realQuoteReserves: "2000000000", tokenTotalSupply: "1000000000000000", complete: false };
const chain: Chain = {
  solUsd: 150,
  priorityFeeMicroLamports: 40_000,
  tipFloorSol: 0.00002,
  blockHeight: 300_000_000,
  mints: Object.fromEntries(mints.map((m) => [m, { tokenProgram: "token-2022", mintAuthority: false, freezeAuthority: false, supply: "1000000000000000", decimals: 6, curve }])),
};
const config = parseConfig({
  rules: [{ name: "bench", mode: "ca_scanner", accounts: [AUTHOR.id], token: { amountSol: 0.001 }, filters: { maxSnipes: 0 } }],
  spend: { maxPerTradeSol: 1, maxPerHourSol: 1e6, maxTotalSol: 1e6 },
});
class NearbyRpc extends ScenarioNetwork {
  override async getAccounts(addresses: string[]) {
    await new Promise((r) => setTimeout(r, 1));
    return super.getAccounts(addresses);
  }
}
const network = new NearbyRpc(chain);
const ledger = Ledger.open(null);
const cache = new ChainCache(network);
cache.start(60_000);
await new Promise((r) => setTimeout(r, 50)); // let the caches fill once
const report = new LatencyReport();
const trader = new Trader({ config, network, ledger, cache, report, wallet: Keypair.generate(), dryRun: true, lander: null, jitoEnabled: true, jupiterHeaders: {} });
const decider = new Decider(config, ledger);

const quiet = console.log;
console.log = () => {};
const decideUs: number[] = [];
for (const [i, mint] of mints.entries()) {
  const tweetId = ((BigInt(Date.now()) - 1288834974657n) << 22n) + BigInt(i);
  const frame = JSON.stringify({ v: 1, t: "tweet", op: "content", ts: Date.now(), d: { tweetId: tweetId.toString(), text: `CA: ${mint}`, createdAt: Date.now(), kind: "post", author: AUTHOR } });
  const receivedNs = process.hrtime.bigint();
  const outcome = decider.handle(JSON.parse(frame), { wallMs: Date.now() });
  const decidedNs = process.hrtime.bigint();
  decideUs.push(Number(decidedNs - receivedNs) / 1000);
  for (const m of outcome.triggers) await trader.buy(m, { mint, rule: "bench", connection: "bench", publishedAtWall: null, receivedWall: Date.now(), receivedNs, decidedNs });
}
console.log = quiet;
cache.stop();

const pct = (list: number[], p: number) => [...list].sort((a, b) => a - b)[Math.ceil(p * list.length) - 1]!;
console.log(`${N} posts, each with a new mint, on ${process.platform} ${process.arch}, Node ${process.version}`);
console.log(`receipt->decision (JSON.parse + dedupe + detect + rules): p50 ${pct(decideUs, 0.5).toFixed(1)} us, p95 ${pct(decideUs, 0.95).toFixed(1)} us`);
for (const line of report.summary().filter((l) => /decision->signed|safety read|build\+sign/.test(l))) console.log(line);
process.exit(0);
