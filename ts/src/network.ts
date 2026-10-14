// Every call the bot makes to a service except sending a signed transaction: Solana RPC reads,
// Jupiter quotes, orders and executes, Jito tip levels, DexScreener, Rugcheck.
// LiveNetwork calls the real services; scripts/scenario-network.ts answers from a scenario file,
// so the demo, the benchmark, and the trade tests run with no network. land/land.ts sends.

export type AccountData = { owner: string; data: Uint8Array };

export type MarketSnapshot = {
  marketCapUsd?: number;
  liquidityUsd?: number;
  createdAtMs?: number; // earliest pool creation
  platform?: string; // venue of the deepest pool, as DexScreener names it (pumpfun, raydium, ...)
};

export type RugcheckSummary = { score: number | null; dangers: string[] };

// GET /swap/v2/order. transaction is null or "" when Jupiter cannot build one (see errorMessage).
export type JupiterOrder = { transaction: string | null; requestId: string; lastValidBlockHeight?: number; errorMessage?: string };
// POST /swap/v2/execute. code 0 is success; https://developers.jup.ag/docs/swap/order-and-execute.
export type JupiterExecute = { status: string; code: number; signature?: string; error?: string; totalOutputAmount?: string; totalInputAmount?: string };

export interface Network {
  // Several accounts in one round trip, at "processed": a mint created in the last slot is there.
  getAccounts(addresses: string[], signal?: AbortSignal): Promise<Array<AccountData | null>>;
  latestBlockhash(): Promise<{ blockhash: string; lastValidBlockHeight: number }>;
  epochInfo(): Promise<{ epoch: number; blockHeight: number }>;
  // Status of a sent transaction: null when the RPC has not seen it.
  signatureStatus(signature: string): Promise<{ confirmed: boolean; err: unknown } | null>;
  // False once a transaction built on this blockhash can no longer land.
  isBlockhashValid(blockhash: string): Promise<boolean>;
  balance(owner: string): Promise<bigint>; // lamports
  // The wallet's lamports after the signed transaction, simulated without landing it.
  simulate(signedTx: string, wallet: string): Promise<{ err: unknown; walletLamportsAfter: bigint | null }>;
  // How many lamports the transaction took from `owner` (fees, tips, rent, principal). null: not found.
  transactionDebit(signature: string, owner: string): Promise<bigint | null>;
  jupiterOrder(q: { inputMint: string; outputMint: string; amount: bigint; taker: string }): Promise<JupiterOrder>;
  jupiterExecute(signedTx: string, requestId: string, timeoutMs: number): Promise<JupiterExecute>;
  recentPriorityFees(): Promise<number[]>; // micro-lamports per compute unit, recent slots
  jitoTipFloorSol(): Promise<number | null>;
  solUsdPrice(): Promise<number | null>;
  marketSnapshot(mint: string, signal?: AbortSignal): Promise<MarketSnapshot>;
  rugcheck(mint: string, signal?: AbortSignal): Promise<RugcheckSummary | null>;
  // What selling `tokens` of `mint` for SOL would return now, from a Jupiter quote. null: no route.
  quoteSellLamports(mint: string, tokens: bigint, signal?: AbortSignal): Promise<bigint | null>;
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
  async epochInfo(): Promise<{ epoch: number; blockHeight: number }> {
    return rpcCall(this.rpcUrl, "getEpochInfo", [{ commitment: "confirmed" }]);
  }

  async signatureStatus(signature: string): Promise<{ confirmed: boolean; err: unknown } | null> {
    type Raw = { value: Array<{ confirmationStatus?: string; err: unknown } | null> };
    const value = (await rpcCall<Raw>(this.rpcUrl, "getSignatureStatuses", [[signature], { searchTransactionHistory: true }])).value[0];
    if (!value) return null;
    return { confirmed: value.confirmationStatus === "confirmed" || value.confirmationStatus === "finalized", err: value.err };
  }

  async transactionDebit(signature: string, owner: string): Promise<bigint | null> {
    type Raw = { meta: { preBalances: number[]; postBalances: number[] }; transaction: { message: { accountKeys: Array<string | { pubkey: string }> } } } | null;
    const tx = await rpcCall<Raw>(this.rpcUrl, "getTransaction", [signature, { commitment: "confirmed", maxSupportedTransactionVersion: 0, encoding: "jsonParsed" }]);
    if (!tx) return null;
    const i = tx.transaction.message.accountKeys.findIndex((k) => (typeof k === "string" ? k : k.pubkey) === owner);
    return i < 0 ? null : BigInt(tx.meta.preBalances[i]! - tx.meta.postBalances[i]!);
  }

  // https://solana.com/docs/rpc/http/isblockhashvalid
  async isBlockhashValid(blockhash: string): Promise<boolean> {
    return (await rpcCall<{ value: boolean }>(this.rpcUrl, "isBlockhashValid", [blockhash, { commitment: "processed" }])).value;
  }

  async balance(owner: string): Promise<bigint> {
    return BigInt((await rpcCall<{ value: number }>(this.rpcUrl, "getBalance", [owner, { commitment: "processed" }])).value);
  }

  // https://solana.com/docs/rpc/http/simulatetransaction, asking for the wallet account afterwards.
  async simulate(signedTx: string, wallet: string): Promise<{ err: unknown; walletLamportsAfter: bigint | null }> {
    type Raw = { value: { err: unknown; accounts: Array<{ lamports: number } | null> | null } };
    const params = { encoding: "base64", sigVerify: false, commitment: "processed", accounts: { encoding: "base64", addresses: [wallet] } };
    const { value } = await rpcCall<Raw>(this.rpcUrl, "simulateTransaction", [signedTx, params]);
    const after = value.accounts?.[0];
    return { err: value.err, walletLamportsAfter: after ? BigInt(after.lamports) : null };
  }

  async jupiterOrder(q: { inputMint: string; outputMint: string; amount: bigint; taker: string }): Promise<JupiterOrder> {
    const query = new URLSearchParams({ inputMint: q.inputMint, outputMint: q.outputMint, amount: q.amount.toString(), taker: q.taker });
    const response = await fetch(`${JUPITER}/swap/v2/order?${query}`, { headers: this.jupiterHeaders(), signal: AbortSignal.timeout(5_000) });
    if (!response.ok) throw new Error(`Jupiter /order HTTP ${response.status}`);
    return (await response.json()) as JupiterOrder;
  }

  // Throws when the answer is missing or unreadable; the caller treats that as an unknown outcome.
  async jupiterExecute(signedTx: string, requestId: string, timeoutMs: number): Promise<JupiterExecute> {
    const response = await fetch(`${JUPITER}/swap/v2/execute`, {
      method: "POST",
      headers: { ...this.jupiterHeaders(), "Content-Type": "application/json" },
      body: JSON.stringify({ signedTransaction: signedTx, requestId }),
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (!response.ok) throw new Error(`Jupiter /execute HTTP ${response.status}`);
    return (await response.json()) as JupiterExecute;
  }

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

  private async quote(inputMint: string, outputMint: string, amount: bigint, signal?: AbortSignal): Promise<bigint | null> {
    const query = new URLSearchParams({ inputMint, outputMint, amount: amount.toString() });
    const response = await fetch(`${JUPITER}/swap/v2/order?${query}`, { headers: this.jupiterHeaders(), signal: signal ?? AbortSignal.timeout(5_000) });
    if (!response.ok) return null;
    const body = (await response.json()) as { outAmount?: string };
    return body.outAmount ? BigInt(body.outAmount) : null;
  }

  // The keyless tier allows 0.5 requests per second; a free key raises that.
  private jupiterHeaders(): Record<string, string> {
    return this.jupiterApiKey ? { "x-api-key": this.jupiterApiKey } : {};
  }
}
