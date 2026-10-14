// What the detectors produce. A Signal is one contract address found in one event; an Activity
// is what an account did (posted, followed, changed its profile) plus the signals it carried.
// Next: rules/engine.ts matches activities against the configured rules.

import type { Chain } from "./addresses.ts";
import type { PostKind } from "../protocol.ts";

export type Confidence = "high" | "medium" | "low";

// Where the address was found. These names are the "sources" a CA scanner rule can allow.
export type SignalSource =
  | "text" // written in the post text
  | "url" // a token link in the post's URLs or text
  | "meta" // detected by TweetStream enrichment (tweet/meta detected.tokens)
  | "ocr" // read from an image (tweet/meta ocr.text)
  | "reference" // inside the post that was quoted, replied to, or reposted
  | "pin" // in a newly pinned post
  | "profile" // newly added to the account's bio, name, or website
  | "follow_bio" // in the bio of an account that was just followed
  | "callout" // the token of an account/callout event
  | "fomo"; // the coin of an account/thesis or account/token_verified event

// Who acted. Post authors are qualified by platform: X accounts are "x", and Truth Social or Binance
// Square authors keep their platform name, so an X rule never matches them by a shared id.
// pump.fun callers have a username, FOMO authors an id and a username, verifications no actor.
export type Actor = {
  namespace: "x" | "truth_social" | "binance_square" | "pump_fun" | "fomo" | "none";
  id?: string;
  handle?: string;
};

export type Signal = {
  source: SignalSource;
  chain: Chain;
  mint: string;
  actor: Actor;
  eventRef: string; // "tweet/content twitter:123", for logs and the latency report
  confidence: Confidence;
  detail: string;
  postKey?: string; // the post that carried it, so deleting that post can trigger an exit
  marketCapUsd?: number; // only when the event itself carries one
};

export type PostActivity = {
  type: "post";
  kind: PostKind;
  actor: Actor;
  postKey: string;
  eventRef: string;
  // The post this one quotes, replies to, or reposts (the direct reference only).
  target: { tweetId: string; account: Actor } | null;
  // texts[0] is the post's own text, texts[1] the referenced post, texts[2] one level deeper.
  texts: string[][];
  signals: Signal[];
};

export type FollowActivity = {
  type: "follow";
  op: "follow" | "unfollow";
  actor: Actor;
  target: Actor;
  eventRef: string;
  bioInPayload: boolean;
  signals: Signal[]; // CAs in the target's bio
};

export type ProfileField = "avatar" | "banner" | "name" | "bio" | "websiteUrl";

export type ProfileActivity = {
  type: "profile";
  actor: Actor;
  eventRef: string;
  changed: ProfileField[];
  bio: string | null; // the new bio, when it changed
  signals: Signal[]; // CAs newly added to bio, name, or website
};

// A pin only carries addresses; only the CA scanner reads it.
export type PinActivity = { type: "pin"; actor: Actor; eventRef: string; signals: Signal[] };

export type CalloutActivity = {
  type: "callout";
  actor: Actor;
  eventRef: string;
  kind: "callout" | "author_update" | "reply" | "repost";
  update: boolean; // account/callout_update: enrichment of an earlier callout, never a new call
  signals: Signal[];
  multiple: number | null;
  callerHolds: boolean | null; // from position.amountHeld; null when the event has no position
};

export type FomoActivity = {
  type: "fomo";
  op: "thesis" | "token_verified";
  actor: Actor; // namespace "none" for token_verified
  coin: string;
  eventRef: string;
  text: string; // thesis text; empty for token_verified
  signals: Signal[];
  authorExited: boolean; // the thesis position reports closedAt
};

export type Activity = PostActivity | FollowActivity | ProfileActivity | PinActivity | CalloutActivity | FomoActivity;
