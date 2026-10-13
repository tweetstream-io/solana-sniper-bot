// Turns a fired rule into a signed transaction and, in live mode, hands it to land/land.ts.
// Holds the background caches (blockhash, fee levels, SOL price) so nothing on the buy path
// waits for them. Order of one buy: reserve -> filters/gate.ts -> build and sign -> land -> ledger.

import { ComputeBudgetProgram, Keypair, PublicKey, SystemProgram, TransactionInstruction, TransactionMessage, VersionedTransaction } from "@solana/web3.js";
import bs58 from "bs58";
import { createPrivateKey, sign as ed25519Sign, type KeyObject } from "node:crypto";
import type { Config, Rule } from "../setup/config.ts";
import type { Match } from "../rules/modes.ts";
import { solToLamports, type Ledger, type Position } from "../rules/ledger.ts";
import { runGate, type GateResult } from "../filters/gate.ts";
import { WSOL, type Network } from "../network.ts";
import type { Lander, Landing } from "../land/land.ts";
import type { LatencyReport, Trace } from "../report/latency.ts";
import { bondingCurveAddress, buyInstructions, lamportsForTokens, localBuildBlocker, parseCurve, sellInstruction, tokensForLamports, warmBuyAccounts, withSlippage } from "./pump.ts";
import { log, short } from "../log.ts";

// Jito tip accounts, as listed by getTipAccounts in https://docs.jito.wtf/lowlatencytxnsend/.
// Jito says they do not change, so they are not fetched at startup.
const JITO_TIP_ACCOUNTS = [
  "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
  "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
  "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
  "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
  "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
  "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
  "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
  "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
].map((a) => new PublicKey(a));

const pick = <T>(list: T[]): T => list[Math.floor(Math.random() * list.length)]!;

// Values refreshed in the background. A buy reads them from memory.
export class ChainCache {
  blockhash: { blockhash: string; lastValidBlockHeight: number; at: number } | null = null;
  priorityMicroLamports: number | null = null; // 75th percentile of recent fees, per compute unit
  tipFloorSol: number | null = null;
  solUsd: number | null = null;
  private timers: NodeJS.Timeout[] = [];

  private readonly network: Network;

  constructor(network: Network) {
    this.network = network;
  }

  start(blockhashRefreshMs: number): void {
    const every = (ms: number, task: () => Promise<void>) => {
      let lastError = ""; // log an outage once, not every 400 ms
      const run = () =>
        task().then(
          () => void (lastError = ""),
          (e: Error) => e.message !== lastError && log("cache", (lastError = e.message)),
        );
      run();
      this.timers.push(setInterval(run, ms));
    };
    every(blockhashRefreshMs, async () => {
      const value = await this.network.latestBlockhash();
      this.blockhash = { ...value, at: Date.now() };
    });
    every(2_000, async () => {
      const fees = (await this.network.recentPriorityFees()).filter((f) => f > 0).sort((a, b) => a - b);
      if (fees.length) this.priorityMicroLamports = fees[Math.floor(fees.length * 0.75)]!;
    });
    every(10_000, async () => {
      this.tipFloorSol = await this.network.jitoTipFloorSol();
    });
    every(30_000, async () => {
      this.solUsd = await this.network.solUsdPrice();
    });
  }

  stop(): void {
    for (const t of this.timers) clearInterval(t);
  }

  // A blockhash is valid for 150 blocks after the one it names.
  currentBlockHeight(): number | null {
    return this.blockhash ? this.blockhash.lastValidBlockHeight - 150 : null;
  }
}

export type TraderDeps = {
  config: Config;
  network: Network;
  ledger: Ledger;
  cache: ChainCache;
  report: LatencyReport;
  wallet: Keypair; // a throwaway key in dry run, so build and sign are still measured
  dryRun: boolean;
  lander: Lander | null; // null in dry run
  jitoEnabled: boolean;
  jupiterHeaders: Record<string, string>;
};

// PKCS#8 header for a raw Ed25519 seed, so node:crypto can sign with the wallet key.
const ED25519_PKCS8 = Buffer.from("302e020100300506032b657004220420", "hex");

export class Trader {
  private readonly d: TraderDeps;
  private readonly signingKey: KeyObject;
  private readonly selling = new Set<string>();

  constructor(d: TraderDeps) {
    this.d = d;
    this.signingKey = createPrivateKey({ key: Buffer.concat([ED25519_PKCS8, d.wallet.secretKey.subarray(0, 32)]), format: "der", type: "pkcs8" });
  }

  // Node's native Ed25519 signs in about 75 us; the pure-JS signer in web3.js takes about 400.
  private sign(tx: VersionedTransaction): void {
    tx.addSignature(this.d.wallet.publicKey, ed25519Sign(null, Buffer.from(tx.message.serialize()), this.signingKey));
  }

  async buy(match: Match, trace: Trace): Promise<void> {
    const { rule, mint } = match;
    const { ledger, config } = this.d;
    const now = Date.now();
    const blocked = ledger.tryReserveMint(mint, rule.filters.skipIfAlreadyBought, trace.connection, now);
    if (blocked) return this.drop(rule, mint, blocked, trace);
    const lamports = solToLamports(rule.token.amountSol);
    const capped = ledger.trySpend(lamports, config.spend, now);
    if (capped) {
      ledger.finishMint(mint, "rejected");
      return this.drop(rule, mint, capped, trace);
    }
    const undo = (reason: string) => {
      ledger.refundSpend(lamports, now);
      ledger.finishMint(mint, "rejected");
      this.drop(rule, mint, reason, trace);
    };

    const pending = runGate(mint, rule, match.signal, {
      network: this.d.network,
      safety: config.safety,
      pumpBuilder: config.execution.pumpBuilder,
      solUsd: () => this.d.cache.solUsd,
      holding: (m) => ledger.position(m) !== undefined,
    });
    // setImmediate lets the read go out on the socket first, then uses the wait to derive addresses.
    setImmediate(() => warmBuyAccounts(mint, this.d.wallet.publicKey));
    const gate = await pending;
    trace.gateDoneNs = process.hrtime.bigint();
    trace.gateMs = gate.elapsedMs;
    if (!gate.pass) return undo(`${gate.code}: ${gate.detail}`);

    let result: Landing | null | "dry_run";
    try {
      result = await this.executeBuy(rule, mint, lamports, gate, trace);
    } catch (error) {
      // Nothing was sent: the build failed (an API refused, no blockhash yet, no route).
      return undo(`build failed: ${(error as Error).message}`);
    }
    if (result === "dry_run") {
      ledger.finishMint(mint, "dry_run");
      const tokens = gate.route === "jupiter" ? await this.d.network.quoteBuyTokens(mint, lamports).catch(() => null) : tokensForLamports(gate.curve!, lamports);
      this.open(match, gate, lamports, tokens, true);
      this.d.report.finish(trace, "dry run, not sent");
      return;
    }
    if (result === null) return undo("build produced no transaction");
    this.record(trace, result);
    if (result.status === "confirmed") {
      ledger.finishMint(mint, "bought");
      const tokens = await this.d.network.tokenBalance(this.d.wallet.publicKey.toBase58(), mint).catch(() => null);
      this.open(match, gate, lamports, tokens, false);
      this.d.report.finish(trace, `bought, ${result.signature}`);
    } else if (result.status === "failed") {
      // The transaction landed and failed, so the SOL stayed in the wallet.
      ledger.refundSpend(lamports, now);
      ledger.finishMint(mint, "failed");
      ledger.releaseSnipe(rule.name);
      this.d.report.finish(trace, `failed on chain: ${result.error}`);
      if (gate.route === "pump" && rule.fees.retriesOnGraduation > 0) await this.retryAfterGraduation(match, trace);
    } else {
      ledger.finishMint(mint, "unknown"); // may still land: never buy this mint again automatically
      this.d.report.finish(trace, `outcome unknown: ${result.error}`);
    }
  }

  // A curve buy that failed because the coin graduated mid-flight is retried through Jupiter.
  private async retryAfterGraduation(match: Match, trace: Trace): Promise<void> {
    const [account] = await this.d.network.getAccounts([bondingCurveAddress(match.mint).toBase58()]);
    const curve = account ? parseCurve(account.data) : null;
    if (!curve?.complete) return;
    log("retry", `${short(match.mint)} graduated during the buy; retrying through Jupiter`);
    const rule = { ...match.rule, fees: { ...match.rule.fees, retriesOnGraduation: match.rule.fees.retriesOnGraduation - 1 } };
    await this.buy({ ...match, rule }, { ...trace });
  }

  private async executeBuy(rule: Rule, mint: string, lamports: bigint, gate: Extract<GateResult, { pass: true }>, trace: Trace): Promise<Landing | null | "dry_run"> {
    const { wallet, dryRun } = this.d;
    if (gate.route === "jupiter") {
      if (dryRun) {
        log("dry run", `${rule.name}: would buy ${short(mint)} for ${rule.token.amountSol} SOL through Jupiter /order and /execute${describeFacts(gate)}`);
        return "dry_run";
      }
      return this.jupiterSwap(WSOL, mint, lamports, trace);
    }
    let tx: VersionedTransaction;
    if (gate.route === "pump") {
      const tokens = tokensForLamports(gate.curve!, lamports);
      const minOut = withSlippage(tokens, rule.fees.slippagePct);
      tx = this.compile(rule, buyInstructions(wallet.publicKey, new PublicKey(mint), new PublicKey(gate.tokenProgram), lamports, minOut));
    } else {
      tx = await this.pumpPortal({ action: "buy", mint, amount: rule.token.amountSol, denominatedInSol: "true", slippage: rule.fees.slippagePct, priorityFee: this.priorityFeeSol(rule) });
    }
    trace.signedNs = process.hrtime.bigint();
    // Logged after signing, so console output never sits between the decision and the send.
    const what = `${gate.route} buy of ${short(mint)} for ${rule.token.amountSol} SOL${describeFacts(gate)}`;
    if (dryRun) {
      log("dry run", `${rule.name}: would send a signed ${tx.serialize().length}-byte ${what}`);
      return "dry_run";
    }
    const landing = this.d.lander!.land(tx, this.d.cache.blockhash!.lastValidBlockHeight);
    log("send", `${rule.name}: ${what}, signature ${bs58.encode(tx.signatures[0]!)}`);
    return landing;
  }

  async sell(mint: string, reason: string): Promise<void> {
    const position = this.d.ledger.position(mint);
    // A delete and a stop loss can ask for the same exit at once; only one sell may run.
    if (!position || this.selling.has(mint)) return;
    this.selling.add(mint);
    try {
      await this.sellPosition(position, mint, reason);
    } finally {
      this.selling.delete(mint);
    }
  }

  private async sellPosition(position: Position, mint: string, reason: string): Promise<void> {
    const { config, network, wallet, dryRun } = this.d;
    if (dryRun || position.dryRun) {
      log("dry run", `would sell ${short(mint)} (${reason})`);
      this.d.ledger.closePosition(mint);
      return;
    }
    const tokens = position.tokenAmount !== null ? BigInt(position.tokenAmount) : await network.tokenBalance(wallet.publicKey.toBase58(), mint);
    const [account] = await network.getAccounts([bondingCurveAddress(mint).toBase58()]);
    const curve = account ? parseCurve(account.data) : null;
    const trace: Trace = { mint, rule: `exit:${reason}`, connection: "-", publishedAtWall: null, receivedWall: Date.now(), receivedNs: process.hrtime.bigint() };
    trace.decidedNs = trace.receivedNs;
    let result: Landing;
    if (curve && !localBuildBlocker(curve) && config.execution.pumpBuilder === "local") {
      const minOut = withSlippage(lamportsForTokens(curve, tokens), config.exits.sellSlippagePct);
      const rule = config.rules.find((r) => r.name === position.rule) ?? config.rules[0]!;
      const tx = this.compile(rule, [sellInstruction(wallet.publicKey, new PublicKey(mint), new PublicKey(position.tokenProgram), tokens, minOut)]);
      trace.signedNs = process.hrtime.bigint();
      result = await this.d.lander!.land(tx, this.d.cache.blockhash!.lastValidBlockHeight);
    } else if (curve && !curve.complete) {
      const tx = await this.pumpPortal({ action: "sell", mint, amount: "100%", denominatedInSol: "false", slippage: config.exits.sellSlippagePct, priorityFee: 0.0005 });
      result = await this.d.lander!.land(tx, this.d.cache.blockhash!.lastValidBlockHeight);
    } else {
      result = await this.jupiterSwap(mint, WSOL, tokens, trace);
    }
    this.record(trace, result);
    if (result.status === "confirmed") this.d.ledger.closePosition(mint);
    this.d.report.finish(trace, `sell ${result.status}${result.error ? `: ${result.error}` : ""}`);
  }

  // Compute budget first, then the trade, then tips. One transaction carries every tip, so the
  // same signed bytes can go to every sender.
  private compile(rule: Rule, trade: TransactionInstruction[]): VersionedTransaction {
    const { wallet, cache, config } = this.d;
    if (!cache.blockhash) throw new Error("no blockhash cached yet");
    const units = config.execution.computeUnitLimit;
    const microLamports = Math.floor((this.priorityFeeSol(rule) * 1e9 * 1e6) / units);
    const instructions = [
      ComputeBudgetProgram.setComputeUnitLimit({ units }),
      ComputeBudgetProgram.setComputeUnitPrice({ microLamports }),
      ...trade,
    ];
    const tip = this.jitoTipLamports(rule);
    if (tip > 0) instructions.push(SystemProgram.transfer({ fromPubkey: wallet.publicKey, toPubkey: pick(JITO_TIP_ACCOUNTS), lamports: tip }));
    for (const sender of config.senders) {
      if (sender.tipAccounts.length && sender.tipSol > 0) {
        const toPubkey = new PublicKey(pick(sender.tipAccounts));
        instructions.push(SystemProgram.transfer({ fromPubkey: wallet.publicKey, toPubkey, lamports: Math.round(sender.tipSol * 1e9) }));
      }
    }
    const message = new TransactionMessage({ payerKey: wallet.publicKey, recentBlockhash: cache.blockhash.blockhash, instructions }).compileToV0Message();
    const tx = new VersionedTransaction(message);
    this.sign(tx);
    return tx;
  }

  private priorityFeeSol(rule: Rule): number {
    const fee = rule.fees.priorityFeeSol;
    if (fee !== "auto") return fee;
    const { priorityMicroLamports } = this.d.cache;
    const estimate = priorityMicroLamports === null ? 0 : (priorityMicroLamports * this.d.config.execution.computeUnitLimit) / 1e15;
    return Math.min(estimate, this.d.config.execution.maxAutoPriorityFeeSol);
  }

  private jitoTipLamports(rule: Rule): number {
    if (!rule.fees.antiMev || !this.d.jitoEnabled) return 0;
    const tip = rule.fees.tipSol === "auto" ? Math.min(this.d.cache.tipFloorSol ?? 0, this.d.config.execution.maxAutoTipSol) : rule.fees.tipSol;
    return Math.max(Math.round(tip * 1e9), 1_000); // Jito ignores tips under 1000 lamports
  }

  // PumpPortal local trading API (https://pumpportal.fun/local-trading-api/trading-api/): it
  // returns an unsigned transaction that we sign and send ourselves. One extra HTTPS round trip.
  private async pumpPortal(body: Record<string, string | number>): Promise<VersionedTransaction> {
    const response = await fetch("https://pumpportal.fun/api/trade-local", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ publicKey: this.d.wallet.publicKey.toBase58(), pool: "auto", ...body }),
      signal: AbortSignal.timeout(5_000),
    });
    if (response.status !== 200) throw new Error(`PumpPortal HTTP ${response.status}`);
    const tx = VersionedTransaction.deserialize(new Uint8Array(await response.arrayBuffer()));
    this.sign(tx);
    return tx;
  }

  // Jupiter Swap V2 order + execute (https://developers.jup.ag/docs/swap/order-and-execute).
  // Jupiter lands the transaction itself, so our senders are not used on this route.
  private async jupiterSwap(inputMint: string, outputMint: string, amount: bigint, trace: Trace): Promise<Landing> {
    const headers = this.d.jupiterHeaders;
    const query = new URLSearchParams({ inputMint, outputMint, amount: amount.toString(), taker: this.d.wallet.publicKey.toBase58() });
    const orderResponse = await fetch(`https://api.jup.ag/swap/v2/order?${query}`, { headers, signal: AbortSignal.timeout(5_000) });
    if (!orderResponse.ok) throw new Error(`Jupiter /order HTTP ${orderResponse.status}`);
    const order = (await orderResponse.json()) as { transaction: string | null; requestId: string; errorMessage?: string };
    if (!order.transaction) throw new Error(`Jupiter /order returned no transaction: ${order.errorMessage ?? "no route"}`);
    const tx = VersionedTransaction.deserialize(Buffer.from(order.transaction, "base64"));
    this.sign(tx); // JupiterZ quotes add the market maker's signature during /execute
    trace.signedNs = process.hrtime.bigint();
    const sentNs = process.hrtime.bigint();
    const executeResponse = await fetch("https://api.jup.ag/swap/v2/execute", {
      method: "POST",
      headers: { ...headers, "Content-Type": "application/json" },
      body: JSON.stringify({ signedTransaction: Buffer.from(tx.serialize()).toString("base64"), requestId: order.requestId }),
      signal: AbortSignal.timeout(this.d.config.execution.confirmTimeoutMs),
    });
    const signature = bs58.encode(tx.signatures[0]!);
    if (!executeResponse.ok) return { signature, status: "unknown", error: `Jupiter /execute HTTP ${executeResponse.status}`, sentNs };
    const result = (await executeResponse.json()) as { status: string; code: number; error?: string };
    return {
      signature,
      status: result.status === "Success" ? "confirmed" : result.code === -1000 || result.code === -2000 ? "unknown" : "failed",
      error: result.error ?? (result.code ? `code ${result.code}` : undefined),
      sentNs,
      firstAckNs: process.hrtime.bigint(),
      firstAckBy: "jupiter",
      confirmedNs: process.hrtime.bigint(),
    };
  }

  private open(match: Match, gate: Extract<GateResult, { pass: true }>, lamports: bigint, tokens: bigint | null, dryRun: boolean): void {
    const position: Position = {
      mint: match.mint,
      route: gate.route === "jupiter" ? "jupiter" : "pump",
      tokenProgram: gate.tokenProgram,
      tokenAmount: tokens?.toString() ?? null,
      costLamports: lamports.toString(),
      openedAt: Date.now(),
      postKey: match.signal?.postKey,
      rule: match.rule.name,
      dryRun,
    };
    this.d.ledger.openPosition(position);
  }

  private record(trace: Trace, landing: Landing): void {
    trace.sentNs = landing.sentNs;
    trace.firstAckNs = landing.firstAckNs;
    trace.firstAckBy = landing.firstAckBy;
    trace.confirmedNs = landing.confirmedNs;
  }

  private drop(rule: Rule, mint: string, reason: string, trace: Trace): void {
    this.d.ledger.releaseSnipe(rule.name);
    log("skip", `${rule.name} ${short(mint)}: ${reason}`);
    this.d.report.finish(trace, `skipped (${reason.split(":")[0]})`);
  }
}

function describeFacts(gate: GateResult): string {
  const f = gate.facts;
  const parts = [
    `checks ${gate.elapsedMs.toFixed(1)} ms`,
    f.marketCapUsd !== undefined ? `mcap $${Math.round(f.marketCapUsd)}` : "",
    f.liquidityUsd !== undefined ? `liquidity $${Math.round(f.liquidityUsd)}` : "",
    f.taxPct ? `tax ${f.taxPct}%` : "",
  ].filter(Boolean);
  return ` (${parts.join(", ")})`;
}
