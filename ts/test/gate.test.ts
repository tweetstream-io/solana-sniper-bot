// Safety checks and filters through runGate, with a hand-built network that answers like the RPC.

import { test } from "node:test";
import assert from "node:assert/strict";
import { runGate, type GateDeps } from "../src/filters/gate.ts";
import { parseConfig } from "../src/setup/config.ts";
import { bondingCurveAddress } from "../src/execute/pump.ts";
import type { AccountData, Network } from "../src/network.ts";

const MINT = "FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA";
const SPL = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const T22 = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

function mint(owner: string, { mintAuthority = false, freezeAuthority = false, feeBps = 0 } = {}): AccountData {
  const base = Buffer.alloc(82);
  base.writeUInt32LE(mintAuthority ? 1 : 0, 0);
  base.writeBigUInt64LE(10n ** 15n, 36);
  base[44] = 6;
  base.writeUInt32LE(freezeAuthority ? 1 : 0, 46);
  if (!feeBps) return { owner, data: base };
  const tlv = Buffer.alloc(4 + 108);
  tlv.writeUInt16LE(1, 0);
  tlv.writeUInt16LE(108, 2);
  tlv.writeUInt16LE(feeBps, 4 + 106);
  return { owner, data: Buffer.concat([base, Buffer.alloc(83), Buffer.from([1]), tlv]) };
}

function liveCurve(quoteMint?: Buffer): AccountData {
  const data = Buffer.alloc(115);
  if (quoteMint) quoteMint.copy(data, 83); // quote_mint; all zeros means SOL
  Buffer.from([23, 183, 248, 55, 96, 216, 172, 96]).copy(data, 0);
  data.writeBigUInt64LE(10n ** 15n, 8); // virtual token reserves
  data.writeBigUInt64LE(32n * 10n ** 9n, 16); // virtual SOL reserves: 32 SOL
  data.writeBigUInt64LE(2n * 10n ** 9n, 32); // real SOL reserves: 2 SOL
  data.writeBigUInt64LE(10n ** 15n, 40); // total supply
  return { owner: "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P", data };
}

function network(accounts: Record<string, AccountData>, delayMs = 0, rug: { score: number | null; dangers: string[] } | null = null): Network {
  const never = () => new Promise<never>(() => {});
  return {
    getAccounts: async (addresses, signal) => {
      if (delayMs) await new Promise((resolve, reject) => {
        const t = setTimeout(resolve, delayMs);
        signal?.addEventListener("abort", () => (clearTimeout(t), reject(Object.assign(new Error("aborted"), { name: "AbortError" }))));
      });
      return addresses.map((a) => accounts[a] ?? null);
    },
    marketSnapshot: async () => ({}),
    rugcheck: async () => rug,
    latestBlockhash: never,
    epochInfo: never,
    signatureStatus: never,
    transactionDebit: never,
    pumpPortalTx: never,
    jupiterOrder: never,
    jupiterExecute: never,
    recentPriorityFees: never,
    jitoTipFloorSol: never,
    solUsdPrice: never,
    quoteSellLamports: never,
    tokenBalance: never,
  };
}

const rule = (filters: object = {}) => parseConfig({ rules: [{ name: "r", mode: "ca_scanner", accounts: ["1"], filters }] }).rules[0]!;
const deps = (net: Network, budgetMs = 100, safety: object = {}): GateDeps => ({
  network: net,
  safety: parseConfig({ rules: [{ name: "r", mode: "ca_scanner", accounts: ["1"] }], safety: { budgetMs, ...safety } }).safety,
  pumpBuilder: "local",
  solUsd: () => 150,
  epoch: () => 850,
});
const curveKey = bondingCurveAddress(MINT).toBase58();

test("a live pump.fun curve skips authority checks, routes locally, and prices from the curve", async () => {
  const result = await runGate(MINT, rule(), null, deps(network({ [MINT]: mint(T22, { mintAuthority: true }), [curveKey]: liveCurve() })));
  assert.equal(result.pass, true);
  assert.equal(result.pass && result.route, "pump");
  // 32 SOL virtual reserves x 1e15 supply / 1e15 virtual tokens = 32 SOL = $4800 at $150.
  assert.equal(result.facts.marketCapUsd, 4800);
  assert.equal(result.facts.liquidityUsd, 300);
});

test("off the curve, a freeze authority rejects the token", async () => {
  const result = await runGate(MINT, rule(), null, deps(network({ [MINT]: mint(SPL, { freezeAuthority: true }) })));
  assert.equal(result.pass === false && result.code, "freeze_authority");
});

test("an address owned by another program is not a mint", async () => {
  const result = await runGate(MINT, rule(), null, deps(network({ [MINT]: { owner: "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P", data: Buffer.alloc(200) } })));
  assert.equal(result.pass === false && result.code, "not_a_mint");
});

test("a read slower than the budget counts as unknown and rejects", async () => {
  const result = await runGate(MINT, rule(), null, deps(network({ [MINT]: mint(SPL) }, 200), 20));
  assert.equal(result.pass === false && result.code, "check_unknown");
  assert.ok(result.elapsedMs < 150);
});

test("a market cap filter with no market data rejects unless the rule allows missing data", async () => {
  const net = network({ [MINT]: mint(SPL) });
  const strict = await runGate(MINT, rule({ maxMarketCapUsd: 100_000 }), null, deps(net));
  assert.equal(strict.pass === false && strict.code, "missing_data");
  const lenient = await runGate(MINT, rule({ maxMarketCapUsd: 100_000, onMissingData: "allow" }), null, deps(net));
  assert.equal(lenient.pass, true);
});

test("a 2.5% transfer fee fails the default maxPoolTaxPct of 0", async () => {
  const result = await runGate(MINT, rule(), null, deps(network({ [MINT]: mint(T22, { feeBps: 250 }) })));
  assert.equal(result.pass === false && result.code, "filter_tax");
  assert.equal(result.facts.taxPct, 2.5);
});

test("a curve paired with another token is not priced in SOL, so a market cap filter has no data", async () => {
  const usdcPaired = liveCurve(Buffer.alloc(32, 1));
  const result = await runGate(MINT, rule({ maxMarketCapUsd: 1_000_000 }), null, deps(network({ [MINT]: mint(T22), [curveKey]: usdcPaired })));
  assert.equal(result.pass === false && result.code, "missing_data");
  assert.equal(result.facts.marketCapUsd, undefined);
});

test("a required Rugcheck report without a score counts as unknown when a score limit is set", async () => {
  const net = network({ [MINT]: mint(SPL) }, 0, { score: null, dangers: [] });
  const result = await runGate(MINT, rule(), null, deps(net, 100, { rugcheck: "required", maxRugcheckScore: 50 }));
  assert.equal(result.pass === false && result.code, "check_unknown");
});
