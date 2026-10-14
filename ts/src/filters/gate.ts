// Safety checks and rule filters for one fired rule, all started together under one time budget.
// The only read every buy needs is one getMultipleAccounts for the mint and its pump.fun curve;
// only a live pump.fun curve quoted in SOL passes. Next: execute/trade.ts builds and signs the buy.

import type { Config, Rule } from "../setup/config.ts";
import type { Signal } from "../detect/types.ts";
import type { MarketSnapshot, Network, RugcheckSummary } from "../network.ts";
import { bondingCurveAddress, parseCurve, unsupportedCurve, TOKEN_2022_PROGRAM, TOKEN_PROGRAM, type Curve } from "../execute/pump.ts";

type Facts = {
  marketCapUsd?: number;
  liquidityUsd?: number;
  ageSec?: number;
  taxPct?: number;
  suspicious?: boolean;
};

type GateCode =
  | "check_unknown"
  | "not_a_mint"
  | "rugcheck_danger"
  | "rugcheck_score"
  | "filter_tax"
  | "filter_market_cap"
  | "filter_liquidity"
  | "filter_age"
  | "filter_suspicious"
  | "missing_data"
  | "unsupported_curve";

type Verdict =
  | { pass: true; curve: Curve; tokenProgram: string; facts: Facts }
  | { pass: false; code: GateCode; detail: string; facts: Facts };

export type GateResult = Verdict & { elapsedMs: number };

export type GateDeps = {
  network: Network;
  safety: Config["safety"];
  solUsd: () => number | null; // from execute/cache.ts, refreshed in the background
  epoch: () => number | null; // the current epoch decides which Token-2022 transfer fee applies
};

type Settled<T> = { ok: true; value: T } | { ok: false; error: string };
const settle = <T>(p: Promise<T>): Promise<Settled<T>> =>
  p.then((value) => ({ ok: true as const, value })).catch((e: Error) => ({ ok: false as const, error: e.name === "TimeoutError" || e.name === "AbortError" ? "timed out" : e.message }));

// A Token-2022 transfer fee has an older and a newer setting; the newer one applies from its epoch.
type TransferFee = { older: { epoch: bigint; bps: number }; newer: { epoch: bigint; bps: number } };

export type MintInfo = {
  mintAuthority: boolean;
  freezeAuthority: boolean;
  supply: bigint;
  decimals: number;
  extensions: number[];
  transferFee: TransferFee | null;
};

// The fee in force at `epoch`; with no epoch known, the higher of the two.
export function transferFeeBps(fee: TransferFee | null, epoch: number | null): number {
  if (!fee) return 0;
  if (epoch === null) return Math.max(fee.older.bps, fee.newer.bps);
  return BigInt(epoch) >= fee.newer.epoch ? fee.newer.bps : fee.older.bps;
}

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
    transferFee: null,
  };
  for (let at = 166; data.length > 165 && view[165] === 1 && at + 4 <= data.length; ) {
    const type = view.readUInt16LE(at);
    const length = view.readUInt16LE(at + 2);
    if (type === 0) break;
    info.extensions.push(type);
    // TransferFeeConfig: two authorities (64 bytes) and withheld amount (8), then the older and the
    // newer fee, each {epoch u64, maximum fee u64, basis points u16}.
    if (type === 1 && length >= 108) {
      const v = at + 4;
      info.transferFee = {
        older: { epoch: view.readBigUInt64LE(v + 72), bps: view.readUInt16LE(v + 88) },
        newer: { epoch: view.readBigUInt64LE(v + 90), bps: view.readUInt16LE(v + 106) },
      };
    }
    at += 4 + length;
  }
  return info;
}

const needsMarketData = (f: Rule["filters"]) =>
  f.minTokenAgeSec !== null || f.maxTokenAgeSec !== null || f.minMarketCapUsd !== null || f.maxMarketCapUsd !== null || f.minLiquidityUsd !== null;

export async function runGate(mint: string, rule: Rule, signal: Signal | null, deps: GateDeps): Promise<GateResult> {
  const started = performance.now();
  const budget = AbortSignal.timeout(deps.safety.budgetMs);
  const [accounts, market, rug] = await Promise.all([
    settle(deps.network.getAccounts([mint, bondingCurveAddress(mint).toBase58()], budget)),
    needsMarketData(rule.filters) ? settle(deps.network.marketSnapshot(mint, budget)) : null,
    deps.safety.rugcheck !== "off" ? settle(deps.network.rugcheck(mint, budget)) : null,
  ]);
  const verdict = judge(rule, signal, deps, accounts, market, rug);
  return { ...verdict, elapsedMs: performance.now() - started };
}

function judge(
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

  // The bot buys only on a live pump.fun curve, so without the mint and curve reads there is no buy.
  if (!accounts.ok) return fail("check_unknown", `mint read ${accounts.error} (budget ${safety.budgetMs} ms)`);
  let curve: Curve;
  let tokenProgram: string;
  {
    const [mintAccount, curveAccount] = accounts.value;
    if (!mintAccount) return fail("not_a_mint", "no account at this address yet");
    const owner = mintAccount.owner;
    if (owner !== TOKEN_PROGRAM.toBase58() && owner !== TOKEN_2022_PROGRAM.toBase58()) {
      return fail("not_a_mint", `owned by ${owner}, not a token program (a chart link can point at a pool)`);
    }
    const info = parseMint(mintAccount.data);
    if (!info) return fail("not_a_mint", "account data is too short for a mint");
    tokenProgram = owner;
    const parsed = curveAccount ? parseCurve(curveAccount.data) : null;
    // pump.fun creates every curve mint itself, with no mint or freeze authority, so a live curve
    // needs no authority checks.
    const live = parsed !== null && !parsed.complete;
    if (!live) return fail("unsupported_curve", parsed ? "the coin left its pump.fun curve" : "not a pump.fun curve coin");
    curve = parsed;
    const unsupported = unsupportedCurve(curve);
    if (unsupported) return fail("unsupported_curve", unsupported);
    facts.taxPct = transferFeeBps(info.transferFee, deps.epoch()) / 100;
    const solUsd = deps.solUsd();
    if (solUsd !== null) {
      // Market cap in lamports = price per raw unit (vQuote / vToken) x total raw supply.
      const capLamports = (curve.virtualQuoteReserves * curve.tokenTotalSupply) / curve.virtualTokenReserves;
      facts.marketCapUsd = (Number(capLamports) / 1e9) * solUsd;
      facts.liquidityUsd = (Number(curve.realQuoteReserves) / 1e9) * solUsd;
    }
  }
  if (market?.ok) {
    facts.marketCapUsd ??= market.value.marketCapUsd;
    facts.liquidityUsd ??= market.value.liquidityUsd;
    if (market.value.createdAtMs !== undefined) facts.ageSec = Math.max(0, (Date.now() - market.value.createdAtMs) / 1000);
  }
  if (signal?.marketCapUsd !== undefined) facts.marketCapUsd = signal.marketCapUsd;
  if (rug?.ok && rug.value) facts.suspicious = rug.value.dangers.length > 0;

  const f = rule.filters;
  const missing = (filter: string, what: string) =>
    f.onMissingData === "reject" ? fail("missing_data", `${filter} needs ${what}, and none arrived within ${safety.budgetMs} ms`) : null;
  const range = (value: number | undefined, min: number | null, max: number | null, code: GateCode, name: string, unit: string) => {
    if (min === null && max === null) return null;
    if (value === undefined) return missing(name, unit);
    if (min !== null && value < min) return fail(code, `${unit} ${Math.round(value)} is below ${min}`);
    if (max !== null && value > max) return fail(code, `${unit} ${Math.round(value)} is above ${max}`);
    return null;
  };
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
      if (safety.maxRugcheckScore !== null) {
        if (rug.value.score === null) {
          if (safety.rugcheck === "required" && safety.onUnknown === "reject") return fail("check_unknown", "rugcheck report has no score");
        } else if (rug.value.score > safety.maxRugcheckScore) {
          return fail("rugcheck_score", `score ${rug.value.score} is above ${safety.maxRugcheckScore}`);
        }
      }
    }
  } else if (f.skipSuspiciousPools) {
    const m = missing("skipSuspiciousPools", "a rugcheck report (turn on safety.rugcheck)");
    if (m) return m;
  }
  return { pass: true, curve, tokenProgram, facts };
}
