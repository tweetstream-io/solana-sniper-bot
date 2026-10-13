// Detectors for posts: tweet/content, tweet/update (after merging), and tweet/meta.
// Pure and synchronous, so detection costs microseconds and tests need no network.
// Next: rules/engine.ts.

import type { Author, Reference, TweetMeta } from "../protocol.ts";
import { normalizeHandle } from "../protocol.ts";
import type { Post } from "./store.ts";
import { addressesFromUrl, DENYLIST, findAll, isSolanaAddress, type FoundAddress } from "./addresses.ts";
import type { Actor, Confidence, PostActivity, Signal, SignalSource } from "./types.ts";

const MAX_REF_DEPTH = 4;

export const xActor = (author: Author | undefined): Actor => ({
  namespace: "x",
  id: author?.id,
  handle: normalizeHandle(author?.handle),
});

export function makeSignal(
  source: SignalSource,
  found: Pick<FoundAddress, "chain" | "address">,
  actor: Actor,
  eventRef: string,
  confidence: Confidence,
  detail: string,
  extra: Partial<Signal> = {},
): Signal {
  return { source, chain: found.chain, mint: found.address, actor, eventRef, confidence, detail, ...extra };
}

// A link names a mint more reliably than a bare string, except chart links that may name a pool.
const linkConfidence = (f: FoundAddress): Confidence => (f.via === "dexscreener" ? "low" : "high");

// Referenced posts by depth: level 1 is the direct reference, level 2 what that one references.
export function refLevels(ref: Reference | undefined): Reference[][] {
  const levels: Reference[][] = [];
  let current = ref ? [ref] : [];
  for (let depth = 0; current.length && depth < MAX_REF_DEPTH; depth++) {
    levels.push(current);
    current = current.flatMap((r) => [r.quoted, r.subtweet].filter((x): x is Reference => x !== undefined));
  }
  return levels;
}

// Every contract address in a post, strongest evidence first, one signal per mint.
export function detectPostSignals(post: Post, postKey: string, eventRef: string): Signal[] {
  const actor = xActor(post.author);
  const extra = { postKey };
  const out: Signal[] = [];
  for (const u of post.urls ?? []) {
    for (const f of addressesFromUrl(u.url)) out.push(makeSignal("url", f, actor, eventRef, linkConfidence(f), `${f.via} link`, extra));
  }
  for (const f of findAll(post.text)) {
    if (f.via === "address") out.push(makeSignal("text", f, actor, eventRef, "high", "in post text", extra));
    else out.push(makeSignal("url", f, actor, eventRef, linkConfidence(f), `${f.via} link in text`, extra));
  }
  for (const level of refLevels(post.ref)) {
    for (const r of level) {
      for (const f of findAll(r.text)) {
        out.push(makeSignal("reference", f, actor, eventRef, "medium", `in ${r.type} of ${r.tweetId}`, extra));
      }
    }
  }
  return firstPerMint(out);
}

// tweet/meta carries no author, so the caller passes the author of the stored post.
export function detectMetaSignals(meta: TweetMeta, author: Author | undefined, postKey: string, eventRef: string): Signal[] {
  const actor = xActor(author);
  const out: Signal[] = [];
  for (const token of meta.detected?.tokens ?? []) {
    const contract = token.contract;
    if (!contract || DENYLIST.has(contract) || !isSolanaAddress(contract)) continue;
    // Found in the text is as strong as a CA in the text; found only in an image is weaker.
    const confidence = token.sources?.includes("text") ? "high" : "medium";
    const label = token.symbol ? `$${token.symbol}` : "token";
    out.push(makeSignal("meta", { chain: "solana", address: contract }, actor, eventRef, confidence, `${label} detected`, { postKey }));
  }
  for (const f of findAll(meta.ocr?.text)) out.push(makeSignal("ocr", f, actor, eventRef, "medium", "in image text", { postKey }));
  return firstPerMint(out);
}

// What the post's author did, for the engagement and new tweet modes.
export function postActivity(post: Post, postKey: string, eventRef: string, signals: Signal[]): PostActivity {
  const levels = refLevels(post.ref);
  return {
    type: "post",
    kind: post.kind ?? "post",
    actor: xActor(post.author),
    postKey,
    eventRef,
    target: post.ref ? { tweetId: post.ref.tweetId, account: xActor(post.ref.author) } : null,
    texts: [[post.text ?? ""], ...levels.map((level) => level.map((r) => r.text ?? ""))],
    signals,
  };
}

export function firstPerMint<T extends { mint: string }>(list: T[]): T[] {
  const seen = new Set<string>();
  return list.filter((s) => !seen.has(s.mint) && seen.add(s.mint));
}
