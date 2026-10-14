// Runs fired rules as trades. One buy, top to bottom: filters/gate.ts -> build and sign a pump.fun
// curve buy -> write the pending trade (execute/trades.ts) -> send (land/land.ts) -> resolve in one
// write. Sells use the same steps, through Jupiter when the curve has graduated; the reconciler
// finishes trades whose outcome was not known.

import { ComputeBudgetProgram, Keypair, PublicKey, SystemProgram, TransactionInstruction, TransactionMessage, VersionedTransaction } from "@solana/web3.js";
import bs58 from "bs58";
import { readFileSync } from "node:fs";
import { createPrivateKey, sign as ed25519Sign, type KeyObject } from "node:crypto";
import type { Config, Rule } from "../setup/config.ts";
import type { Fire } from "../rules/modes.ts";
import { solToLamports, type Outcome, type Pending, type TradeBook, type Trigger } from "./trades.ts";
import { runGate, type GateResult } from "../filters/gate.ts";
import { WSOL, type Network } from "../network.ts";
import type { Lander, SenderTarget } from "../land/land.ts";
import type { LatencyReport, Trace } from "../report/latency.ts";
import type { ChainCache } from "./cache.ts";
import { bondingCurveAddress, buyInstructions, curveFeeBps, lamportsForTokens, parseCurve, sellInstruction, tokensForLamports, unsupportedCurve, warmBuyAccounts, withSlippage, type Curve } from "./pump.ts";
import { log, short } from "../log.ts";

// Jito tip accounts and the Jupiter /execute code map for sells, shared with the Rust bot.
const TRADING = JSON.parse(readFileSync(new URL("../../../shared/trading.json", import.meta.url), "utf8")) as {
  jitoTipAccounts: string[];
  jupiterExecuteCodes: { landed: number[]; rejected: number[] };
};
const JITO_TIP_ACCOUNTS = TRADING.jitoTipAccounts.map((a) => new PublicKey(a));

// What a buy can cost beyond its amount (shared/README.md, section Money bound). Rent for an
// account is (bytes + 128) x 6,960 lamports: a Token-2022 token account with ImmutableOwner is
// 170 bytes, pump.fun's per-user volume accumulator 137 bytes, a full bonding curve 115 bytes.
const SIGNATURE_FEE = 5_000n;
const TOKEN_ACCOUNT_RENT = 2_074_080n;
const VOLUME_ACCUMULATOR_RENT = 1_844_400n;
const CURVE_LENGTH = 115;
const RENT_PER_BYTE = 6_960n;
const JITO_MIN_TIP = 1_000n; // Jito ignores smaller tips
const ED25519_PKCS8 = Buffer.from("302e020100300506032b657004220420", "hex"); // header for a raw Ed25519 seed

function jupiterOutcome(code: number): "landed" | "rejected" | "unknown" {
  if (TRADING.jupiterExecuteCodes.landed.includes(code)) return "landed";
  return TRADING.jupiterExecuteCodes.rejected.includes(code) ? "rejected" : "unknown";
}

// The largest debit of a locally built pump.fun buy.
export function pumpBuyReservation(amount: bigint, priorityLamports: bigint, jitoTip: bigint, curveLength: number): bigint {
  const topUp = BigInt(Math.max(0, CURVE_LENGTH - curveLength)) * RENT_PER_BYTE;
  return amount + SIGNATURE_FEE + priorityLamports + jitoTip + TOKEN_ACCOUNT_RENT + VOLUME_ACCUMULATOR_RENT + topUp;
}

// What one transaction pays besides its amount. jitoTip is 0 unless it goes to Jito.
type FeePlan = { priorityLamports: bigint; jitoTip: bigint };

// A signed transaction ready to record and send. Jupiter trades carry the order's requestId.
type Built = { tx: VersionedTransaction; route: "pump" | "jupiter"; lastValidBlockHeight: number | null; targets: SenderTarget[]; requestId?: string };

export type TraderDeps = {
  config: Config;
  network: Network;
  book: TradeBook;
  cache: ChainCache;
  report: LatencyReport;
  wallet: Keypair; // a throwaway key in dry run, so build and sign are still measured
  dryRun: boolean;
  lander: Lander | null; // null in dry run
  senders: SenderTarget[]; // the RPC URLs and, when configured, Jito
  releaseFire: (fire: Fire) => void; // give a fire back when nothing was sent
};

export class Trader {
  private readonly d: TraderDeps;
  private readonly signingKey: KeyObject;
  private readonly sending = new Set<string>(); // trade ids this process is still sending
  private readonly reviewLogged = new Set<string>();
  private reconciling = false;
  private timer: NodeJS.Timeout | null = null;

  constructor(d: TraderDeps) {
    this.d = d;
    this.signingKey = createPrivateKey({ key: Buffer.concat([ED25519_PKCS8, d.wallet.secretKey.subarray(0, 32)]), format: "der", type: "pkcs8" });
  }

  // ---- buy ----

  async buy(fire: Fire, trace: Trace): Promise<void> {
    const { rule, mint } = fire;
    const { book } = this.d;
    const busy = book.begin(mint, "buy");
    if (busy) return this.drop(fire, busy, trace);
    try {
      const pending = runGate(mint, rule, fire.signal, {
        network: this.d.network,
        safety: this.d.config.safety,
        solUsd: () => this.d.cache.solUsd,
        epoch: () => this.d.cache.epoch(),
      });
      // setImmediate lets the read go out on the socket first, then uses the wait to derive addresses.
      setImmediate(() => warmBuyAccounts(mint, this.d.wallet.publicKey));
      const gate = await pending;
      trace.gateDoneNs = process.hrtime.bigint();
      trace.gateMs = gate.elapsedMs;
      if (!gate.pass) return this.drop(fire, `${gate.code}: ${gate.detail}`, trace);
      const error = await this.buyPassed(fire, gate, trace);
      if (error) this.drop(fire, error, trace);
    } finally {
      book.end(mint);
    }
  }

  // Returns why nothing was sent, or null when a trade was recorded (sent, or a dry run).
  private async buyPassed(fire: Fire, gate: Extract<GateResult, { pass: true }>, trace: Trace): Promise<string | null> {
    const { rule, mint } = fire;
    const { book, config, dryRun } = this.d;
    const amount = solToLamports(rule.token.amountSol);
    const plan = this.feePlan(rule);
    const reservation = pumpBuyReservation(amount, plan.priorityLamports, plan.jitoTip, gate.curve.dataLength);
    const trigger: Trigger = { eventRef: trace.eventRef, ...(fire.signal?.postKey ? { postKey: fire.signal.postKey } : {}) };
    const id = book.newTradeId(Date.now());
    const entry = { mint, reservedLamports: reservation.toString(), amountLamports: amount.toString(), rule: rule.name, trigger, tokenProgram: gate.tokenProgram };
    const what = `pump buy of ${short(mint)} for ${rule.token.amountSol} SOL${describeFacts(gate)}`;
    let built: Built;
    try {
      built = this.buildPumpBuy(rule, mint, amount, gate.curve, gate.tokenProgram, plan);
    } catch (error) {
      return `not sent: ${(error as Error).message}`;
    }
    trace.signedNs = process.hrtime.bigint();

    if (dryRun) {
      log("dry run", `${rule.name}: would send a signed ${built.tx.serialize().length}-byte ${what}`);
      const tokens = tokensForLamports(gate.curve, amount, curveFeeBps(this.feeTiers(), gate.curve));
      const refused = book.recordDryRunBuy(id, entry, tokens, config.spend, Date.now());
      if (refused) return refused;
      this.d.report.finish(trace, "dry run, not sent");
      return null;
    }

    const outcome = await this.send(id, { ...entry, side: "buy", route: "pump" }, built, trace);
    if (typeof outcome === "string") return outcome;
    log("send", `${rule.name}: ${what} -> ${outcome.kind}`);
    book.resolve(id, outcome, Date.now());
    this.d.report.finish(trace, `buy ${outcome.kind}${"error" in outcome ? `: ${outcome.error}` : ""}`);
    return null;
  }

  // ---- sell ----

  // Sells the whole position. Exits stay on the position until a sell confirms, so a sell that
  // fails or is not sent is tried again by the reconciler.
  async sell(mint: string, reason: string): Promise<void> {
    const { book, dryRun, network, config, wallet } = this.d;
    const busy = book.begin(mint, "sell");
    if (busy) return log("exit", `${short(mint)}: not selling now (${busy})`);
    try {
      const position = book.position(mint)!;
      if (dryRun) {
        log("dry run", `would sell ${short(mint)} (${reason})`);
        return book.recordDryRunSell(mint);
      }
      const trace: Trace = { mint, rule: `exit:${reason}`, connection: "-", eventRef: reason, publishedAtWall: null, receivedWall: Date.now(), receivedNs: process.hrtime.bigint() };
      trace.decidedNs = trace.receivedNs;
      let built: Built;
      let reservation: bigint;
      try {
        const tokens = BigInt(position.tokenAmount ?? "0");
        if (tokens <= 0n) throw new Error("token balance not known yet");
        const rule = config.rules.find((r) => r.name === position.rule) ?? config.rules[0]!;
        const [account] = await network.getAccounts([bondingCurveAddress(mint).toBase58()]);
        const curve = account ? parseCurve(account.data) : null;
        if (curve && !curve.complete && !unsupportedCurve(curve)) {
          const plan = this.feePlan(rule);
          const minOut = withSlippage(lamportsForTokens(curve, tokens, curveFeeBps(this.feeTiers(), curve)), config.exits.sellSlippagePct);
          built = this.compileLocal(rule, plan, [sellInstruction(wallet.publicKey, new PublicKey(mint), new PublicKey(position.tokenProgram), tokens, minOut)]);
          reservation = SIGNATURE_FEE + plan.priorityLamports + plan.jitoTip;
        } else {
          // The curve graduated: Jupiter routes the sell and sets its own fee.
          built = await this.buildJupiter(mint, WSOL, tokens);
          reservation = SIGNATURE_FEE + solToLamports(config.execution.maxAutoPriorityFeeSol);
        }
      } catch (error) {
        return log("exit", `${short(mint)}: sell not sent: ${(error as Error).message}`);
      }
      trace.signedNs = process.hrtime.bigint();
      const id = book.newTradeId(Date.now());
      const entry = { side: "sell" as const, mint, route: built.route, reservedLamports: reservation.toString(), amountLamports: "0", rule: position.rule, trigger: position.trigger, tokenProgram: position.tokenProgram };
      const outcome = await this.send(id, entry, built, trace);
      if (typeof outcome === "string") return log("exit", `${short(mint)}: sell ${outcome}`);
      log("send", `sell of ${short(mint)} (${reason}) -> ${outcome.kind}`);
      book.resolve(id, outcome, Date.now());
      this.d.report.finish(trace, `sell ${outcome.kind}${"error" in outcome ? `: ${outcome.error}` : ""}`);
    } finally {
      book.end(mint);
    }
  }

  // Kept on the position until a sell confirms. On a pending buy it waits there, and the reconciler
  // sells once the buy confirms.
  async exit(mint: string, code: string): Promise<void> {
    const where = this.d.book.addExit(mint, { code, at: Date.now() });
    if (where === "pending") log("exit", `${short(mint)}: ${code} recorded on the pending buy; it sells once the buy confirms`);
    if (where === "position") await this.sellIfExiting(mint);
  }

  // Sells a position that has exits waiting, reading its balance first when it is not known.
  private async sellIfExiting(mint: string): Promise<void> {
    const { book, network, wallet, dryRun } = this.d;
    const position = book.position(mint);
    if (!position?.pendingExits.length) return;
    if (position.tokenAmount === null && !dryRun) {
      book.setTokenAmount(mint, await network.tokenBalance(wallet.publicKey.toBase58(), mint).catch(() => 0n));
      if (book.position(mint)?.tokenAmount === null) return log("exit", `${short(mint)}: waiting for the token balance before selling`);
    }
    await this.sell(mint, position.pendingExits.map((e) => e.code).join(", "));
  }

  // ---- sending ----

  // Writes the pending trade (signature, blockhash, validity, the spend reservation for a buy)
  // before anything goes out. Returns why nothing was sent when the spend caps refuse it or the
  // write fails.
  private async send(id: string, entry: Omit<Pending, "signature" | "blockhash" | "lastValidBlockHeight" | "sentAt" | "pendingExits">, built: Built, trace: Trace): Promise<Outcome | string> {
    const signature = ownSignature(built.tx);
    const now = Date.now();
    const pending: Pending = { ...entry, signature, blockhash: built.tx.message.recentBlockhash, lastValidBlockHeight: built.lastValidBlockHeight, sentAt: now, pendingExits: [] };
    let refused: string | null;
    try {
      refused = this.d.book.recordPending(id, pending, this.d.config.spend, now);
    } catch (error) {
      log("error", `${short(entry.mint)}: state write failed, transaction not sent: ${(error as Error).message}`);
      return `not sent: state write failed`;
    }
    if (refused) return refused;
    this.sending.add(id);
    try {
      return built.requestId !== undefined ? await this.jupiterExecute(built, signature, trace) : await this.landLocal(built, trace);
    } finally {
      this.sending.delete(id);
    }
  }

  private async landLocal(built: Built, trace: Trace): Promise<Outcome> {
    const landing = await this.d.lander!.land(built.tx, built.lastValidBlockHeight!, built.targets);
    Object.assign(trace, { sentNs: landing.sentNs, firstAckNs: landing.firstAckNs, firstAckBy: landing.firstAckBy, confirmedNs: landing.confirmedNs });
    if (landing.status === "unknown") return { kind: "unknown", error: landing.error ?? "no confirmation" };
    const spent = await this.debit(landing.signature);
    return landing.status === "confirmed" ? { kind: "landed", spentLamports: spent, tokenAmount: null } : { kind: "failed", error: landing.error ?? "failed on chain", spentLamports: spent };
  }

  // Jupiter lands a sell itself; its code maps through shared/trading.json. A sell counts as landed
  // only with status "Success", a landed code, and a signature to check it by. Anything else, a
  // timeout, or an unreadable answer leaves it pending, with any returned signature kept.
  private async jupiterExecute(built: Built, ownSig: string | null, trace: Trace): Promise<Outcome> {
    trace.sentNs = process.hrtime.bigint();
    let result;
    try {
      result = await this.d.network.jupiterExecute(Buffer.from(built.tx.serialize()).toString("base64"), built.requestId!, this.d.config.execution.confirmTimeoutMs);
    } catch (error) {
      return { kind: "unknown", error: (error as Error).message };
    }
    trace.firstAckNs = trace.confirmedNs = process.hrtime.bigint();
    trace.firstAckBy = "jupiter";
    const detail = `Jupiter ${result.status} code ${result.code}${result.error ? `: ${result.error}` : ""}`;
    const signature = result.signature || ownSig || undefined;
    const kind = jupiterOutcome(result.code);
    if (kind === "landed" && result.status === "Success" && signature) return { kind: "landed", spentLamports: null, tokenAmount: null };
    if (kind === "rejected") return { kind: "rejected", error: detail };
    return { kind: "unknown", error: detail, signature };
  }

  private async debit(signature: string): Promise<bigint | null> {
    return this.d.network.transactionDebit(signature, this.d.wallet.publicKey.toBase58()).catch(() => null);
  }

  // ---- reconciler ----

  start(): void {
    const tick = () => void this.reconcile().catch((e: Error) => log("reconcile", e.message));
    tick();
    this.timer = setInterval(tick, 2_000);
  }

  stop(): void {
    if (this.timer) clearInterval(this.timer);
  }

  // Finishes pending trades, reads missing balances, and sells positions with exits waiting. A call
  // while another runs returns at once, so slow RPC answers never pile up.
  async reconcile(): Promise<void> {
    if (this.reconciling) return;
    this.reconciling = true;
    try {
      await this.resolvePending();
      for (const [mint, p] of this.d.book.positions()) {
        if (p.tokenAmount !== null) continue;
        this.d.book.setTokenAmount(mint, await this.d.network.tokenBalance(this.d.wallet.publicKey.toBase58(), mint).catch(() => 0n));
      }
      for (const mint of this.d.book.positionsToExit()) await this.sellIfExiting(mint);
    } finally {
      this.reconciling = false;
    }
  }

  // A confirmed signature resolves a trade. One the RPC has never seen expires only when its
  // blockhash is proven invalid and a second lookup, made after that proof, still finds nothing:
  // the transaction may confirm between the first lookup and the blockhash check.
  private async resolvePending(): Promise<void> {
    const { book, network } = this.d;
    for (const [id, p] of book.pending()) {
      if (this.sending.has(id)) continue;
      if (p.signature === null) {
        if (!this.reviewLogged.has(id)) log("reconcile", manualReviewMessage(id, p));
        this.reviewLogged.add(id);
        continue;
      }
      let status = await network.signatureStatus(p.signature).catch(() => undefined);
      if (status === null) {
        if ((await network.isBlockhashValid(p.blockhash).catch(() => true)) !== false) continue;
        status = await network.signatureStatus(p.signature).catch(() => undefined);
      }
      let outcome: Outcome | null = null;
      if (status === null) outcome = { kind: "expired" };
      else if (status?.confirmed) {
        const spent = await this.debit(p.signature);
        outcome = status.err ? { kind: "failed", error: JSON.stringify(status.err), spentLamports: spent } : { kind: "landed", spentLamports: spent, tokenAmount: null };
      }
      if (!outcome) continue;
      book.resolve(id, outcome, Date.now());
      log("reconcile", `${p.side} ${short(p.mint)} ${p.signature.slice(0, 8)}: ${outcome.kind}`);
    }
  }

  // ---- building blocks ----

  private feeTiers() {
    const tiers = this.d.cache.feeTiers();
    if (!tiers) throw new Error("pump.fun fee tiers not loaded yet");
    return tiers;
  }

  private feePlan(rule: Rule): FeePlan {
    const { cache, config } = this.d;
    const units = BigInt(config.execution.computeUnitLimit);
    const fee = rule.fees.priorityFeeSol;
    const estimate = cache.priorityMicroLamports === null ? 0n : (BigInt(cache.priorityMicroLamports) * units) / 1_000_000n;
    const priorityLamports = fee === "auto" ? min(estimate, solToLamports(config.execution.maxAutoPriorityFeeSol)) : solToLamports(fee);
    if (!rule.fees.antiMev) return { priorityLamports, jitoTip: 0n };
    const tipSol = rule.fees.tipSol === "auto" ? Math.min(cache.tipFloorSol ?? 0, config.execution.maxAutoTipSol) : rule.fees.tipSol;
    return { priorityLamports, jitoTip: max(solToLamports(tipSol), JITO_MIN_TIP) };
  }

  private buildPumpBuy(rule: Rule, mint: string, amount: bigint, curve: Curve, tokenProgram: string, plan: FeePlan): Built {
    const minOut = withSlippage(tokensForLamports(curve, amount, curveFeeBps(this.feeTiers(), curve)), rule.fees.slippagePct);
    return this.compileLocal(rule, plan, buyInstructions(this.d.wallet.publicKey, new PublicKey(mint), new PublicKey(tokenProgram), amount, minOut));
  }

  // Compute budget, the trade, then the Jito tip when antiMev is on. With antiMev the transaction
  // goes only to Jito; otherwise to every RPC URL.
  private compileLocal(rule: Rule, plan: FeePlan, trade: TransactionInstruction[]): Built {
    const targets = this.d.senders.filter((s) => s.kind === (rule.fees.antiMev ? "jito" : "rpc"));
    if (!targets.length && !this.d.dryRun) throw new Error(rule.fees.antiMev ? "antiMev is on but JITO_URL is off" : "no RPC URL to send to");
    const hash = this.d.cache.freshBlockhash();
    if (!hash) throw new Error("no fresh blockhash (the background refresh is failing)");
    const units = this.d.config.execution.computeUnitLimit;
    const payer = this.d.wallet.publicKey;
    const instructions = [
      ComputeBudgetProgram.setComputeUnitLimit({ units }),
      ComputeBudgetProgram.setComputeUnitPrice({ microLamports: (plan.priorityLamports * 1_000_000n) / BigInt(units) }),
      ...trade,
      ...(plan.jitoTip > 0n ? [SystemProgram.transfer({ fromPubkey: payer, toPubkey: JITO_TIP_ACCOUNTS[Math.floor(Math.random() * JITO_TIP_ACCOUNTS.length)]!, lamports: plan.jitoTip })] : []),
    ];
    const tx = new VersionedTransaction(new TransactionMessage({ payerKey: payer, recentBlockhash: hash.blockhash, instructions }).compileToV0Message());
    this.sign(tx);
    return { tx, route: "pump", lastValidBlockHeight: hash.lastValidBlockHeight, targets };
  }

  // Jupiter Swap V2 /order with our wallet as taker; Jupiter lands it through /execute.
  private async buildJupiter(inputMint: string, outputMint: string, amount: bigint): Promise<Built> {
    const order = await this.d.network.jupiterOrder({ inputMint, outputMint, amount, taker: this.d.wallet.publicKey.toBase58() });
    if (!order.transaction) throw new Error(`Jupiter /order returned no transaction: ${order.errorMessage ?? "no route"}`);
    const tx = VersionedTransaction.deserialize(Buffer.from(order.transaction, "base64"));
    this.sign(tx); // JupiterZ quotes add the market maker's signature during /execute
    return { tx, route: "jupiter", lastValidBlockHeight: order.lastValidBlockHeight ?? null, targets: [], requestId: order.requestId };
  }

  // Node's native Ed25519 signs in about 75 us; the pure-JS signer in web3.js takes about 400.
  private sign(tx: VersionedTransaction): void {
    tx.addSignature(this.d.wallet.publicKey, ed25519Sign(null, Buffer.from(tx.message.serialize()), this.signingKey));
  }

  private drop(fire: Fire, reason: string, trace: Trace): void {
    this.d.releaseFire(fire);
    log("skip", `${fire.rule.name} ${short(fire.mint)}: ${reason}`);
    this.d.report.finish(trace, `skipped (${reason.split(":")[0]})`);
  }
}

// The first signature when it is ours: a Jupiter transaction can leave slot 0 to another signer.
function ownSignature(tx: VersionedTransaction): string | null {
  const first = tx.signatures[0];
  return first && first.some((b) => b !== 0) ? bs58.encode(first) : null;
}

// A trade only a person can settle: the steps are in shared/README.md, section Trade state.
function manualReviewMessage(id: string, p: Pending): string {
  return [
    `${p.side} ${short(p.mint)} (trade ${id}) has no signature to check, so it needs you.`,
    `Stop the bot and look up the wallet's transactions for ${p.mint}.`,
    `If the ${p.side} landed, ${p.side === "sell" ? `delete positions["${p.mint}"]` : `add purchases["${p.mint}"] and positions["${p.mint}"]`} and pending["${id}"] in the same edit.`,
    `If it did not land, delete pending["${id}"] and subtract ${p.reservedLamports} from spend.totalLamports.`,
    "Then start the bot again.",
  ].join(" ");
}

const min = (a: bigint, b: bigint) => (a < b ? a : b);
const max = (a: bigint, b: bigint) => (a > b ? a : b);

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
