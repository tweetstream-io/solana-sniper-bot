// The whole path of one event, in order: feed -> dedupe -> store and detect -> rule match ->
// execute/trade.ts (filters, build and sign, land, resolve) -> report. Decider is the synchronous
// part (no I/O) that the shared fixtures test; startBot wires it to the feed and the trader.
// Reading order: Decider.handle here, then Trader.buy in execute/trade.ts.

import { Keypair } from "@solana/web3.js";
import { eventRef, payloadProblem, postKey, publishedAtMs, type AccountCallout, type Platform, type AccountFollow, type AccountProfileUpdate, type AccountThesis, type AccountTokenVerified, type Envelope, type TweetContent, type TweetDelete, type TweetMeta, type TweetPin, type TweetUpdate } from "./protocol.ts";
import type { Config } from "./setup/config.ts";
import { FeedClient, type Receipt } from "./feed/client.ts";
import { EventDedupe } from "./feed/dedupe.ts";
import { PostStore, type Post } from "./detect/store.ts";
import { detectMetaSignals, detectPostSignals, postActivity } from "./detect/post.ts";
import { calloutActivity, followActivity, fomoActivity, pinActivity, profileActivity } from "./detect/account.ts";
import type { Activity, Signal, SignalSource } from "./detect/types.ts";
import { RuleIndex } from "./rules/index.ts";
import { RuleEngine } from "./rules/engine.ts";
import type { Fire, Skip, SkipCode } from "./rules/modes.ts";
import type { TradeBook } from "./execute/trades.ts";
import { Trader } from "./execute/trade.ts";
import { ChainCache } from "./execute/cache.ts";
import { Lander, SignatureWatcher, type SenderTarget } from "./land/land.ts";
import type { Network } from "./network.ts";
import { LatencyReport, type Trace } from "./report/latency.ts";
import { ExitWatcher } from "./exits/exits.ts";
import { log, short } from "./log.ts";

// A delete of the post a position was bought on.
type DeleteExit = { mint: string; code: "post_deleted" };

export type Outcome = {
  duplicate: boolean;
  eventRef: string;
  signals: Signal[];
  fires: Fire[];
  skips: Skip[];
  exits: DeleteExit[];
  invalid: string | null; // why the payload was skipped, when it was malformed
  publishedAt: number | null; // publish time of a post, for publish -> receipt
  firstContent: boolean; // first tweet/content for this post
};

// The stable JSON shapes compared by shared/fixtures (documented in shared/README.md).
export type SignalJson = { event: string; source: SignalSource; chain: string; mint: string; confidence: string; actor: { namespace: string; id?: string; handle?: string } };
export type DecisionJson =
  | { event: string; rule: string; decision: "fire"; mint: string; source: SignalSource | null }
  | { event: string; rule: string; decision: "skip"; code: SkipCode }
  | { event: string; decision: "exit"; mint: string; code: "post_deleted" };

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
    ...o.fires.map((m): DecisionJson => ({ event: o.eventRef, rule: m.rule.name, decision: "fire", mint: m.mint, source: m.signal?.source ?? null })),
    ...o.skips.map((s): DecisionJson => ({ event: o.eventRef, rule: s.rule.name, decision: "skip", code: s.code })),
    ...o.exits.map((e): DecisionJson => ({ event: o.eventRef, decision: "exit", mint: e.mint, code: e.code })),
  ];
}

// Synchronous: from receipt to "these rules fire" with no await, so nothing else can interleave
// and the decision costs microseconds.
export class Decider {
  private readonly dedupe = new EventDedupe();
  private readonly store = new PostStore();
  private readonly engine: RuleEngine;
  private readonly config: Config;
  private readonly book: TradeBook;

  constructor(config: Config, book: TradeBook) {
    this.config = config;
    this.book = book;
    this.engine = new RuleEngine(new RuleIndex(config.rules), book, config);
  }

  // Gives a fire back when it sent nothing: the same key may fire again.
  release(fire: Fire): void {
    this.engine.release(fire);
  }

  handle(event: Envelope, receipt: Pick<Receipt, "wallMs">): Outcome {
    const ref = eventRef(event);
    const outcome: Outcome = { duplicate: false, eventRef: ref, signals: [], fires: [], skips: [], exits: [], invalid: null, publishedAt: null, firstContent: false };
    const pair = `${event.t}/${event.op}`;
    outcome.invalid = payloadProblem(pair, event.d);
    if (outcome.invalid) return outcome;
    if (!this.dedupe.first(event)) return { ...outcome, duplicate: true };
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
          for (const mint of this.book.boughtOnPost(postKey(event.d as TweetDelete))) outcome.exits.push({ mint, code: "post_deleted" });
        }
        break;
      case "account/follow":
      case "account/unfollow":
        activity = followActivity(event.d as AccountFollow, event.op as "follow" | "unfollow", ref);
        break;
      case "account/profile_update": {
        const d = event.d as AccountProfileUpdate;
        activity = profileActivity(d, this.store.profile(d.actor), ref);
        this.store.updateProfile(d.actor, d.changes);
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
    const platform = activity.actor.namespace === "x" ? "twitter" : activity.actor.namespace;
    const knownBio = this.store.profile({ id: activity.actor.id, platform: platform as Platform })?.bio;
    const result = this.engine.evaluate(activity, { now: receipt.wallMs, ageMs, knownBio });
    outcome.fires = result.fires;
    outcome.skips = result.skips;
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
  book: TradeBook;
  feeds: Array<{ name: string; apiKey: string }>;
  wsUrl: string;
  dryRun: boolean;
  wallet: Keypair | null; // required when dryRun is false
  senders: SenderTarget[];
  rpcWsUrl: string;
  heartbeatMs?: number;
  onFatal: (reason: string) => void;
};

export type RunningBot = { stop(): void; report: LatencyReport };

export async function startBot(o: BotOptions): Promise<RunningBot> {
  const report = new LatencyReport();
  const cache = new ChainCache(o.network);
  cache.start(o.config.execution.blockhashRefreshMs);
  await cache.ready();
  let watcher: SignatureWatcher | null = null;
  let lander: Lander | null = null;
  if (!o.dryRun) {
    watcher = new SignatureWatcher(o.rpcWsUrl);
    watcher.start();
    const statusRpcUrl = o.senders.find((s) => s.kind === "rpc")?.url ?? null;
    lander = new Lander(watcher, { ...o.config.execution, currentBlockHeight: () => cache.blockHeight(), statusRpcUrl });
  }
  const decider = new Decider(o.config, o.book);
  const trader = new Trader({
    config: o.config,
    network: o.network,
    book: o.book,
    cache,
    report,
    wallet: o.wallet ?? Keypair.generate(),
    dryRun: o.dryRun,
    lander,
    senders: o.senders,
    releaseFire: (fire) => decider.release(fire),
  });
  trader.start();
  const exits = new ExitWatcher(o.config, o.book, o.network, cache, trader);
  exits.start();

  const onEvent = (event: Envelope, receipt: Receipt) => {
    if (event.t === "control") {
      if (event.op === "auth_pong") log("auth", `${receipt.connection} authenticated`);
      return;
    }
    let outcome: Outcome;
    try {
      outcome = decider.handle(event, receipt);
    } catch (error) {
      return log("skip", `${event.t}/${event.op}: could not process the event: ${(error as Error).message}`);
    }
    const decidedNs = process.hrtime.bigint();
    if (outcome.invalid) return log("skip", `${event.t}/${event.op}: ${outcome.invalid}`);
    if (outcome.duplicate) return;
    // Trades start before anything is logged: console output is synchronous and slow.
    for (const fire of outcome.fires) {
      const trace: Trace = { mint: fire.mint, rule: fire.rule.name, connection: receipt.connection, eventRef: outcome.eventRef, publishedAtWall: outcome.publishedAt, receivedWall: receipt.wallMs, receivedNs: receipt.monoNs, decidedNs };
      trader.buy(fire, trace).catch((error: Error) => log("error", `${fire.rule.name}: ${error.message}`));
    }
    for (const e of outcome.exits) trader.exit(e.mint, e.code).catch((error: Error) => log("error", `exit ${short(e.mint)}: ${error.message}`));
    report.recordFirstCopy(receipt.connection);
    if (outcome.firstContent && outcome.publishedAt !== null) report.recordPost(receipt.connection, receipt.wallMs - outcome.publishedAt);
    for (const s of outcome.signals) log("signal", `${s.source} ${short(s.mint)} from ${s.actor.handle ? `@${s.actor.handle}` : s.actor.namespace} (${s.eventRef})`);
    for (const s of outcome.skips) log("skip", `${s.rule.name}: ${s.code}, ${s.detail}`);
    for (const f of outcome.fires) log("fire", `${f.rule.name}: buy ${short(f.mint)}, ${f.detail}`);
    for (const e of outcome.exits) log("exit", `${short(e.mint)}: the post it was bought on was deleted`);
  };

  const feeds = o.feeds.map((f) => new FeedClient({ name: f.name, apiKey: f.apiKey, url: o.wsUrl, heartbeatMs: o.heartbeatMs ?? 30_000, onEvent, onFatal: o.onFatal }));
  for (const feed of feeds) feed.start();
  return {
    report,
    stop() {
      for (const feed of feeds) feed.stop();
      exits.stop();
      trader.stop();
      cache.stop();
      watcher?.stop();
      o.book.flush();
    },
  };
}
