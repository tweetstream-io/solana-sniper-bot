// Entry point for `npm start`: reads .env and config.json, prints setup help when something is
// missing, then starts the pipeline in pipeline.ts. Dry run unless DRY_RUN=false.

import { Keypair } from "@solana/web3.js";
import bs58 from "bs58";
import { readEnv, SETUP_HELP, SetupError, ultraTrialLink, type Env } from "./setup/env.ts";
import { ConfigError, handleWarnings, loadConfig, type Config } from "./setup/config.ts";
import { StateError, TradeBook } from "./execute/trades.ts";
import { LiveNetwork } from "./network.ts";
import { startBot } from "./pipeline.ts";
import type { SenderTarget } from "./land/land.ts";
import { log } from "./log.ts";

function fail(message: string): never {
  console.error(message);
  process.exit(1);
}

function loadWallet(env: Env): Keypair | null {
  if (!env.walletSecret) {
    if (!env.dryRun) fail(`DRY_RUN=false needs WALLET_SECRET_KEY in .env: the base58 secret key of a wallet that holds only what you can lose.`);
    return null;
  }
  let bytes: Uint8Array;
  try {
    bytes = bs58.decode(env.walletSecret);
  } catch {
    fail("WALLET_SECRET_KEY is not base58. Paste the 64-byte secret key in base58, as wallets export it.");
  }
  if (bytes.length !== 64) fail(`WALLET_SECRET_KEY decodes to ${bytes.length} bytes; a Solana secret key is 64.`);
  return Keypair.fromSecretKey(bytes);
}

function senderTargets(env: Env, config: Config): SenderTarget[] {
  const targets: SenderTarget[] = env.rpcUrls.map((url) => ({ name: new URL(url).host, url, kind: "rpc" as const }));
  if (env.jitoUrl) targets.push({ name: "jito", url: `${env.jitoUrl}/api/v1/transactions`, kind: "jito" });
  for (const s of config.senders) {
    const url = process.env[s.urlEnv]?.trim();
    if (!url) fail(`senders "${s.name}": set ${s.urlEnv} in .env to the sender's URL, or remove the sender from config.json.`);
    targets.push({ name: s.name, url, kind: "rpc", needsTip: s.tipAccounts.length > 0 && s.tipSol > 0 });
  }
  return targets;
}

let env: Env;
let config: Config;
try {
  env = readEnv();
  if (!env.apiKey && !env.ultraApiKey) fail(SETUP_HELP);
  config = loadConfig(env.configPath);
} catch (error) {
  if (error instanceof SetupError || error instanceof ConfigError) fail(error.message);
  throw error;
}
for (const warning of handleWarnings(config)) log("warning", warning);
const wallet = loadWallet(env);
if (env.dryRun) {
  log("mode", "DRY RUN: nothing is sent. Set DRY_RUN=false in .env to trade.");
} else {
  console.log(`
##########################################################################
  LIVE MODE: this bot will spend real SOL from ${wallet!.publicKey.toBase58()}
  Caps: ${config.spend.maxPerTradeSol} SOL per trade, ${config.spend.maxPerHourSol} per hour, ${config.spend.maxTotalSol} in total.
  Stop with Ctrl+C. Tokens bought this way can go to zero.
##########################################################################
`);
}

const feeds = [
  ...(env.ultraApiKey ? [{ name: "ultra", apiKey: env.ultraApiKey }] : []),
  ...(env.apiKey ? [{ name: "standard", apiKey: env.apiKey }] : []),
];
const network = new LiveNetwork(env.rpcUrls[0]!, env.jupiterApiKey);
// Dry runs and live trading keep separate state, so a dry run never uses up live limits.
let book: TradeBook;
try {
  book = TradeBook.open(`${env.stateDir}/${env.dryRun ? "dry-run" : "live"}.json`, env.dryRun ? "dry_run" : "live");
} catch (error) {
  if (error instanceof StateError) fail(error.message);
  throw error;
}
const bot = await startBot({
  config,
  network,
  book,
  feeds,
  wsUrl: env.wsUrl,
  dryRun: env.dryRun,
  wallet,
  senders: senderTargets(env, config), // unused in dry run, but they shape the tips each transaction carries
  rpcWsUrl: env.rpcWsUrl,
  onFatal: (reason) => {
    bot.stop();
    fail(reason);
  },
});

process.on("SIGINT", () => {
  bot.stop();
  for (const line of bot.report.summary()) log("summary", line);
  if (!env.ultraApiKey) log("next", `Ultra Speed gives your snipe accounts first-send priority: ${ultraTrialLink("cli-latency-report")}`);
  process.exit(0);
});
