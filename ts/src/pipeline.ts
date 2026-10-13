// The whole path of one event, in order: feed -> dedupe -> store and detect -> rule match ->
// filters -> build and sign -> land -> report. Decider is the synchronous part (no I/O) that the
// shared fixtures test; startBot wires it to the feed, the trader, and the exit watcher.

import { Keypair } from "@solana/web3.js";
import { eventRef, postKey, publishedAtMs, type AccountCallout, type AccountFollow, type AccountProfileUpdate, type AccountThesis, type AccountTokenVerified, type Envelope, type TweetContent, type TweetDelete, type TweetMeta, type TweetPin, type TweetUpdate } from "./protocol.ts";
import type { Config } from "./setup/config.ts";
import { FeedClient, type Receipt } from "./feed/client.ts";
import { EventDedupe } from "./feed/dedupe.ts";
import { PostStore, type Post } from "./detect/store.ts";
import { detectMetaSignals, detectPostSignals, postActivity } from "./detect/post.ts";
import { calloutActivity, followActivity, fomoActivity, pinActivity, profileActivity } from "./detect/account.ts";
import type { Activity, Signal, SignalSource } from "./detect/types.ts";
import { RuleIndex } from "./rules/index.ts";
import { RuleEngine } from "./rules/engine.ts";
import type { Match, Skip, SkipCode } from "./rules/modes.ts";
import type { Ledger } from "./rules/ledger.ts";
import { ChainCache, Trader } from "./execute/trade.ts";
import { Lander, SignatureWatcher, type SenderTarget } from "./land/land.ts";
import type { Network } from "./network.ts";
import { LatencyReport, type Trace } from "./report/latency.ts";
import { ExitWatcher } from "./exits/exits.ts";
import { log, short } from "./log.ts";

export type ExitCode = "post_deleted" | "caller_exited" | "author_exited";
type PlannedExit = { mint: string; code: ExitCode; rule: string | null; detail: string };

export type Outcome = {
  duplicate: boolean;
  eventRef: string;
  signals: Signal[];
  triggers: Match[];
  skips: Skip[];
  exits: PlannedExit[];
  publishedAt: number | null; // publish time of a post, for publish -> receipt
  firstContent: boolean; // first tweet/content for this post
};

// The stable JSON shapes compared by shared/fixtures (documented in shared/README.md).
export type SignalJson = { event: string; source: SignalSource; chain: string; mint: string; confidence: string; actor: { namespace: string; id?: string; handle?: string } };
export type DecisionJson =
  | { event: string; rule: string; decision: "fire"; mint: string; action: "buy" | "sell"; source: SignalSource | null }
  | { event: string; rule: string; decision: "skip"; code: SkipCode }
  | { event: string; rule?: string; decision: "exit"; mint: string; code: ExitCode };

export function signalsJson(o: Outcome): SignalJson[] {
  return o.signals.map((s) => ({
    event: s.eventRef,
    source: s.source,
    chain: s.chain,
    mint: s.mint,
    confidence: s.confidence,
    actor: JSON.parse(JSON.stringify(s.actor)) as SignalJson["actor"], // drops undefined fields
  }));
}

export function decisionsJson(o: Outcome): DecisionJson[] {
  return [
    ...o.triggers.map((m): DecisionJson => ({ event: o.eventRef, rule: m.rule.name, decision: "fire", mint: m.mint, action: m.rule.token.action, source: m.signal?.source ?? null })),
    ...o.skips.map((s): DecisionJson => ({ event: o.eventRef, rule: s.rule.name, decision: "skip", code: s.code })),
    ...o.exits.map((e): DecisionJson => ({ event: o.eventRef, ...(e.rule ? { rule: e.rule } : {}), decision: "exit", mint: e.mint, code: e.code })),
  ];
}

// Synchronous: from receipt to "these rules fire" with no await, so nothing else can interleave
// and the decision costs microseconds.
export class Decider {
  private readonly dedupe = new EventDedupe();
  private readonly store = new PostStore();
  private readonly engine: RuleEngine;
  private readonly config: Config;
  private readonly ledger: Ledger;

  constructor(config: Config, ledger: Ledger) {
    this.config = config;
    this.ledger = ledger;
    this.engine = new RuleEngine(new RuleIndex(config.rules), ledger, config);
  }

  handle(event: Envelope, receipt: Pick<Receipt, "wallMs">): Outcome {
    const ref = eventRef(event);
    const outcome: Outcome = { duplicate: false, eventRef: ref, signals: [], triggers: [], skips: [], exits: [], publishedAt: null, firstContent: false };
    if (!this.dedupe.first(event)) return { ...outcome, duplicate: true };
    const pair = `${event.t}/${event.op}`;
    let activity: Activity | null = null;
    switch (pair) {
      case "tweet/content":
      case "tweet/update":
      case "tweet/meta": {
        const d = event.d as TweetContent | TweetUpdate | TweetMeta;
        const key = postKey(d);
        const hadContent = this.store.get(key)?.post?.text !== undefined;
        if (pair === "tweet/content") this.store.applyContent(key, d as TweetContent);
        else if (pair === "tweet/update") this.store.applyUpdate(key, d as TweetUpdate);
        else this.store.applyMeta(key, d as TweetMeta);
        const entry = this.store.get(key)!;
        const author = entry.post?.author;
        if (!entry.post || (!author?.id && !author?.handle)) break; // no author yet: wait for the content
        activity = this.postActivityFor(key, entry.post, entry.meta, ref);
        outcome.publishedAt = publishedAtMs(key.split(":")[0]!, entry.post.tweetId, entry.post.createdAt);
        outcome.firstContent = pair === "tweet/content" && !hadContent;
        break;
      }
      case "tweet/pin":
        activity = pinActivity(event.d as TweetPin, postKey(event.d as TweetPin), ref);
        break;
      case "tweet/delete":
        if (this.config.exits.sellOnDelete) {
          for (const mint of this.ledger.mintsForPost(postKey(event.d as TweetDelete))) {
            outcome.exits.push({ mint, code: "post_deleted", rule: null, detail: "the post that triggered the buy was deleted" });
          }
        }
        break;
      case "account/follow":
      case "account/unfollow":
        activity = followActivity(event.d as AccountFollow, event.op as "follow" | "unfollow", ref);
        break;
      case "account/profile_update": {
        const d = event.d as AccountProfileUpdate;
        activity = profileActivity(d, this.store.profile(d.actor.id), ref);
        this.store.updateProfile(d.actor.id, d.changes);
        break;
      }
      case "account/callout":
      case "account/callout_update":
        activity = calloutActivity(event.d as AccountCallout, event.op === "callout_update", ref);
        break;
      case "account/thesis":
      case "account/token_verified":
        activity = fomoActivity(event.op as "thesis" | "token_verified", event.d as AccountThesis | AccountTokenVerified, ref);
        break;
    }
    if (!activity) return outcome;
    outcome.signals = activity.signals;
    const ageMs = outcome.publishedAt === null ? null : receipt.wallMs - outcome.publishedAt;
    const result = this.engine.evaluate(activity, { now: receipt.wallMs, ageMs, knownBio: this.store.profile(activity.actor.id)?.bio });
    outcome.triggers = result.triggers;
    outcome.skips = result.skips;
    for (const exit of result.exits) {
      if (this.ledger.position(exit.mint)?.rule === exit.rule.name) outcome.exits.push({ mint: exit.mint, code: exit.code, rule: exit.rule.name, detail: exit.detail });
    }
    return outcome;
  }

  private postActivityFor(key: string, post: Post, meta: TweetMeta | null, ref: string): Activity {
    const signals = detectPostSignals(post, key, ref);
    const fromMeta = meta ? detectMetaSignals(meta, post.author, key, ref) : [];
    const seen = new Set(signals.map((s) => s.mint));
    return postActivity(post, key, ref, [...signals, ...fromMeta.filter((s) => !seen.has(s.mint))]);
  }
}

export type BotOptions = {
  config: Config;
  network: Network;
  ledger: Ledger;
  feeds: Array<{ name: string; apiKey: string }>;
  wsUrl: string;
  dryRun: boolean;
  wallet: Keypair | null; // required when dryRun is false
  senders: SenderTarget[];
  rpcWsUrl: string;
  jitoEnabled: boolean;
  jupiterHeaders: Record<string, string>;
  heartbeatMs?: number;
  onFatal: (reason: string) => void;
};

export type RunningBot = { stop(): void; report: LatencyReport };

export function startBot(o: BotOptions): RunningBot {
  const report = new LatencyReport();
  const cache = new ChainCache(o.network);
  cache.start(o.config.execution.blockhashRefreshMs);
  let watcher: SignatureWatcher | null = null;
  let lander: Lander | null = null;
  if (!o.dryRun) {
    watcher = new SignatureWatcher(o.rpcWsUrl);
    watcher.start();
    lander = new Lander(o.senders, watcher, { ...o.config.execution, currentBlockHeight: () => cache.currentBlockHeight() });
  }
  const trader = new Trader({
    config: o.config,
    network: o.network,
    ledger: o.ledger,
    cache,
    report,
    wallet: o.wallet ?? Keypair.generate(),
    dryRun: o.dryRun,
    lander,
    jitoEnabled: o.jitoEnabled,
    jupiterHeaders: o.jupiterHeaders,
  });
  const decider = new Decider(o.config, o.ledger);
  const exits = new ExitWatcher(o.config, o.ledger, o.network, trader);
  exits.start();

  const onEvent = (event: Envelope, receipt: Receipt) => {
    if (event.t === "control") {
      if (event.op === "auth_pong") log("auth", `${receipt.connection} authenticated`);
      return;
    }
    const outcome = decider.handle(event, receipt);
    const decidedNs = process.hrtime.bigint();
    if (outcome.duplicate) return;
    // Trades start before anything is logged: console output is synchronous and slow.
    for (const m of outcome.triggers) {
      const trace: Trace = {
        mint: m.mint,
        rule: m.rule.name,
        connection: receipt.connection,
        publishedAtWall: outcome.publishedAt,
        receivedWall: receipt.wallMs,
        receivedNs: receipt.monoNs,
        decidedNs,
      };
      const run = m.rule.token.action === "buy" ? trader.buy(m, trace) : trader.sell(m.mint, `rule ${m.rule.name}`);
      run.catch((error: Error) => log("error", `${m.rule.name}: ${error.message}`));
    }
    for (const e of outcome.exits) trader.sell(e.mint, e.code).catch((error: Error) => log("error", `exit ${short(e.mint)}: ${error.message}`));
    report.recordFirstCopy(receipt.connection);
    if (outcome.firstContent && outcome.publishedAt !== null) report.recordPost(receipt.connection, receipt.wallMs - outcome.publishedAt);
    for (const s of outcome.signals) log("signal", `${s.source} ${short(s.mint)} from ${s.actor.handle ? `@${s.actor.handle}` : s.actor.namespace} (${s.eventRef})`);
    for (const s of outcome.skips) log("skip", `${s.rule.name}: ${s.code}, ${s.detail}`);
    for (const m of outcome.triggers) log("fire", `${m.rule.name}: ${m.rule.token.action} ${short(m.mint)}, ${m.detail}`);
    for (const e of outcome.exits) log("exit", `${short(e.mint)}: ${e.code}, ${e.detail}`);
  };

  const feeds = o.feeds.map(
    (f) => new FeedClient({ name: f.name, apiKey: f.apiKey, url: o.wsUrl, heartbeatMs: o.heartbeatMs ?? 30_000, onEvent, onFatal: o.onFatal }),
  );
  for (const feed of feeds) feed.start();
  return {
    report,
    stop() {
      for (const feed of feeds) feed.stop();
      exits.stop();
      cache.stop();
      watcher?.stop();
      o.ledger.flushSync();
    },
  };
}
