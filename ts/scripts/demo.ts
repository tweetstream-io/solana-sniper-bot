// npm run demo: the full bot in dry run against scripts/demo-feed.ts, with chain and market reads
// answered from the scenario file. Needs no API key and makes no network request.

import { Keypair } from "@solana/web3.js";
import { fileURLToPath } from "node:url";
import { loadConfig } from "../src/setup/config.ts";
import { trialLink } from "../src/setup/env.ts";
import { TradeBook } from "../src/execute/trades.ts";
import { startBot } from "../src/pipeline.ts";
import { ScenarioNetwork, type ScenarioChain } from "./scenario-network.ts";
import { loadScenario, startDemoFeed } from "./demo-feed.ts";
import { log } from "../src/log.ts";

const scenario = loadScenario();
const config = loadConfig(fileURLToPath(new URL("../../shared/demo/config.json", import.meta.url)));
const feed = await startDemoFeed({ port: 0, scenario, clients: 2, speed: 1 });
log("demo", `dry run against the local demo feed on port ${feed.port}; nothing leaves this machine`);

const bot = await startBot({
  config,
  network: new ScenarioNetwork(scenario.chain as ScenarioChain),
  book: TradeBook.open(null, "dry_run"),
  feeds: [
    { name: "ultra", apiKey: scenario.keys.ultra },
    { name: "standard", apiKey: scenario.keys.standard },
  ],
  wsUrl: `ws://127.0.0.1:${feed.port}/ws`,
  dryRun: true,
  wallet: Keypair.generate(),
  senders: [],
  rpcWsUrl: "",
  onFatal: (reason) => {
    console.error(reason);
    process.exit(1);
  },
});

await feed.done;
bot.stop();
feed.close();
for (const line of bot.report.summary()) log("summary", line);
log("next", `Run it on live X data with a TweetStream key: ${trialLink("cli-demo")}`);
process.exit(0);
