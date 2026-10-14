// Detectors for account events: follow/unfollow, profile updates, pins, pump.fun callouts, and FOMO
// theses and token verifications. Pure and synchronous. Next: rules/engine.ts.

import type { AccountCallout, AccountFollow, AccountProfileUpdate, AccountThesis, AccountTokenVerified, ProfileFields, TweetPin } from "../protocol.ts";
import { normalizeHandle } from "../protocol.ts";
import { addressesFromUrl, classifyAddress, findAddresses, findAll, type FoundAddress } from "./addresses.ts";
import { accountActor, firstPerMint, makeSignal } from "./post.ts";
import type { Actor, CalloutActivity, Confidence, FollowActivity, FomoActivity, PinActivity, ProfileActivity, ProfileField } from "./types.ts";

// account/follow and account/unfollow. The target's bio is used only when the payload has it;
// this example does not call a profile lookup API.
export function followActivity(d: AccountFollow, op: "follow" | "unfollow", eventRef: string): FollowActivity {
  const actor = accountActor(d.actor, "twitter");
  const target = accountActor(d.target, d.actor.platform ?? "twitter");
  const found = [...findAll(d.target.bio), ...(d.target.websiteUrl ? addressesFromUrl(d.target.websiteUrl) : [])];
  const signals = firstPerMint(
    found.map((f) => makeSignal("follow_bio", f, actor, eventRef, "medium", `in bio of @${target.handle}`)),
  );
  return { type: "follow", op, actor, target, eventRef, bioInPayload: d.target.bio !== undefined, signals };
}

const PROFILE_FIELDS: ProfileField[] = ["avatar", "banner", "name", "bio", "websiteUrl"];
const CA_FIELDS = ["bio", "name", "websiteUrl"] as const;

// account/profile_update. A field's baseline is the payload's `previous` value when it has one,
// else the value this process saw last for the account. The same baseline decides which fields
// changed and which addresses are new.
export function profileActivity(d: AccountProfileUpdate, known: ProfileFields | undefined, eventRef: string): ProfileActivity {
  const actor = accountActor(d.actor, "twitter");
  const baseline = (field: ProfileField) => {
    if (d.previous !== undefined && field in d.previous) return { known: true, value: d.previous[field] };
    return { known: known?.[field] !== undefined, value: known?.[field] };
  };
  const changed = PROFILE_FIELDS.filter((f) => f in d.changes && d.changes[f] !== baseline(f).value);
  const signals = [];
  for (const field of CA_FIELDS) {
    if (!(field in d.changes)) continue;
    const prior = baseline(field);
    const old = new Set(fieldAddresses(field, prior.value).map((f) => f.address));
    for (const f of fieldAddresses(field, d.changes[field])) {
      if (old.has(f.address)) continue;
      // Without a baseline the address may have been there all along, so trust it less.
      const confidence: Confidence = !prior.known ? "low" : field === "name" ? "medium" : "high";
      signals.push(makeSignal("profile", f, actor, eventRef, confidence, `added to ${field}${prior.known ? "" : " (no earlier value seen)"}`));
    }
  }
  const bio = changed.includes("bio") ? (d.changes.bio ?? "") : null;
  return { type: "profile", actor, eventRef, changed, bio, signals: firstPerMint(signals) };
}

function fieldAddresses(field: (typeof CA_FIELDS)[number], value: string | null | undefined): FoundAddress[] {
  if (!value) return [];
  return field === "websiteUrl" ? [...addressesFromUrl(value), ...findAddresses(value)] : findAll(value);
}

// tweet/pin. A pin event means the post was just pinned; read its text and links.
export function pinActivity(d: TweetPin, postKey: string, eventRef: string): PinActivity {
  const actor = accountActor(d.author, d.platform ?? "twitter");
  const found = [...(d.tweet?.urls ?? []).flatMap((u) => addressesFromUrl(u.url)), ...findAll(d.tweet?.text ?? d.text)];
  const signals = firstPerMint(found.map((f) => makeSignal("pin", f, actor, eventRef, "high", "in pinned post", { postKey })));
  return { type: "pin", actor, eventRef, signals };
}

function tokenSignal(source: "callout" | "fomo", address: string, actor: Actor, eventRef: string, detail: string, marketCapUsd: number | undefined) {
  const hit = classifyAddress(address);
  return hit ? [makeSignal(source, hit, actor, eventRef, "high", detail, { marketCapUsd })] : [];
}

// account/callout and account/callout_update. An update enriches an earlier callout; rules never
// treat it as a new call. position.amountHeld is a decimal string; "0" means the caller holds none.
export function calloutActivity(d: AccountCallout, update: boolean, eventRef: string): CalloutActivity {
  const actor: Actor = { namespace: "pump_fun", handle: d.caller.username };
  const held = d.position?.amountHeld;
  return {
    type: "callout",
    actor,
    eventRef,
    kind: d.kind ?? "callout",
    update,
    signals: tokenSignal("callout", d.token.address, actor, eventRef, `${d.kind ?? "callout"} ${d.token.symbol ?? ""}`.trim(), d.marketCapUsd),
    multiple: d.multiple ?? null,
    callerHolds: held === undefined ? null : Number(held) > 0,
  };
}

// account/thesis and account/token_verified. Market values are decimal strings observed at
// metadataObservedAt; they describe that moment, not a live quote.
export function fomoActivity(op: "thesis" | "token_verified", d: AccountThesis | AccountTokenVerified, eventRef: string): FomoActivity {
  const thesis = op === "thesis" ? (d as AccountThesis) : null;
  const actor: Actor = thesis ? { namespace: "fomo", id: thesis.author.id, handle: thesis.author.username } : { namespace: "none" };
  const cap = d.coin.marketCapUsd === undefined ? undefined : Number(d.coin.marketCapUsd);
  const detail = `${op === "thesis" ? "thesis on" : "verified"} ${d.coin.symbol ?? "coin"}`;
  return {
    type: "fomo",
    op,
    actor,
    coin: d.coin.address,
    eventRef,
    text: thesis?.thesis.text ?? "",
    signals: tokenSignal("fomo", d.coin.address, actor, eventRef, detail, Number.isFinite(cap) ? cap : undefined),
    authorExited: thesis?.position?.closedAt !== undefined,
  };
}
