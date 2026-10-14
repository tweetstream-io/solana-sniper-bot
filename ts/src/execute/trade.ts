// Runs fired rules as trades. For one buy, top to bottom: reserve in the TradeBook -> filters/gate.ts
// -> build and sign (pump.fun locally, PumpPortal, or Jupiter) -> record "sending" on disk -> send
// (land/land.ts, or Jupiter /execute) -> resolve. Sells and the reconciler, which finishes trades
// whose outcome was unknown, follow the same steps. State rules live in execute/trades.ts.

import { ComputeBudgetProgram, Keypair, PublicKey, SystemProgram, TransactionInstruction, TransactionMessage, VersionedTransaction } from "@solana/web3.js";
import bs58 from "bs58";
import { createPrivateKey, sign as ed25519Sign, type KeyObject } from "node:crypto";
import type { Config, Rule } from "../setup/config.ts";
import type { Fire } from "../rules/modes.ts";
import { solToLamports, type Outcome, type Trade, type TradeBook, type Trigger } from "./trades.ts";
import { runGate, type GateResult } from "../filters/gate.ts";
import { WSOL, type Network } from "../network.ts";
import type { Lander, SenderTarget } from "../land/land.ts";
import type { LatencyReport, Trace } from "../report/latency.ts";
import type { ChainCache } from "./cache.ts";
import { bondingCurveAddress, buyInstructions, curveFeeBps, lamportsForTokens, localBuildBlocker, parseCurve, sellInstruction, tokensForLamports, warmBuyAccounts, withSlippage } from "./pump.ts";
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

// Largest costs a buy can add to its principal, for the spend reservation: the signature fee,
// rent for a Token-2022 token account with ImmutableOwner (170 bytes), and rent for pump.fun's
// per-user volume accumulator (137 bytes, 0.0018444 SOL in TRADE_V3.md). Rent is (bytes + 128) x 6960.
const SIGNATURE_FEE = 5_000n;
const TOKEN_ACCOUNT_RENT = 2_074_080n;
const VOLUME_ACCUMULATOR_RENT = 1_844_400n;
const JITO_MIN_TIP = 1_000n; // Jito ignores smaller tips
const ED25519_PKCS8 = Buffer.from("302e020100300506032b657004220420", "hex"); // header for a raw Ed25519 seed

const pick = <T>(list: T[]): T => list[Math.floor(Math.random() * list.length)]!;

// What one transaction pays besides its principal, and where it may be sent.
type FeePlan = { priorityLamports: bigint; jitoTip: bigint; senderTips: Array<{ name: string; account: PublicKey; lamports: bigint }> };

// A signed transaction ready to record and send.
type Built = { tx: VersionedTransaction; lastValidBlockHeight: number; route: Trade["route"]; targets: SenderTarget[]; requestId?: string };

export type TraderDeps = {
  config: Config;
  network: Network;
  book: TradeBook;
  cache: ChainCache;
  report: LatencyReport;
  wallet: Keypair; // a throwaway key in dry run, so build and sign are still measured
  dryRun: boolean;
  lander: Lander | null; // null in dry run
  senders: SenderTarget[]; // every configured sender; each trade picks its subset
  releaseFire: (fire: Fire) => void; // give a fire back when nothing was sent
};

export class Trader {
  private readonly d: TraderDeps;
  private readonly signingKey: KeyObject;
  private readonly inflight = new Set<string>(); // trades this process is still driving
  private timer: NodeJS.Timeout | null = null;

  constructor(d: TraderDeps) {
    this.d = d;
    this.signingKey = createPrivateKey({ key: Buffer.concat([ED25519_PKCS8, d.wallet.secretKey.subarray(0, 32)]), format: "der", type: "pkcs8" });
  }

  // ---- buy ----

  async buy(fire: Fire, trace: Trace): Promise<void> {
    const { rule, mint } = fire;
    const { book, config, dryRun } = this.d;
    const now = Date.now();
    const plan = this.feePlan(rule);
    const principal = solToLamports(rule.token.amountSol);
    const feeBudget = this.planCost(plan) + TOKEN_ACCOUNT_RENT + VOLUME_ACCUMULATOR_RENT;
    const trigger: Trigger = { rule: rule.name, fireKey: fire.fireKey, eventRef: trace.eventRef, postKey: fire.signal?.postKey, actor: fire.actor };
    const trade = book.reserveBuy({
      mint,
      trigger,
      route: dryRun ? "dry_run" : "pump",
      maxDebit: principal + feeBudget,
      feeBudget,
      caps: config.spend,
      skipIfAlreadyBought: rule.filters.skipIfAlreadyBought,
      skipIfAlreadyHolding: rule.filters.skipIfAlreadyHolding,
      now,
    });
    if (typeof trade === "string") return this.drop(fire, trade, trace);

    const pending = runGate(mint, rule, fire.signal, {
      network: this.d.network,
      safety: config.safety,
      pumpBuilder: config.execution.pumpBuilder,
      solUsd: () => this.d.cache.solUsd,
      epoch: () => this.d.cache.epoch(),
    });
    // setImmediate lets the read go out on the socket first, then uses the wait to derive addresses.
    setImmediate(() => warmBuyAccounts(mint, this.d.wallet.publicKey));
    const gate = await pending;
    trace.gateDoneNs = process.hrtime.bigint();
    trace.gateMs = gate.elapsedMs;
    if (!gate.pass) {
      book.failBeforeSend(trade.id, `${gate.code}: ${gate.detail}`);
      return this.drop(fire, `${gate.code}: ${gate.detail}`, trace);
    }
    const sent = await this.attemptBuy(fire, trade, gate, plan, principal, trace, rule.fees.retriesOnGraduation);
    if (!sent) this.d.releaseFire(fire); // nothing in this fire reached the chain
  }

  // One attempt, and a graduation retry if the curve completed under it. Returns whether any
  // transaction was sent (a dry run counts as sent); the fire is released only when none was.
  private async attemptBuy(fire: Fire, trade: Trade, gate: Extract<GateResult, { pass: true }>, plan: FeePlan, principal: bigint, trace: Trace, retries: number): Promise<boolean> {
    const { book, dryRun } = this.d;
    const { rule, mint } = fire;
    if (dryRun && gate.route === "jupiter") {
      // A Jupiter order needs a funded taker, so the dry run stops before building.
      log("dry run", `${rule.name}: would buy ${short(mint)} for ${rule.token.amountSol} SOL through Jupiter /order and /execute${describeFacts(gate)}`);
      book.confirmDryRun(trade.id, Date.now());
      book.openPosition({ mint, route: "jupiter", tokenProgram: gate.tokenProgram, openedAt: Date.now(), buyTradeId: trade.id, costLamports: principal.toString(), tokenAmount: null, trigger: trade.trigger });
      this.d.report.finish(trace, "dry run, not built");
      return true; // a dry run counts as a trade, so limits behave as they would live
    }
    let built: Built;
    try {
      built = await this.buildBuy(rule, mint, principal, gate, plan);
    } catch (error) {
      book.failBeforeSend(trade.id, `build failed: ${(error as Error).message}`);
      log("skip", `${rule.name} ${short(mint)}: build failed: ${(error as Error).message}`);
      this.d.report.finish(trace, "build failed, nothing sent");
      return false;
    }
    trace.signedNs = process.hrtime.bigint();
    const what = `${built.route} buy of ${short(mint)} for ${rule.token.amountSol} SOL${describeFacts(gate)}`;
    if (dryRun) {
      log("dry run", `${rule.name}: would send a signed ${built.tx.serialize().length}-byte ${what}`);
      const intents = book.confirmDryRun(trade.id, Date.now());
      const tokens = gate.curve && this.d.cache.feeTiers ? tokensForLamports(gate.curve, principal, curveFeeBps(this.d.cache.feeTiers, gate.curve)) : null;
      book.openPosition({ mint, route: gate.route === "jupiter" ? "jupiter" : "pump", tokenProgram: gate.tokenProgram, openedAt: Date.now(), buyTradeId: trade.id, costLamports: principal.toString(), tokenAmount: tokens?.toString() ?? null, trigger: trade.trigger });
      this.d.report.finish(trace, "dry run, not sent");
      await this.runIntents(mint, intents);
      return true; // a dry run counts as a trade, so limits behave as they would live
    }

    const outcome = await this.sendRecorded(trade, built, trace);
    if (outcome === null) return false; // the pre-send write failed; nothing went out
    log("send", `${rule.name}: ${what}, ${built.tx.signatures[0] ? bs58.encode(built.tx.signatures[0]) : ""} -> ${outcome.kind}`);
    const intents = book.resolve(trade.id, outcome, Date.now());
    this.d.report.finish(trace, `buy ${outcome.kind}${"error" in outcome ? `: ${outcome.error}` : ""}`);
    if (outcome.kind === "landed") {
      book.openPosition({ mint, route: built.route === "jupiter" ? "jupiter" : "pump", tokenProgram: gate.tokenProgram, openedAt: Date.now(), buyTradeId: trade.id, costLamports: principal.toString(), tokenAmount: outcome.tokenAmount?.toString() ?? null, trigger: trade.trigger });
      await this.runIntents(mint, intents);
    } else if (outcome.kind === "failed" && built.route === "pump" && retries > 0 && (await this.graduated(mint))) {
      // The coin left the curve while the buy was in flight. Same fire, a new trade via Jupiter.
      log("retry", `${short(mint)} graduated during the buy; retrying through Jupiter`);
      const retry = book.reserveBuy({ mint, trigger: trade.trigger, route: "jupiter", maxDebit: BigInt(trade.reservedLamports), feeBudget: BigInt(trade.feeBudgetLamports), caps: this.d.config.spend, skipIfAlreadyBought: true, skipIfAlreadyHolding: true, now: Date.now() });
      if (typeof retry !== "string") await this.attemptBuy(fire, retry, { ...gate, route: "jupiter", curve: null }, plan, principal, { ...trace }, retries - 1);
    }
    return true;
  }

  private async graduated(mint: string): Promise<boolean> {
    const [account] = await this.d.network.getAccounts([bondingCurveAddress(mint).toBase58()]).catch(() => [null]);
    return parseCurve(account?.data ?? new Uint8Array())?.complete ?? false;
  }

  private async buildBuy(rule: Rule, mint: string, lamports: bigint, gate: Extract<GateResult, { pass: true }>, plan: FeePlan): Promise<Built> {
    const { wallet, cache, network } = this.d;
    const tiers = cache.feeTiers;
    if (gate.route === "pump" && tiers && gate.curve) {
      const tokens = tokensForLamports(gate.curve, lamports, curveFeeBps(tiers, gate.curve));
      const minOut = withSlippage(tokens, rule.fees.slippagePct);
      return this.compileLocal(rule, plan, buyInstructions(wallet.publicKey, new PublicKey(mint), new PublicKey(gate.tokenProgram), lamports, minOut));
    }
    if (gate.route !== "jupiter") {
      const priorityFee = Number(plan.priorityLamports) / 1e9;
      const bytes = await network.pumpPortalTx({ publicKey: wallet.publicKey.toBase58(), action: "buy", mint, amount: Number(lamports) / 1e9, denominatedInSol: "true", slippage: rule.fees.slippagePct, priorityFee, pool: "auto" });
      return this.adoptPumpPortal(rule, plan, bytes);
    }
    return this.jupiterBuilt(WSOL, mint, lamports);
  }

  // ---- sell ----

  // "no_position" when there is nothing to sell; "busy" while an earlier sell is unresolved.
  async sell(mint: string, reason: string): Promise<"sold" | "sent" | "no_position" | "busy" | "not_sent"> {
    const { book, dryRun, network, config, wallet, cache } = this.d;
    const position = book.position(mint);
    if (!position) return "no_position";
    const trade = book.reserveSell(mint, position.trigger, dryRun ? "dry_run" : "pump", Date.now());
    if (typeof trade === "string") {
      log("exit", `${short(mint)}: not selling now (${trade})`);
      return trade === "no position" ? "no_position" : "busy";
    }
    if (dryRun) {
      log("dry run", `would sell ${short(mint)} (${reason})`);
      book.confirmDryRun(trade.id, Date.now());
      return "sold";
    }
    const trace: Trace = { mint, rule: `exit:${reason}`, connection: "-", eventRef: reason, publishedAtWall: null, receivedWall: Date.now(), receivedNs: process.hrtime.bigint() };
    trace.decidedNs = trace.receivedNs;
    let built: Built;
    try {
      const tokens = position.tokenAmount !== null ? BigInt(position.tokenAmount) : await network.tokenBalance(wallet.publicKey.toBase58(), mint);
      if (tokens <= 0n) throw new Error("token balance not known yet");
      const rule = config.rules.find((r) => r.name === position.trigger.rule) ?? config.rules[0]!;
      const plan = this.feePlan(rule);
      const [account] = await network.getAccounts([bondingCurveAddress(mint).toBase58()]);
      const curve = account ? parseCurve(account.data) : null;
      if (curve && !localBuildBlocker(curve) && config.execution.pumpBuilder === "local" && cache.feeTiers) {
        const minOut = withSlippage(lamportsForTokens(curve, tokens, curveFeeBps(cache.feeTiers, curve)), config.exits.sellSlippagePct);
        built = this.compileLocal(rule, plan, [sellInstruction(wallet.publicKey, new PublicKey(mint), new PublicKey(position.tokenProgram), tokens, minOut)]);
      } else if (curve && !curve.complete) {
        const bytes = await network.pumpPortalTx({ publicKey: wallet.publicKey.toBase58(), action: "sell", mint, amount: "100%", denominatedInSol: "false", slippage: config.exits.sellSlippagePct, priorityFee: Number(plan.priorityLamports) / 1e9, pool: "auto" });
        built = this.adoptPumpPortal(rule, plan, bytes);
      } else {
        built = await this.jupiterBuilt(mint, WSOL, tokens);
      }
    } catch (error) {
      book.failBeforeSend(trade.id, (error as Error).message);
      log("exit", `${short(mint)}: sell not sent: ${(error as Error).message}`);
      return "not_sent";
    }
    trace.signedNs = process.hrtime.bigint();
    const outcome = await this.sendRecorded(trade, built, trace);
    if (outcome === null) return "not_sent";
    book.resolve(trade.id, outcome, Date.now());
    this.d.report.finish(trace, `sell ${outcome.kind}${"error" in outcome ? `: ${outcome.error}` : ""}`);
    return outcome.kind === "landed" ? "sold" : "sent";
  }

  // An exit for a mint: sold now if there is a position, kept on the buy if the buy is pending.
  async exit(mint: string, code: string, detail: string): Promise<void> {
    const where = this.d.book.addExitIntent(mint, code, detail, Date.now());
    if (where === "pending") log("exit", `${short(mint)}: ${code} recorded on the pending buy; it sells when the buy lands`);
    if (where === "position") await this.sell(mint, code);
  }

  private async runIntents(mint: string, intents: Trade["exitIntents"]): Promise<void> {
    if (intents.length) await this.sell(mint, intents.map((i) => i.code).join(", "));
  }

  // ---- sending ----

  // Writes "sending" (signature, validity window, signed bytes) to disk before anything goes out.
  // Returns null, with the trade failed and nothing sent, when that write fails.
  private async sendRecorded(trade: Trade, built: Built, trace: Trace): Promise<Outcome | null> {
    const signature = bs58.encode(built.tx.signatures[0]!);
    const base64 = Buffer.from(built.tx.serialize()).toString("base64");
    try {
      this.d.book.markSending(trade.id, { signature, lastValidBlockHeight: built.lastValidBlockHeight, tx: base64, route: built.route, senders: built.targets.map((t) => t.name) }, Date.now());
    } catch (error) {
      this.d.book.failBeforeSend(trade.id, `state write failed, not sent: ${(error as Error).message}`);
      log("error", `${short(trade.mint)}: state write failed, transaction not sent: ${(error as Error).message}`);
      return null;
    }
    this.inflight.add(trade.id);
    try {
      return built.requestId !== undefined ? await this.jupiterExecute(built, base64, trace) : await this.landLocal(built, trace);
    } finally {
      this.inflight.delete(trade.id);
    }
  }

  private async landLocal(built: Built, trace: Trace): Promise<Outcome> {
    if (!built.targets.length) return { kind: "unknown", error: "no sender can take this transaction" };
    const landing = await this.d.lander!.land(built.tx, built.lastValidBlockHeight, built.targets);
    Object.assign(trace, { sentNs: landing.sentNs, firstAckNs: landing.firstAckNs, firstAckBy: landing.firstAckBy, confirmedNs: landing.confirmedNs });
    if (landing.status === "unknown") return { kind: "unknown", error: landing.error ?? "no confirmation" };
    const spent = await this.debit(landing.signature);
    return landing.status === "confirmed" ? { kind: "landed", tokenAmount: null, spentLamports: spent } : { kind: "failed", error: landing.error ?? "failed on chain", spentLamports: spent };
  }

  // Jupiter lands the transaction itself. Its documented codes: 0 success; -1, -2, -3 and the
  // -1002..-1004 / -2002..-2004 group reject it before landing; -1000, -2000 "failed to land" and
  // -1001, -2001 "unknown error" are not proof either way, nor is a timeout or an unreadable answer.
  private async jupiterExecute(built: Built, base64: string, trace: Trace): Promise<Outcome> {
    trace.sentNs = process.hrtime.bigint();
    let result;
    try {
      result = await this.d.network.jupiterExecute(base64, built.requestId!, this.d.config.execution.confirmTimeoutMs);
    } catch (error) {
      return { kind: "unknown", error: (error as Error).message };
    }
    trace.firstAckNs = trace.confirmedNs = process.hrtime.bigint();
    trace.firstAckBy = "jupiter";
    if (result.status === "Success" && result.code === 0) {
      const signature = result.signature ?? bs58.encode(built.tx.signatures[0]!);
      const tokens = result.totalOutputAmount ? BigInt(result.totalOutputAmount) : null;
      return { kind: "landed", tokenAmount: built.route === "jupiter" && tokens !== null ? tokens : null, spentLamports: await this.debit(signature), signature };
    }
    const rejected = [-1, -2, -3, -1002, -1003, -1004, -2002, -2003, -2004];
    if (rejected.includes(result.code)) return { kind: "rejected", error: `Jupiter code ${result.code}${result.error ? `: ${result.error}` : ""}` };
    return { kind: "unknown", error: `Jupiter code ${result.code}${result.error ? `: ${result.error}` : ""}` };
  }

  private async debit(signature: string): Promise<bigint | null> {
    return this.d.network.transactionDebit(signature, this.d.wallet.publicKey.toBase58()).catch(() => null);
  }

  // ---- reconciler ----

  // At startup and every 2 seconds: finish trades whose outcome is not known yet, rebroadcast
  // the ones still valid, and read token balances that are still missing.
  start(): void {
    for (const t of this.d.book.recoverAfterRestart()) log("state", `trade ${t.id} (${t.side} ${short(t.mint)}) was never sent; released`);
    const tick = () => this.reconcile().catch((e: Error) => log("reconcile", e.message));
    void tick();
    this.timer = setInterval(tick, 2_000);
  }

  stop(): void {
    if (this.timer) clearInterval(this.timer);
  }

  async reconcile(): Promise<void> {
    const { book, network, cache } = this.d;
    for (const trade of book.unresolved()) {
      if (this.inflight.has(trade.id) || !trade.signature) continue;
      const status = await network.signatureStatus(trade.signature).catch(() => undefined);
      if (status === undefined) continue; // RPC unreachable: try again next tick
      const height = cache.blockHeight();
      let outcome: Outcome | null = null;
      if (status?.confirmed) {
        const spent = await this.debit(trade.signature);
        outcome = status.err ? { kind: "failed", error: JSON.stringify(status.err), spentLamports: spent } : { kind: "landed", tokenAmount: null, spentLamports: spent };
      } else if (!status && height !== null && trade.lastValidBlockHeight !== undefined && height > trade.lastValidBlockHeight) {
        outcome = { kind: "expired" };
      } else if (trade.tx && this.d.lander) {
        await this.d.lander.rebroadcast(trade.tx, this.d.senders.filter((s) => trade.senders?.includes(s.name)));
      }
      if (!outcome) continue;
      const intents = book.resolve(trade.id, outcome, Date.now());
      log("reconcile", `${trade.side} ${short(trade.mint)} ${trade.signature.slice(0, 8)}: ${outcome.kind}`);
      if (trade.side === "buy" && outcome.kind === "landed") {
        const [mintAccount] = await network.getAccounts([trade.mint]).catch(() => [null]);
        book.openPosition({ mint: trade.mint, route: trade.route === "jupiter" ? "jupiter" : "pump", tokenProgram: mintAccount?.owner ?? "", openedAt: Date.now(), buyTradeId: trade.id, costLamports: (BigInt(trade.reservedLamports) - BigInt(trade.feeBudgetLamports)).toString(), tokenAmount: null, trigger: trade.trigger });
        await this.runIntents(trade.mint, intents);
      }
    }
    // A balance read right after a buy can still show 0 at the RPC; keep asking until it is positive.
    for (const p of book.positions().filter((p) => p.tokenAmount === null)) {
      const balance = await network.tokenBalance(this.d.wallet.publicKey.toBase58(), p.mint).catch(() => 0n);
      if (balance > 0n) book.setTokenAmount(p.mint, balance);
    }
  }

  // ---- building blocks ----

  private feePlan(rule: Rule): FeePlan {
    const { cache, config } = this.d;
    const units = BigInt(config.execution.computeUnitLimit);
    const fee = rule.fees.priorityFeeSol;
    const estimate = cache.priorityMicroLamports === null ? 0n : (BigInt(cache.priorityMicroLamports) * units) / 1_000_000n;
    const priorityLamports = fee === "auto" ? min(estimate, solToLamports(config.execution.maxAutoPriorityFeeSol)) : solToLamports(fee);
    const jitoUsable = rule.fees.antiMev && this.d.senders.some((s) => s.kind === "jito");
    const tipSol = rule.fees.tipSol === "auto" ? Math.min(cache.tipFloorSol ?? 0, config.execution.maxAutoTipSol) : rule.fees.tipSol;
    const jitoTip = jitoUsable ? max(solToLamports(tipSol), JITO_MIN_TIP) : 0n;
    // With antiMev the transaction goes only to Jito, so other senders' tips are not paid.
    const senderTips = rule.fees.antiMev
      ? []
      : config.senders.filter((s) => s.tipAccounts.length && s.tipSol > 0).map((s) => ({ name: s.name, account: new PublicKey(pick(s.tipAccounts)), lamports: solToLamports(s.tipSol) }));
    return { priorityLamports, jitoTip, senderTips };
  }

  private planCost(plan: FeePlan): bigint {
    return SIGNATURE_FEE + plan.priorityLamports + plan.jitoTip + plan.senderTips.reduce((sum, t) => sum + t.lamports, 0n);
  }

  // antiMev: Jito only, and only when the transaction carries its tip. Otherwise every RPC, plus
  // each tip-requiring sender whose tip the transaction carries.
  private targetsFor(rule: Rule, plan: FeePlan, tipped: boolean): SenderTarget[] {
    if (rule.fees.antiMev) return tipped && plan.jitoTip > 0n ? this.d.senders.filter((s) => s.kind === "jito") : [];
    const paid = new Set(tipped ? plan.senderTips.map((t) => t.name) : []);
    return this.d.senders.filter((s) => s.kind === "rpc" && (!s.needsTip || paid.has(s.name)));
  }

  private tipInstructions(plan: FeePlan): TransactionInstruction[] {
    const from = this.d.wallet.publicKey;
    const tips = plan.jitoTip > 0n ? [SystemProgram.transfer({ fromPubkey: from, toPubkey: pick(JITO_TIP_ACCOUNTS), lamports: plan.jitoTip })] : [];
    return [...tips, ...plan.senderTips.map((t) => SystemProgram.transfer({ fromPubkey: from, toPubkey: t.account, lamports: t.lamports }))];
  }

  // Compute budget, the trade, then every tip, so one signed transaction suits every sender.
  private compileLocal(rule: Rule, plan: FeePlan, trade: TransactionInstruction[]): Built {
    const hash = this.d.cache.freshBlockhash();
    if (!hash) throw new Error("no fresh blockhash (the background refresh is failing)");
    const units = this.d.config.execution.computeUnitLimit;
    const instructions = [
      ComputeBudgetProgram.setComputeUnitLimit({ units }),
      ComputeBudgetProgram.setComputeUnitPrice({ microLamports: (plan.priorityLamports * 1_000_000n) / BigInt(units) }),
      ...trade,
      ...this.tipInstructions(plan),
    ];
    const message = new TransactionMessage({ payerKey: this.d.wallet.publicKey, recentBlockhash: hash.blockhash, instructions }).compileToV0Message();
    const tx = new VersionedTransaction(message);
    this.sign(tx);
    return { tx, lastValidBlockHeight: hash.lastValidBlockHeight, route: "pump", targets: this.targetsFor(rule, plan, true) };
  }

  // PumpPortal builds the trade. When its message uses no lookup tables we add our tips and our
  // fresh blockhash, which also gives a known validity window. Otherwise it goes out unchanged, to
  // senders that need no tip, with a window of 150 blocks from now as an upper bound.
  private adoptPumpPortal(rule: Rule, plan: FeePlan, bytes: Uint8Array): Built {
    const original = VersionedTransaction.deserialize(bytes);
    const hash = this.d.cache.freshBlockhash();
    if (original.message.addressTableLookups.length === 0 && hash) {
      const decompiled = TransactionMessage.decompile(original.message);
      const message = new TransactionMessage({ payerKey: decompiled.payerKey, recentBlockhash: hash.blockhash, instructions: [...decompiled.instructions, ...this.tipInstructions(plan)] }).compileToV0Message();
      const tx = new VersionedTransaction(message);
      this.sign(tx);
      return { tx, lastValidBlockHeight: hash.lastValidBlockHeight, route: "pumpportal", targets: this.targetsFor(rule, plan, true) };
    }
    const height = this.d.cache.blockHeight();
    if (height === null) throw new Error("block height unknown");
    this.sign(original);
    return { tx: original, lastValidBlockHeight: height + 150, route: "pumpportal", targets: this.targetsFor(rule, plan, false) };
  }

  // Jupiter Swap V2 /order with our wallet as taker. Jupiter lands it through /execute, so the
  // bot's own senders are not used. The order's lastValidBlockHeight is the hard expiry.
  private async jupiterBuilt(inputMint: string, outputMint: string, amount: bigint): Promise<Built> {
    const order = await this.d.network.jupiterOrder({ inputMint, outputMint, amount, taker: this.d.wallet.publicKey.toBase58() });
    if (!order.transaction) throw new Error(`Jupiter /order returned no transaction: ${order.errorMessage ?? "no route"}`);
    const height = this.d.cache.blockHeight();
    const lastValid = order.lastValidBlockHeight ?? (height !== null ? height + 150 : null);
    if (lastValid === null) throw new Error("block height unknown");
    const tx = VersionedTransaction.deserialize(Buffer.from(order.transaction, "base64"));
    this.sign(tx); // JupiterZ quotes add the market maker's signature during /execute
    return { tx, lastValidBlockHeight: lastValid, route: "jupiter", targets: [], requestId: order.requestId };
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
