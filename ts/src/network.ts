// Every read the bot makes from outside the process: Solana RPC, Jupiter quotes and prices, Jito
// tip levels, DexScreener pools, Rugcheck. LiveNetwork calls the real services; the demo
// (scripts/demo.ts) supplies a scripted one, so a dry run can work with no network at all.

export type AccountData = { owner: string; data: Uint8Array };

export type MarketSnapshot = {
  marketCapUsd?: number;
  liquidityUsd?: number;
  createdAtMs?: number; // earliest pool creation
  platform?: string; // venue of the deepest pool, as DexScreener names it (pumpfun, raydium, ...)
};

export type RugcheckSummary = { score: number | null; dangers: string[] };

export interface Network {
  // Several accounts in one round trip, at "processed": a mint created in the last slot is there.
  getAccounts(addresses: string[], signal?: AbortSignal): Promise<Array<AccountData | null>>;
  latestBlockhash(): Promise<{ blockhash: string; lastValidBlockHeight: number }>;
  recentPriorityFees(): Promise<number[]>; // micro-lamports per compute unit, recent slots
  jitoTipFloorSol(): Promise<number | null>;
  solUsdPrice(): Promise<number | null>;
  marketSnapshot(mint: string, signal?: AbortSignal): Promise<MarketSnapshot>;
  rugcheck(mint: string, signal?: AbortSignal): Promise<RugcheckSummary | null>;
  // What selling `tokens` of `mint` for SOL would return now, from a Jupiter quote. null: no route.
  quoteSellLamports(mint: string, tokens: bigint, signal?: AbortSignal): Promise<bigint | null>;
  quoteBuyTokens(mint: string, lamports: bigint, signal?: AbortSignal): Promise<bigint | null>;
  tokenBalance(owner: string, mint: string): Promise<bigint>;
}

export class RpcError extends Error {}

let nextId = 1;

// One JSON-RPC call. Node's fetch keeps the TLS connection alive between calls, so after the first
// request a call costs one round trip. land/land.ts uses this for sendTransaction too.
export async function rpcCall<T>(url: string, method: string, params: unknown[], signal?: AbortSignal): Promise<T> {
  const response = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: nextId++, method, params }),
    signal: signal ?? AbortSignal.timeout(10_000),
  });
  if (!response.ok) throw new RpcError(`${method}: HTTP ${response.status}`);
  const body = (await response.json()) as { result?: T; error?: { message?: string } };
  if (body.error) throw new RpcError(`${method}: ${body.error.message ?? "error"}`);
  return body.result as T;
}

export const WSOL = "So11111111111111111111111111111111111111112";
const JUPITER = "https://api.jup.ag";

export class LiveNetwork implements Network {
  private readonly rpcUrl: string;
  private readonly jupiterApiKey: string | null;

  constructor(rpcUrl: string, jupiterApiKey: string | null) {
    this.rpcUrl = rpcUrl;
    this.jupiterApiKey = jupiterApiKey;
  }

  async getAccounts(addresses: string[], signal?: AbortSignal): Promise<Array<AccountData | null>> {
    type Raw = { value: Array<{ owner: string; data: [string, string] } | null> };
    const result = await rpcCall<Raw>(this.rpcUrl, "getMultipleAccounts", [addresses, { encoding: "base64", commitment: "processed" }], signal);
    return result.value.map((a) => (a ? { owner: a.owner, data: Buffer.from(a.data[0], "base64") } : null));
  }

  async latestBlockhash(): Promise<{ blockhash: string; lastValidBlockHeight: number }> {
    type Raw = { value: { blockhash: string; lastValidBlockHeight: number } };
    return (await rpcCall<Raw>(this.rpcUrl, "getLatestBlockhash", [{ commitment: "confirmed" }])).value;
  }

  // No account filter: a filter only counts transactions that lock every listed account as
  // writable, and the accounts every pump.fun trade shares (program, global, fee config) are read-only.
  async recentPriorityFees(): Promise<number[]> {
    const result = await rpcCall<Array<{ prioritizationFee: number }>>(this.rpcUrl, "getRecentPrioritizationFees", []);
    return result.map((r) => r.prioritizationFee);
  }

  async tokenBalance(owner: string, mint: string): Promise<bigint> {
    type Raw = { value: Array<{ account: { data: { parsed: { info: { tokenAmount: { amount: string } } } } } }> };
    const result = await rpcCall<Raw>(this.rpcUrl, "getTokenAccountsByOwner", [owner, { mint }, { encoding: "jsonParsed", commitment: "confirmed" }]);
    return result.value.reduce((sum, a) => sum + BigInt(a.account.data.parsed.info.tokenAmount.amount), 0n);
  }

  // https://docs.jito.wtf/lowlatencytxnsend/ "Get Tip Information". Values are in SOL.
  async jitoTipFloorSol(): Promise<number | null> {
    const response = await fetch("https://bundles.jito.wtf/api/v1/bundles/tip_floor", { signal: AbortSignal.timeout(5_000) });
    if (!response.ok) return null;
    const rows = (await response.json()) as Array<{ landed_tips_75th_percentile?: number }>;
    return rows[0]?.landed_tips_75th_percentile ?? null;
  }

  // Jupiter Price API V3 (https://dev.jup.ag/docs/price/v3). Absent mints mean "no reliable price".
  async solUsdPrice(): Promise<number | null> {
    const response = await fetch(`${JUPITER}/price/v3?ids=${WSOL}`, { headers: this.jupiterHeaders(), signal: AbortSignal.timeout(5_000) });
    if (!response.ok) return null;
    const body = (await response.json()) as Record<string, { usdPrice?: number }>;
    return body[WSOL]?.usdPrice ?? null;
  }

  // DexScreener "Get the pools of a given token address" (https://docs.dexscreener.com/api/reference).
  async marketSnapshot(mint: string, signal?: AbortSignal): Promise<MarketSnapshot> {
    const response = await fetch(`https://api.dexscreener.com/token-pairs/v1/solana/${mint}`, { signal });
    if (!response.ok) return {};
    type Pair = { dexId?: string; liquidity?: { usd?: number }; marketCap?: number; pairCreatedAt?: number | null };
    const pairs = (await response.json()) as Pair[];
    if (!Array.isArray(pairs) || !pairs.length) return {};
    const deepest = [...pairs].sort((a, b) => (b.liquidity?.usd ?? 0) - (a.liquidity?.usd ?? 0))[0]!;
    const created = pairs.map((p) => p.pairCreatedAt).filter((t): t is number => typeof t === "number");
    return {
      marketCapUsd: deepest.marketCap,
      liquidityUsd: deepest.liquidity?.usd,
      createdAtMs: created.length ? Math.min(...created) : undefined,
      platform: deepest.dexId,
    };
  }

  // Rugcheck token report summary (https://api.rugcheck.xyz/swagger/index.html). No key needed.
  async rugcheck(mint: string, signal?: AbortSignal): Promise<RugcheckSummary | null> {
    const response = await fetch(`https://api.rugcheck.xyz/v1/tokens/${mint}/report/summary`, { signal });
    if (!response.ok) return null;
    type Raw = { score_normalised?: number; risks?: Array<{ name?: string; level?: string }> };
    const body = (await response.json()) as Raw;
    const dangers = (body.risks ?? []).filter((r) => r.level === "danger").map((r) => r.name ?? "unnamed risk");
    return { score: body.score_normalised ?? null, dangers };
  }

  // GET /swap/v2/order without a taker returns a quote and no transaction
  // (https://developers.jup.ag/docs/swap/order-and-execute).
  async quoteSellLamports(mint: string, tokens: bigint, signal?: AbortSignal): Promise<bigint | null> {
    return this.quote(mint, WSOL, tokens, signal);
  }

  async quoteBuyTokens(mint: string, lamports: bigint, signal?: AbortSignal): Promise<bigint | null> {
    return this.quote(WSOL, mint, lamports, signal);
  }

  private async quote(inputMint: string, outputMint: string, amount: bigint, signal?: AbortSignal): Promise<bigint | null> {
    const query = new URLSearchParams({ inputMint, outputMint, amount: amount.toString() });
    const response = await fetch(`${JUPITER}/swap/v2/order?${query}`, { headers: this.jupiterHeaders(), signal: signal ?? AbortSignal.timeout(5_000) });
    if (!response.ok) return null;
    const body = (await response.json()) as { outAmount?: string };
    return body.outAmount ? BigInt(body.outAmount) : null;
  }

  // The keyless tier allows 0.5 requests per second; a free key raises that.
  jupiterHeaders(): Record<string, string> {
    return this.jupiterApiKey ? { "x-api-key": this.jupiterApiKey } : {};
  }
}
