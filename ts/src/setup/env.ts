// Reads secrets and endpoints from the environment (.env). Every problem gets a message that says
// exactly what to change. Next: setup/config.ts loads the strategy file.

import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

export type Env = {
  apiKey: string | null;
  ultraApiKey: string | null;
  wsUrl: string;
  configPath: string;
  stateDir: string;
  dryRun: boolean;
  rpcUrls: string[];
  rpcWsUrl: string;
  walletSecret: string | null;
  jupiterApiKey: string | null;
  jitoUrl: string | null;
};

export class SetupError extends Error {}

// The US endpoint. Run the bot in Ashburn, Virginia and keep it next to this endpoint.
const DEFAULT_WS_URL = "wss://ws-iad.tweetstream.io/ws";
const DEFAULT_RPC = "https://api.mainnet-beta.solana.com";
const DEFAULT_JITO = "https://ny.mainnet.block-engine.jito.wtf";

const text = (name: string) => process.env[name]?.trim() || null;
// .env, config.json, and state/ live at the repository root, shared by both implementations.
export const REPO_ROOT = fileURLToPath(new URL("../../../", import.meta.url));

export function readEnv(): Env {
  const envFile = resolve(REPO_ROOT, ".env");
  if (existsSync(envFile)) process.loadEnvFile(envFile); // values already in the environment win
  const dryRunRaw = (text("DRY_RUN") ?? "true").toLowerCase();
  if (!["true", "false"].includes(dryRunRaw)) {
    throw new SetupError(`DRY_RUN must be "true" or "false", got "${dryRunRaw}".`);
  }
  const rpcUrls = (text("SOLANA_RPC_URLS") ?? DEFAULT_RPC).split(",").map((u) => u.trim()).filter(Boolean);
  for (const url of rpcUrls) {
    if (!/^https?:\/\//.test(url)) throw new SetupError(`SOLANA_RPC_URLS: "${url}" must start with https://`);
  }
  const jito = text("JITO_URL");
  return {
    apiKey: text("TWEETSTREAM_API_KEY"),
    ultraApiKey: text("TWEETSTREAM_ULTRA_API_KEY"),
    wsUrl: text("TWEETSTREAM_WS_URL") ?? DEFAULT_WS_URL,
    configPath: resolve(REPO_ROOT, text("SNIPER_CONFIG") ?? "config.json"),
    stateDir: resolve(REPO_ROOT, text("STATE_DIR") ?? "state"),
    dryRun: dryRunRaw !== "false",
    rpcUrls,
    rpcWsUrl: text("SOLANA_WS_URL") ?? rpcUrls[0]!.replace(/^http/, "ws"),
    walletSecret: text("WALLET_SECRET_KEY"),
    jupiterApiKey: text("JUPITER_API_KEY"),
    jitoUrl: jito === "off" ? null : (jito ?? DEFAULT_JITO),
  };
}

const UTM = "utm_source=github&utm_medium=referral&utm_campaign=solana-sniper-bot";
// Every tweetstream.io link the bot prints carries the same campaign and a placement name.
export const productLink = (path: string, placement: string) => `https://tweetstream.io${path}${path.includes("?") ? "&" : "?"}${UTM}&utm_content=${placement}`;
export const trialLink = (placement: string) => productLink("/signup?offer=basic-monthly-trial", placement);
export const ultraTrialLink = (placement: string) => productLink("/signup?intent=ultra_speed&ultraBilling=MONTHLY", placement);
const DASHBOARD_KEYS = productLink("/dashboard?tab=api", "cli-missing-key");

export const SETUP_HELP = `
No TweetStream API key found. The bot needs one to read the live feed.

  1. Start a trial: ${trialLink("cli-missing-key")}
  2. Copy your key from ${DASHBOARD_KEYS}
  3. In the repository root: cp .env.example .env, then paste the key into TWEETSTREAM_API_KEY
  4. In the repository root: cp config.example.json config.json, then put your handles and amounts in it
  5. npm start

The bot starts in dry run: it prints what it would buy and sends nothing.
To see it work first without a key, run: npm run demo
`;
