// TweetStream v1 message types (only the fields this bot reads), post keys, event references,
// and publish times from X post ids. Full contract: https://tweetstream.io/docs/asyncapi.json

// Every server message is a v1 envelope. Route on t (channel) and op (operation); d is the payload.
export type Envelope = { v: 1; t: string; op: string; id?: string; ts: number; d: unknown };

export type Platform = "twitter" | "truth_social" | "binance_square";

// Handles arrive with a leading "@" in some events. Always key rules by id, never by handle.
export type Author = {
  id?: string;
  handle?: string;
  name?: string;
  bio?: string;
  platform?: Platform;
  url?: string;
};

// account/* actors and follow targets also carry the profile website.
export type Actor = Author & { websiteUrl?: string };
export type FollowTarget = Actor & { handle: string };

export type TweetUrl = { url: string; tco?: string; name?: string };

// A post that this one quotes, replies to, or reposts. It can nest through quoted or subtweet.
export type Reference = {
  type: "quote" | "reply" | "retweet";
  tweetId: string;
  author?: Author;
  text?: string;
  quoted?: Reference;
  subtweet?: Reference;
};

export type PostKind = "post" | "quote" | "reply" | "retweet";

// tweet/content
export type TweetContent = {
  tweetId: string;
  text: string;
  createdAt: number;
  kind: PostKind;
  author: Author;
  ref?: Reference;
  urls?: TweetUrl[];
  receivedAt?: number;
};

// tweet/update: only the fields that changed. textUpdateType "completion" is the full text
// of a post that first arrived shortened.
export type TweetUpdate = Partial<Omit<TweetContent, "createdAt" | "tweetId">> & {
  tweetId: string;
  platform?: Platform;
  textUpdateType?: "completion";
};

export type MetaToken = {
  chain?: "solana";
  contract?: string;
  name?: string;
  symbol?: string;
  networkId?: number;
  priceUsd?: number;
  sources?: Array<"ocr" | "text">;
};

// tweet/meta: a complete snapshot. Replace the stored one, never merge.
export type TweetMeta = {
  tweetId: string;
  platform?: Platform;
  detected?: { tokens?: MetaToken[] };
  ocr?: { text: string };
};

export type TweetDelete = {
  tweetId: string;
  platform?: Platform;
  author?: Author;
  eventId?: string;
  deletedAt?: number;
  text?: string;
};

export type TweetPin = {
  action: "pin";
  tweetId: string;
  platform?: Platform;
  author: Author;
  eventId: string;
  observedAt: number;
  text?: string;
  tweet?: TweetContent;
};

export type AccountFollow = {
  actor: Actor;
  target: FollowTarget;
  eventId: string;
  kind: "FOLLOW";
  observedAt: number;
};

export type ProfileFields = {
  avatar?: string;
  banner?: string;
  bio?: string;
  handle?: string;
  location?: string;
  name?: string;
  websiteUrl?: string | null;
};

export type AccountProfileUpdate = {
  actor: Actor;
  changes: ProfileFields;
  previous?: ProfileFields;
  eventId: string;
  kind: "PROFILE";
  observedAt: number;
};

// account/callout and account/callout_update. caller.username is a pump.fun name. Decimal values
// arrive as strings to keep their precision.
export type AccountCallout = {
  platform: "pump_fun";
  caller: { username: string };
  token: { address: string; chainId?: string; symbol?: string; name?: string };
  calloutId: string;
  createdAt: number;
  kind?: "callout" | "author_update" | "reply" | "repost";
  marketCapUsd?: number;
  multiple?: number;
  position?: { amountHeld?: string };
  updateOf?: string; // callout_update only: the envelope id of the original callout
};

export type FomoCoin = { address: string; networkId: number; symbol?: string; marketCapUsd?: string };

// account/thesis: a FOMO user's thesis on a coin.
export type AccountThesis = {
  platform: "fomo";
  coin: FomoCoin;
  author: { id: string; username: string; verified: boolean };
  thesis: { id: string; text: string; createdAt: number };
  position?: { closedAt?: number };
};

// account/token_verified: the first observed verification of a tracked coin.
export type AccountTokenVerified = { platform: "fomo"; coin: FomoCoin; observedAt: number };

// Posts are keyed by (platform, tweetId). Content carries the platform on its author;
// later operations carry it on d.platform. A missing platform means twitter.
export function postKey(d: { tweetId: string; platform?: Platform; author?: Author }): string {
  return `${d.platform ?? d.author?.platform ?? "twitter"}:${d.tweetId}`;
}

export function normalizeHandle(handle: string | undefined): string | undefined {
  return handle?.trim().replace(/^@/, "") || undefined;
}

// The stable name of an event, used in logs, the latency report, and shared/fixtures:
// "tweet/content twitter:123", "account/follow <eventId>", "account/callout <envelope id>".
export function eventRef(event: Envelope): string {
  const d = (event.d ?? {}) as { tweetId?: string; platform?: Platform; author?: Author; eventId?: string };
  const pair = `${event.t}/${event.op}`;
  if (event.t === "tweet" && typeof d.tweetId === "string") return `${pair} ${postKey({ ...d, tweetId: d.tweetId })}`;
  return `${pair} ${d.eventId ?? event.id ?? ""}`;
}

const TWITTER_EPOCH_MS = 1288834974657n;

// X post ids are 64-bit snowflakes: (id >> 22) + the Twitter epoch is the publish time in ms.
// A JavaScript Number holds 53 bits, so Number("1849...") silently rounds the id; use BigInt.
export function snowflakeMs(tweetId: string): number | null {
  if (!/^\d{1,20}$/.test(tweetId)) return null;
  const id = BigInt(tweetId);
  if (id >= 1n << 64n) return null;
  return Number((id >> 22n) + TWITTER_EPOCH_MS);
}

// Publish time of a post: from the id on X, from createdAt elsewhere.
export function publishedAtMs(platform: string, tweetId: string, createdAt: number | undefined): number | null {
  const fromId = platform === "twitter" ? snowflakeMs(tweetId) : null;
  return fromId ?? (typeof createdAt === "number" ? createdAt : null);
}
