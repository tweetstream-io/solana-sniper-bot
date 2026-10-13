// One matcher per rule mode. Each answers fire (with a mint), skip (with a stable code), or null
// when the activity is not for that rule. Pure and synchronous. Next: rules/engine.ts.

import type { KeywordFilter, Rule } from "../setup/config.ts";
import type { Activity, CalloutActivity, Confidence, FollowActivity, FomoActivity, PostActivity, ProfileActivity, Signal } from "../detect/types.ts";
import { matchesAccount } from "./index.ts";

// Stable codes, shared with the Rust bot through shared/fixtures. Free text goes in `detail`.
export type SkipCode =
  | "stale_post"
  | "keyword_include_missing"
  | "keyword_exclude_found"
  | "no_contract_address"
  | "evm_not_supported"
  | "low_confidence"
  | "mint_not_in_bio"
  | "bio_not_in_payload"
  | "update_not_new"
  | "multiple_too_high"
  | "caller_not_holding"
  | "cooldown"
  | "max_snipes";

export type Match = {
  kind: "fire";
  rule: Rule;
  mint: string;
  signal: Signal | null; // the signal that supplied the mint; null when the rule names the mint
  detail: string;
  fireKey: string; // a rule fires once per key: per post, per account event, or per mint
};
export type Skip = { kind: "skip"; rule: Rule; code: SkipCode; detail: string };
// Sell a position this rule bought, because the account it followed has left the trade.
export type Exit = { kind: "exit"; rule: Rule; mint: string; code: "caller_exited" | "author_exited"; detail: string };
export type Result = Match | Skip | null;

const RANK: Record<Confidence, number> = { low: 0, medium: 1, high: 2 };

const skip = (rule: Rule, code: SkipCode, detail: string): Skip => ({ kind: "skip", rule, code, detail });

// Whole word, without case: "cat" matches "Cat!" but not "catalog".
function containsWord(text: string, word: string): boolean {
  const escaped = word.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`(^|[^\\p{L}\\p{N}_])${escaped}($|[^\\p{L}\\p{N}_])`, "iu").test(text);
}

// texts[0] is the actor's own text; texts[n] is n levels of referenced posts.
function findWord(texts: string[][], filter: KeywordFilter): string | null {
  const scanned = texts.slice(0, filter.scanDepth + 1).flat();
  return filter.words.find((word) => scanned.some((text) => containsWord(text, word))) ?? null;
}

function keywordSkip(rule: Rule, texts: string[][]): Skip | null {
  const { includeKeywords: inc, excludeKeywords: exc } = rule;
  if (inc.words.length && findWord(texts, inc) === null) {
    return skip(rule, "keyword_include_missing", `none of [${inc.words.join(", ")}] within scan depth ${inc.scanDepth}`);
  }
  const found = findWord(texts, exc);
  return found === null ? null : skip(rule, "keyword_exclude_found", `"${found}" within scan depth ${exc.scanDepth}`);
}

// The configured mint, or with useDetectedCa the first Solana address the event carried.
function pickMint(rule: Rule, signals: Signal[], detail: string, fireKey: string): Match | Skip {
  if (rule.token.mint) return { kind: "fire", rule, mint: rule.token.mint, signal: null, detail, fireKey };
  const signal = signals.find((s) => s.chain === "solana");
  if (signal) return { kind: "fire", rule, mint: signal.mint, signal, detail: `${detail}, CA from ${signal.source}`, fireKey };
  const evm = signals.find((s) => s.chain === "evm");
  return evm
    ? skip(rule, "evm_not_supported", `${detail}: only an EVM address ${evm.mint}; execution is Solana-only`)
    : skip(rule, "no_contract_address", `${detail}: no contract address in the event yet`);
}

// The token the event names, limited to token.mint when the rule sets one.
function eventToken(rule: Rule, signals: Signal[], detail: string): Match | Skip | null {
  const signal = signals[0];
  if (!signal) return skip(rule, "no_contract_address", `${detail}: the token address is not a valid Solana or EVM address`);
  if (rule.token.mint && signal.mint !== rule.token.mint) return null;
  if (signal.chain !== "solana") return skip(rule, "evm_not_supported", `${detail}: EVM token ${signal.mint}; execution is Solana-only`);
  return { kind: "fire", rule, mint: signal.mint, signal, detail, fireKey: `${rule.name}|${signal.mint}` };
}

// Mode 1: trade contract addresses the watched accounts post.
export function matchCaScanner(rule: Rule, activity: Activity, knownBio: string | undefined): Array<Match | Skip> {
  const out: Array<Match | Skip> = [];
  for (const s of activity.signals) {
    if (!(rule.sources as string[]).includes(s.source)) continue;
    if (rule.token.mint && s.mint !== rule.token.mint) continue;
    if (s.chain !== "solana") out.push(skip(rule, "evm_not_supported", `EVM address ${s.mint} in ${s.source}`));
    else if (RANK[s.confidence] < RANK[rule.minConfidence]) {
      out.push(skip(rule, "low_confidence", `${s.source} signal is ${s.confidence}, rule needs ${rule.minConfidence}`));
    } else if (rule.requireMintInBio && !(knownBio ?? "").includes(s.mint)) {
      out.push(skip(rule, "mint_not_in_bio", `${s.mint} is not in the account's last known bio`));
    } else {
      out.push({ kind: "fire", rule, mint: s.mint, signal: s, detail: `CA ${s.detail}`, fireKey: `${rule.name}|${s.mint}` });
    }
  }
  return out;
}

// Mode 2: the reactor retweets, quotes, or replies to the target post or a target account.
export function matchEngagement(rule: Rule, a: PostActivity): Result {
  if (a.kind === "post" || !rule.actions.includes(a.kind) || !a.target) return null;
  const target = a.target;
  const onTarget = rule.targetTweetId
    ? target.tweetId === rule.targetTweetId
    : rule.reactedTo.some((spec) => matchesAccount(spec, target.account));
  if (!onTarget) return null;
  const blocked = keywordSkip(rule, a.texts);
  if (blocked) return blocked;
  const what = rule.targetTweetId ? `post ${rule.targetTweetId}` : `@${target.account.handle ?? target.account.id}`;
  return pickMint(rule, a.signals, `${a.kind} of ${what}`, `${rule.name}|${a.postKey}`);
}

// Mode 3: a watched account posts, retweets, quotes, replies, or changes its bio.
export function matchNewTweet(rule: Rule, a: PostActivity | ProfileActivity): Result {
  if (a.type === "profile") {
    if (!rule.events.includes("bio") || a.bio === null) return null;
    const blocked = keywordSkip(rule, [[a.bio]]);
    return blocked ?? pickMint(rule, a.signals, "bio changed", `${rule.name}|${a.eventRef}`);
  }
  if (!rule.events.includes(a.kind)) return null;
  const blocked = keywordSkip(rule, a.texts);
  return blocked ?? pickMint(rule, a.signals, `new ${a.kind}`, `${rule.name}|${a.postKey}`);
}

// Mode 4: a watched account follows or unfollows someone.
export function matchFollow(rule: Rule, a: FollowActivity): Result {
  if (!rule.followEvents.includes(a.op)) return null;
  if (rule.targets.length && !rule.targets.some((spec) => matchesAccount(spec, a.target))) return null;
  const detail = `${a.op} of @${a.target.handle}`;
  if (!rule.token.mint && rule.useDetectedCa && !a.bioInPayload) {
    return skip(rule, "bio_not_in_payload", `${detail}: no target bio in the event, and this example does not look profiles up`);
  }
  return pickMint(rule, a.signals, detail, `${rule.name}|${a.eventRef}`);
}

// Mode 5: a watched account changes its avatar, or another selected profile field.
export function matchPfp(rule: Rule, a: ProfileActivity): Result {
  const hit = rule.fields.filter((f) => a.changed.includes(f));
  if (!hit.length) return null;
  return pickMint(rule, a.signals, `${hit.join(", ")} changed`, `${rule.name}|${a.eventRef}`);
}

// Mode 6: a watched pump.fun caller posts a callout. callout_update only enriches an earlier call,
// so it never buys; it can still report that the caller sold.
export function matchCallout(rule: Rule, a: CalloutActivity): Array<Match | Skip | Exit> {
  const out: Array<Match | Skip | Exit> = [];
  const mint = a.signals[0]?.mint;
  if (rule.sellWhenCallerExits && a.callerHolds === false && mint) {
    out.push({ kind: "exit", rule, mint, code: "caller_exited", detail: `@${a.actor.handle} reports holding 0` });
  }
  if (a.update) {
    if (rule.calloutKinds.includes(a.kind)) out.push(skip(rule, "update_not_new", "callout_update enriches an earlier call"));
    return out;
  }
  if (!rule.calloutKinds.includes(a.kind)) return out;
  const detail = `${a.kind} by ${a.actor.handle}`;
  if (rule.maxMultiple !== null && a.multiple !== null && a.multiple > rule.maxMultiple) {
    out.push(skip(rule, "multiple_too_high", `${detail}: already ${a.multiple}x, rule allows ${rule.maxMultiple}x`));
  } else if (rule.requireCallerPosition && a.callerHolds !== true) {
    out.push(skip(rule, "caller_not_holding", `${detail}: the event does not show the caller holding the token`));
  } else {
    const result = eventToken(rule, a.signals, detail);
    if (result) out.push(result);
  }
  return out;
}

// Mode 7: a FOMO thesis by a watched author or on a watched coin, or a watched coin's first
// observed verification.
export function matchFomo(rule: Rule, a: FomoActivity): Array<Match | Skip | Exit> {
  const out: Array<Match | Skip | Exit> = [];
  const byWatchedAuthor = rule.fomoUsers.some((u) => [a.actor.id, a.actor.handle].some((v) => v?.toLowerCase() === u.toLowerCase()));
  const mint = a.signals[0]?.mint;
  if (a.op === "thesis" && rule.sellWhenAuthorExits && a.authorExited && byWatchedAuthor && mint) {
    out.push({ kind: "exit", rule, mint, code: "author_exited", detail: `${a.actor.handle} closed the position` });
  }
  if (a.authorExited || !rule.fomoEvents.includes(a.op)) return out; // a closing thesis is never a buy
  if (a.op === "token_verified" && !rule.fomoCoins.includes(a.coin)) return out;
  if (a.op === "thesis") {
    const blocked = keywordSkip(rule, [[a.text]]);
    if (blocked) return [...out, blocked];
  }
  const detail = a.op === "thesis" ? `thesis by ${a.actor.handle}` : "first observed verification";
  const result = eventToken(rule, a.signals, detail);
  return result ? [...out, result] : out;
}
