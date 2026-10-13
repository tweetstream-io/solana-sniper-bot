// Safety checks and rule filters for one fired rule, all started together under one time budget.
// The only read every buy needs is one getMultipleAccounts for the mint and its pump.fun curve;
// it decides the route as well. Next: execute/trade.ts builds and signs the transaction.

import type { Config, Rule } from "../setup/config.ts";
import type { Signal } from "../detect/types.ts";
import type { MarketSnapshot, Network, RugcheckSummary } from "../network.ts";
import { bondingCurveAddress, localBuildBlocker, parseCurve, TOKEN_2022_PROGRAM, TOKEN_PROGRAM, type Curve } from "../execute/pump.ts";

export type Route = "pump" | "pumpportal" | "jupiter";

export type Facts = {
  platform?: string;
  marketCapUsd?: number;
  liquidityUsd?: number;
  ageSec?: number;
  taxPct?: number;
  suspicious?: boolean;
};

export type GateCode =
  | "check_unknown"
  | "not_a_mint"
  | "mint_authority"
  | "freeze_authority"
  | "risky_extension"
  | "rugcheck_danger"
  | "rugcheck_score"
  | "already_holding"
  | "filter_platform"
  | "filter_tax"
  | "filter_market_cap"
  | "filter_liquidity"
  | "filter_age"
  | "filter_suspicious"
  | "missing_data";

type Verdict =
  | { pass: true; route: Route; curve: Curve | null; tokenProgram: string; facts: Facts }
  | { pass: false; code: GateCode; detail: string; facts: Facts };

export type GateResult = Verdict & { elapsedMs: number };

export type GateDeps = {
  network: Network;
  safety: Config["safety"];
  pumpBuilder: Config["execution"]["pumpBuilder"];
  solUsd: () => number | null; // cached by execute/trade.ts, refreshed in the background
  holding: (mint: string) => boolean;
};

type Settled<T> = { ok: true; value: T } | { ok: false; error: string };
const settle = <T>(p: Promise<T>): Promise<Settled<T>> =>
  p.then((value) => ({ ok: true as const, value })).catch((e: Error) => ({ ok: false as const, error: e.name === "TimeoutError" || e.name === "AbortError" ? "timed out" : e.message }));

// Token-2022 extension types (spl-token-2022 ExtensionType) that let someone other than the holder
// block, take, or tax transfers.
const RISKY_EXTENSIONS: Record<number, string> = {
  6: "default account state (accounts start frozen)",
  9: "non-transferable",
  12: "permanent delegate (someone can move your tokens)",
  14: "transfer hook (custom code runs on every transfer)",
  26: "pausable",
};

export type MintInfo = {
  mintAuthority: boolean;
  freezeAuthority: boolean;
  supply: bigint;
  decimals: number;
  extensions: number[];
  transferFeeBps: number;
};

// SPL Mint: 82 bytes (COption<Pubkey> mint authority, u64 supply, u8 decimals, bool, COption<Pubkey>
// freeze authority). Token-2022 adds, after byte 165, an account-type byte and TLV extensions.
export function parseMint(data: Uint8Array): MintInfo | null {
  if (data.length < 82) return null;
  const view = Buffer.from(data.buffer, data.byteOffset, data.byteLength);
  const info: MintInfo = {
    mintAuthority: view.readUInt32LE(0) === 1,
    supply: view.readBigUInt64LE(36),
    decimals: view[44]!,
    freezeAuthority: view.readUInt32LE(46) === 1,
    extensions: [],
    transferFeeBps: 0,
  };
  for (let at = 166; data.length > 165 && view[165] === 1 && at + 4 <= data.length; ) {
    const type = view.readUInt16LE(at);
    const length = view.readUInt16LE(at + 2);
    if (type === 0) break;
    info.extensions.push(type);
    // TransferFeeConfig: the newer fee's basis points sit 106 bytes into the value.
    if (type === 1 && length >= 108) info.transferFeeBps = view.readUInt16LE(at + 4 + 106);
    at += 4 + length;
  }
  return info;
}

const needsMarketData = (f: Rule["filters"]) =>
  f.minTokenAgeSec !== null || f.maxTokenAgeSec !== null || f.minMarketCapUsd !== null || f.maxMarketCapUsd !== null || f.minLiquidityUsd !== null || f.platforms.length > 0;

export async function runGate(mint: string, rule: Rule, signal: Signal | null, deps: GateDeps): Promise<GateResult> {
  const started = performance.now();
  const budget = AbortSignal.timeout(deps.safety.budgetMs);
  const [accounts, market, rug] = await Promise.all([
    settle(deps.network.getAccounts([mint, bondingCurveAddress(mint).toBase58()], budget)),
    needsMarketData(rule.filters) ? settle(deps.network.marketSnapshot(mint, budget)) : null,
    deps.safety.rugcheck !== "off" ? settle(deps.network.rugcheck(mint, budget)) : null,
  ]);
  const verdict = judge(mint, rule, signal, deps, accounts, market, rug);
  return { ...verdict, elapsedMs: performance.now() - started };
}

function judge(
  mint: string,
  rule: Rule,
  signal: Signal | null,
  deps: GateDeps,
  accounts: Settled<Array<{ owner: string; data: Uint8Array } | null>>,
  market: Settled<MarketSnapshot> | null,
  rug: Settled<RugcheckSummary | null> | null,
): Verdict {
  const { safety } = deps;
  const facts: Facts = {};
  const fail = (code: GateCode, detail: string) => ({ pass: false as const, code, detail, facts });

  let route: Route = "jupiter";
  let curve: Curve | null = null;
  let tokenProgram = TOKEN_PROGRAM.toBase58();
  if (!accounts.ok) {
    if (safety.onUnknown === "reject") return fail("check_unknown", `mint read ${accounts.error} (budget ${safety.budgetMs} ms)`);
  } else {
    const [mintAccount, curveAccount] = accounts.value;
    if (!mintAccount) return fail("not_a_mint", "no account at this address yet");
    const owner = mintAccount.owner;
    if (owner !== TOKEN_PROGRAM.toBase58() && owner !== TOKEN_2022_PROGRAM.toBase58()) {
      return fail("not_a_mint", `owned by ${owner}, not a token program (a chart link can point at a pool)`);
    }
    const info = parseMint(mintAccount.data);
    if (!info) return fail("not_a_mint", "account data is too short for a mint");
    tokenProgram = owner;
    curve = curveAccount ? parseCurve(curveAccount.data) : null;
    const live = curve !== null && !curve.complete;
    // pump.fun creates every curve mint with no mint or freeze authority, so these checks cannot
    // fail there; skipping them keeps a curve buy to the single read above.
    if (!live) {
      if (safety.rejectMintAuthority && info.mintAuthority) return fail("mint_authority", "the supply can still be increased");
      if (safety.rejectFreezeAuthority && info.freezeAuthority) return fail("freeze_authority", "holder accounts can be frozen");
      const risky = info.extensions.find((t) => t in RISKY_EXTENSIONS);
      if (safety.rejectRiskyExtensions && risky !== undefined) return fail("risky_extension", RISKY_EXTENSIONS[risky]!);
    }
    facts.taxPct = info.transferFeeBps / 100;
    if (live) {
      route = localBuildBlocker(curve!) || deps.pumpBuilder === "pumpportal" ? "pumpportal" : "pump";
      facts.platform = "pumpfun";
      const solUsd = deps.solUsd();
      if (solUsd !== null) {
        // Market cap in lamports = price per raw unit (vQuote / vToken) x total raw supply.
        const capLamports = (curve!.virtualQuoteReserves * curve!.tokenTotalSupply) / curve!.virtualTokenReserves;
        facts.marketCapUsd = (Number(capLamports) / 1e9) * solUsd;
        facts.liquidityUsd = (Number(curve!.realQuoteReserves) / 1e9) * solUsd;
      }
    }
  }
  if (market?.ok) {
    facts.platform ??= market.value.platform;
    facts.marketCapUsd ??= market.value.marketCapUsd;
    facts.liquidityUsd ??= market.value.liquidityUsd;
    if (market.value.createdAtMs !== undefined) facts.ageSec = Math.max(0, (Date.now() - market.value.createdAtMs) / 1000);
  }
  if (signal?.marketCapUsd !== undefined) facts.marketCapUsd = signal.marketCapUsd;
  if (rug?.ok && rug.value) facts.suspicious = rug.value.dangers.length > 0;

  const f = rule.filters;
  if (f.skipIfAlreadyHolding && deps.holding(mint)) return fail("already_holding", "the bot still holds this token");
  const missing = (filter: string, what: string) =>
    f.onMissingData === "reject" ? fail("missing_data", `${filter} needs ${what}, and none arrived within ${safety.budgetMs} ms`) : null;
  const range = (value: number | undefined, min: number | null, max: number | null, code: GateCode, name: string, unit: string) => {
    if (min === null && max === null) return null;
    if (value === undefined) return missing(name, unit);
    if (min !== null && value < min) return fail(code, `${unit} ${Math.round(value)} is below ${min}`);
    if (max !== null && value > max) return fail(code, `${unit} ${Math.round(value)} is above ${max}`);
    return null;
  };
  if (f.platforms.length) {
    if (facts.platform === undefined) {
      const m = missing("platforms", "the venue");
      if (m) return m;
    } else if (!f.platforms.includes(facts.platform)) return fail("filter_platform", `venue ${facts.platform} is not in [${f.platforms.join(", ")}]`);
  }
  const checks = [
    range(facts.taxPct, null, f.maxPoolTaxPct, "filter_tax", "maxPoolTaxPct", "transfer fee %"),
    range(facts.marketCapUsd, f.minMarketCapUsd, f.maxMarketCapUsd, "filter_market_cap", "market cap filter", "market cap USD"),
    range(facts.liquidityUsd, f.minLiquidityUsd, null, "filter_liquidity", "minLiquidityUsd", "liquidity USD"),
    range(facts.ageSec, f.minTokenAgeSec, f.maxTokenAgeSec, "filter_age", "token age filter", "age in seconds"),
  ];
  const failed = checks.find((c) => c !== null);
  if (failed) return failed;

  if (rug) {
    if (!rug.ok || !rug.value) {
      if (safety.rugcheck === "required" && safety.onUnknown === "reject") return fail("check_unknown", `rugcheck ${rug.ok ? "had no report" : rug.error}`);
      if (f.skipSuspiciousPools) {
        const m = missing("skipSuspiciousPools", "a rugcheck report");
        if (m) return m;
      }
    } else {
      if (rug.value.dangers.length && (f.skipSuspiciousPools || safety.rugcheck !== "off")) {
        return fail(f.skipSuspiciousPools ? "filter_suspicious" : "rugcheck_danger", rug.value.dangers.join(", "));
      }
      if (safety.maxRugcheckScore !== null && rug.value.score !== null && rug.value.score > safety.maxRugcheckScore) {
        return fail("rugcheck_score", `score ${rug.value.score} is above ${safety.maxRugcheckScore}`);
      }
    }
  } else if (f.skipSuspiciousPools) {
    const m = missing("skipSuspiciousPools", "a rugcheck report (turn on safety.rugcheck)");
    if (m) return m;
  }
  return { pass: true, route, curve, tokenProgram, facts };
}
