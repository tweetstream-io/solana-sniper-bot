// Loads config.json: fills defaults from shared/config.schema.json, validates against the same
// schema the Rust bot uses, then checks what a schema cannot (32-byte mints, placeholders).
// Next: pipeline.ts builds the rule index from the result.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { Ajv2020, type ErrorObject } from "ajv/dist/2020.js";
import { isSolanaAddress } from "../detect/addresses.ts";
import type { Confidence, SignalSource } from "../detect/types.ts";

export type Mode = "ca_scanner" | "engagement" | "new_tweet" | "follow" | "pfp_update" | "callout" | "fomo";
export type KeywordFilter = { words: string[]; scanDepth: number };

type Filters = {
  maxSnipes: number;
  cooldownSec: number;
  minTokenAgeSec: number | null;
  maxTokenAgeSec: number | null;
  minMarketCapUsd: number | null;
  maxMarketCapUsd: number | null;
  minLiquidityUsd: number | null;
  maxPoolTaxPct: number | null;
  platforms: string[];
  skipSuspiciousPools: boolean;
  skipIfAlreadyBought: boolean;
  skipIfAlreadyHolding: boolean;
  onMissingData: "reject" | "allow";
};

type Fees = {
  slippagePct: number;
  priorityFeeSol: number | "auto";
  tipSol: number | "auto";
  antiMev: boolean;
  retriesOnGraduation: number;
};

// After defaults every rule carries every field; each mode reads its own.
export type Rule = {
  name: string;
  enabled: boolean;
  mode: Mode;
  accounts: string[];
  sources: Array<Exclude<SignalSource, "follow_bio" | "callout" | "fomo">>;
  minConfidence: Confidence;
  requireMintInBio: boolean;
  reactor: string;
  targetTweetId: string;
  reactedTo: string[];
  actions: Array<"retweet" | "quote" | "reply">;
  includeKeywords: KeywordFilter;
  excludeKeywords: KeywordFilter;
  events: Array<"post" | "retweet" | "quote" | "reply" | "bio">;
  followEvents: Array<"follow" | "unfollow">;
  targets: string[];
  fields: Array<"avatar" | "banner" | "name" | "bio" | "websiteUrl">;
  callers: string[];
  calloutKinds: Array<"callout" | "author_update" | "reply" | "repost">;
  maxMultiple: number | null;
  requireCallerPosition: boolean;
  sellWhenCallerExits: boolean;
  fomoUsers: string[];
  fomoCoins: string[];
  fomoEvents: Array<"thesis" | "token_verified">;
  sellWhenAuthorExits: boolean;
  useDetectedCa: boolean;
  token: { mint: string; action: "buy" | "sell"; amountSol: number };
  filters: Filters;
  fees: Fees;
};

type Sender = { name: string; urlEnv: string; tipAccounts: string[]; tipSol: number };

export type Config = {
  rules: Rule[];
  maxSignalAgeMs: number;
  safety: {
    budgetMs: number;
    onUnknown: "reject" | "allow";
    rugcheck: "off" | "advisory" | "required";
    maxRugcheckScore: number | null;
    rejectMintAuthority: boolean;
    rejectFreezeAuthority: boolean;
    rejectRiskyExtensions: boolean;
  };
  spend: { maxPerTradeSol: number; maxPerHourSol: number; maxTotalSol: number };
  exits: {
    takeProfitPct: number | null;
    stopLossPct: number | null;
    maxHoldSec: number | null;
    pollMs: number;
    sellOnDelete: boolean;
    sellSlippagePct: number;
  };
  execution: {
    pumpBuilder: "local" | "pumpportal";
    computeUnitLimit: number;
    maxAutoPriorityFeeSol: number;
    maxAutoTipSol: number;
    blockhashRefreshMs: number;
    rebroadcastMs: number;
    confirmTimeoutMs: number;
  };
  senders: Sender[];
};

export class ConfigError extends Error {}

type SchemaNode = {
  $ref?: string;
  type?: string | string[];
  default?: unknown;
  properties?: Record<string, SchemaNode>;
  items?: SchemaNode;
  description?: string;
  $defs?: Record<string, SchemaNode>;
};

const SCHEMA_PATH = fileURLToPath(new URL("../../../shared/config.schema.json", import.meta.url));
export const SCHEMA = JSON.parse(readFileSync(SCHEMA_PATH, "utf8")) as SchemaNode;
const validate = new Ajv2020({ allErrors: true, strict: true, strictTypes: false, strictRequired: false }).compile(SCHEMA);

export function loadConfig(path: string): Config {
  let text: string;
  try {
    text = readFileSync(path, "utf8");
  } catch {
    throw new ConfigError(`Cannot read ${path}. Copy config.example.json to config.json in the repository root, then edit the handles and amounts.`);
  }
  let json: unknown;
  try {
    json = JSON.parse(text);
  } catch (error) {
    throw new ConfigError(`${path} is not valid JSON: ${(error as Error).message}`);
  }
  return parseConfig(json);
}

export function parseConfig(json: unknown): Config {
  const config = withDefaults(SCHEMA, structuredClone(json));
  if (!validate(config)) {
    // A misspelled field explains the errors that follow from it, so report it first.
    const errors = validate.errors ?? [];
    throw new ConfigError(explain(errors.find((e) => e.keyword === "additionalProperties") ?? errors[0], config));
  }
  return checkSemantics(config as Config);
}

const resolve = (node: SchemaNode): SchemaNode =>
  node.$ref ? resolve(SCHEMA.$defs![node.$ref.replace("#/$defs/", "")]!) : node;

// Fills every missing property that has a schema default, and walks into objects and arrays.
// The schema is the only place defaults are written down.
function withDefaults(schemaNode: SchemaNode, value: unknown): unknown {
  const node = resolve(schemaNode);
  if (Array.isArray(value) && node.items) return value.map((item) => withDefaults(node.items!, item));
  if (typeof value !== "object" || value === null || !node.properties) return value;
  const out = value as Record<string, unknown>;
  for (const [key, child] of Object.entries(node.properties)) {
    const resolved = resolve(child);
    if (!(key in out)) {
      if ("default" in child || "default" in resolved) out[key] = structuredClone(child.default ?? resolved.default);
      else if (resolved.properties) out[key] = {};
      else continue;
    }
    out[key] = withDefaults(child, out[key]);
  }
  return out;
}

// Turns the first schema error into a sentence naming the rule and the field.
function explain(error: ErrorObject | undefined, config: unknown): string {
  if (!error) return "config does not match shared/config.schema.json";
  const parts = error.instancePath.split("/").filter(Boolean);
  let where = "config";
  if (parts[0] === "rules" && parts[1] !== undefined) {
    const rule = (config as { rules: Array<{ name?: string }> }).rules[Number(parts[1])];
    where = `rule "${rule?.name ?? `#${Number(parts[1]) + 1}`}"`;
    parts.splice(0, 2);
  }
  const field = parts.join(".");
  const params = error.params as { allowedValues?: unknown[]; missingProperty?: string; additionalProperty?: string; pattern?: string };
  if (field === "token.mint" && error.keyword === "minLength") {
    return `${where}: set token.mint to the Solana mint to trade, or set "useDetectedCa": true to trade the address found in the event.`;
  }
  if (params.missingProperty === "reactor") return `${where}: an engagement rule needs "reactor": the handle or id whose retweets, quotes, or replies fire it.`;
  if (error.keyword === "anyOf" && field === "") {
    return `${where}: an engagement rule needs targetTweetId or reactedTo; a fomo rule needs fomoUsers or fomoCoins.`;
  }
  if (error.keyword === "minItems" && ["accounts", "callers", "fomoCoins"].includes(field)) {
    return `${where}: ${field} needs at least one entry${field === "fomoCoins" ? " (token_verified only fires for watched coins)" : ""}.`;
  }
  if (params.additionalProperty) return `${where}: unknown field "${[field, params.additionalProperty].filter(Boolean).join(".")}". Check the spelling against docs/config.md.`;
  if (params.allowedValues) return `${where}: ${field} must be one of ${params.allowedValues.map((v) => JSON.stringify(v)).join(", ")}.`;
  if (error.keyword === "pattern" && /account|reactor|reacted|targets/i.test(field)) {
    return `${where}: ${field} must be an X handle (letters, digits, _; at most 15) or a numeric account id.`;
  }
  if (error.keyword === "pattern" && field.endsWith("mint")) return `${where}: ${field} is not a Solana address (base58, 32 to 44 characters).`;
  return `${where}: ${field || "config"} ${error.message}.`;
}

const PLACEHOLDER = /^@?YOUR_/i;

// Checks the schema cannot express. Disabled rules may keep placeholders.
function checkSemantics(config: Config): Config {
  const names = new Set<string>();
  for (const rule of config.rules) {
    if (names.has(rule.name)) throw new ConfigError(`rule names must be unique: "${rule.name}" appears twice.`);
    names.add(rule.name);
    if (!rule.enabled) continue;
    const where = `rule "${rule.name}"`;
    for (const mint of [rule.token.mint, ...rule.fomoCoins]) {
      if (mint && !isSolanaAddress(mint)) throw new ConfigError(`${where}: "${mint}" does not decode to a 32-byte Solana address.`);
    }
    for (const spec of [...rule.accounts, rule.reactor, ...rule.reactedTo, ...rule.targets, ...rule.callers, ...rule.fomoUsers]) {
      if (spec && PLACEHOLDER.test(spec)) throw new ConfigError(`${where}: replace the placeholder "${spec}" with an X handle or numeric account id.`);
    }
  }
  if (!config.rules.some((r) => r.enabled)) throw new ConfigError('every rule has "enabled": false. Turn one on.');
  for (const sender of config.senders) {
    for (const account of sender.tipAccounts) {
      if (!isSolanaAddress(account)) throw new ConfigError(`senders "${sender.name}": tip account "${account}" is not a Solana address.`);
    }
  }
  return config;
}

// Handles can change owner; numeric ids cannot. Printed at startup for every handle in use.
export function handleWarnings(config: Config): string[] {
  const out: string[] = [];
  for (const rule of config.rules.filter((r) => r.enabled)) {
    const specs = rule.mode === "engagement" ? [rule.reactor] : rule.accounts;
    for (const spec of specs) {
      if (!/^\d+$/.test(spec)) {
        out.push(`rule "${rule.name}" watches ${spec} by handle. A handle can be renamed or taken over; the numeric account id cannot.`);
      }
    }
  }
  return out;
}
