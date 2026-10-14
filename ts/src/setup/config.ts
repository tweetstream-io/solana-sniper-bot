// Loads config.json: fills defaults from shared/config.schema.json, validates against the same
// schema the Rust bot uses, then checks what a schema cannot (32-byte mints, placeholders).
// Next: pipeline.ts builds the rule index from the result.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { Ajv2020, type ErrorObject } from "ajv/dist/2020.js";
import { denylistName, isSolanaAddress } from "../detect/addresses.ts";
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
  skipSuspiciousPools: boolean;
  onMissingData: "reject" | "allow";
};

type Fees = {
  slippagePct: number;
  priorityFeeSol: number | "auto";
  tipSol: number | "auto";
  antiMev: boolean;
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
  fomoUsers: string[];
  fomoCoins: string[];
  fomoEvents: Array<"thesis" | "token_verified">;
  useDetectedCa: boolean;
  token: { mint: string; amountSol: number };
  filters: Filters;
  fees: Fees;
};

export type Config = {
  rules: Rule[];
  maxSignalAgeMs: number;
  safety: {
    budgetMs: number;
    onUnknown: "reject" | "allow";
    rugcheck: "off" | "advisory" | "required";
    maxRugcheckScore: number | null;
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
    computeUnitLimit: number;
    maxAutoPriorityFeeSol: number;
    maxAutoTipSol: number;
    blockhashRefreshMs: number;
    rebroadcastMs: number;
    confirmTimeoutMs: number;
  };
};

// rule: the rule's name, or null for top-level fields. field: the dot-separated path inside that
// rule (or the config), as compared by shared/fixtures/config.
export class ConfigError extends Error {
  readonly rule: string | null;
  readonly field: string | null;
  constructor(message: string, rule: string | null = null, field: string | null = null) {
    super(message);
    this.rule = rule;
    this.field = field;
  }
}

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
    throw explain(errors.find((e) => e.keyword === "additionalProperties") ?? errors[0], config);
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
      if ("default" in child) out[key] = structuredClone(child.default);
      else if ("default" in resolved) out[key] = structuredClone(resolved.default);
      else if (resolved.properties) out[key] = {};
      else continue;
    }
    out[key] = withDefaults(child, out[key]);
  }
  return out;
}

// Turns the first schema error into a sentence naming the rule and the field.
function explain(error: ErrorObject | undefined, config: unknown): ConfigError {
  if (!error) return new ConfigError("config does not match shared/config.schema.json");
  const parts = error.instancePath.split("/").filter(Boolean);
  let where = "config";
  let ruleName: string | null = null;
  if (parts[0] === "rules" && parts[1] !== undefined) {
    const rule = (config as { rules: Array<{ name?: string }> }).rules[Number(parts[1])];
    ruleName = rule?.name ?? `#${Number(parts[1]) + 1}`;
    where = `rule "${ruleName}"`;
    parts.splice(0, 2);
  }
  const params = error.params as { allowedValues?: unknown[]; missingProperty?: string; additionalProperty?: string; pattern?: string };
  const field = [...parts, params.additionalProperty ?? params.missingProperty].filter(Boolean).join(".");
  return new ConfigError(describe(error, where, field, params), ruleName, field || null);
}

function describe(error: ErrorObject, where: string, field: string, params: { allowedValues?: unknown[]; missingProperty?: string; additionalProperty?: string }): string {
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
  if (params.additionalProperty) return `${where}: unknown field "${field}". Check the spelling against docs/config.md.`;
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
    if (names.has(rule.name)) throw new ConfigError(`rule names must be unique: "${rule.name}" appears twice.`, rule.name, "name");
    names.add(rule.name);
    if (!rule.enabled) continue;
    const where = `rule "${rule.name}"`;
    const mints: Array<[string, string]> = [["token.mint", rule.token.mint], ...rule.fomoCoins.map((m, i): [string, string] => [`fomoCoins.${i}`, m])];
    for (const [field, mint] of mints) {
      if (!mint || isSolanaAddress(mint)) continue;
      const denied = denylistName(mint);
      const why = denied ? `is the ${denied}, which shared/addresses.json denylists because it is never a token to snipe` : "does not decode to a 32-byte Solana address";
      throw new ConfigError(`${where}: "${mint}" ${why}.`, rule.name, field);
    }
    const lists = { accounts: rule.accounts, reactedTo: rule.reactedTo, targets: rule.targets, callers: rule.callers, fomoUsers: rule.fomoUsers };
    const specs: Array<[string, string]> = [["reactor", rule.reactor ?? ""], ...Object.entries(lists).flatMap(([k, list]) => list.map((v, i): [string, string] => [`${k}.${i}`, v]))];
    for (const [field, spec] of specs) {
      if (spec && PLACEHOLDER.test(spec)) throw new ConfigError(`${where}: replace the placeholder "${spec}" with the real account.`, rule.name, field);
    }
  }
  if (!config.rules.some((r) => r.enabled)) throw new ConfigError('every rule has "enabled": false. Turn one on.', null, "rules");
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
