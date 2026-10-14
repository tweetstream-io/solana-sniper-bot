// TweetStream v1 message types (only the fields this bot reads), post keys, event references,
// and publish times from X post ids. Full contract: tweetstream.io/docs/asyncapi.json

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
type AccountRef = Author & { websiteUrl?: string };
type FollowTarget = AccountRef & { handle: string };

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

type MetaToken = {
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
  actor: AccountRef;
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
  actor: AccountRef;
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

type FomoCoin = { address: string; networkId: number; symbol?: string; marketCapUsd?: string };

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

// The stable name of an event, used in logs, dedupe, the latency report, and shared/fixtures:
// "tweet/content twitter:123", "account/follow <eventId>", "account/callout <envelope id>".
// Without an envelope id, account events fall back to an id from the payload; "" means none.
export function eventRef(event: Envelope): string {
  const d = (event.d ?? {}) as {
    tweetId?: string;
    platform?: Platform;
    author?: Author;
    eventId?: string;
    calloutId?: string;
    replyId?: string;
    kind?: string;
    thesis?: { id?: string };
    coin?: { address?: string };
  };
  const pair = `${event.t}/${event.op}`;
  if (event.t === "tweet" && typeof d.tweetId === "string") return `${pair} ${postKey({ ...d, tweetId: d.tweetId })}`;
  const fromPayload =
    pair === "account/callout" || pair === "account/callout_update"
      ? d.calloutId && `${d.calloutId}:${d.kind ?? "callout"}:${d.replyId ?? ""}`
      : pair === "account/thesis"
        ? d.thesis?.id
        : pair === "account/token_verified"
          ? d.coin?.address
          : undefined;
  return `${pair} ${d.eventId ?? event.id ?? fromPayload ?? ""}`;
}

// Checks the fields asyncapi.json marks required for each pair this bot reads, so a malformed
// frame is skipped with a reason instead of throwing deep inside a detector. null means valid.
const POST_KINDS: unknown[] = ["post", "quote", "reply", "retweet"];
// verifiedLabel is an object ({badge, description, url}) or null; every other field is text or null.
const profileValues = (fields: object) =>
  Object.entries(fields).every(([k, v]) => v === null || (k === "verifiedLabel" ? typeof v === "object" && !Array.isArray(v) : typeof v === "string"));

export function payloadProblem(pair: string, d: unknown): string | null {
  const o = d as Record<string, any> | null;
  const str = (v: unknown) => typeof v === "string" && v.length > 0;
  const obj = (v: unknown) => typeof v === "object" && v !== null && !Array.isArray(v);
  if (!obj(o)) return "payload is not an object";
  const need = (ok: boolean, what: string) => (ok ? null : `missing or invalid ${what}`);
  switch (pair) {
    case "tweet/content":
      return need(str(o!.tweetId), "tweetId") ?? need(typeof o!.text === "string", "text") ?? need(obj(o!.author), "author") ?? need(POST_KINDS.includes(o!.kind), "kind");
    case "tweet/update":
      return need(str(o!.tweetId), "tweetId") ?? need(!("kind" in o!) || POST_KINDS.includes(o!.kind), "kind");
    case "tweet/meta":
    case "tweet/delete":
      return need(str(o!.tweetId), "tweetId");
    case "tweet/pin":
      return need(str(o!.tweetId), "tweetId") ?? need(obj(o!.author), "author") ?? need(str(o!.eventId), "eventId");
    case "account/follow":
    case "account/unfollow":
      return need(obj(o!.actor), "actor") ?? need(obj(o!.target) && str(o!.target.handle), "target.handle") ?? need(str(o!.eventId), "eventId");
    case "account/profile_update":
      return (
        need(obj(o!.actor), "actor") ??
        need(obj(o!.changes), "changes") ??
        need(str(o!.eventId), "eventId") ??
        need(profileValues(o!.changes), "changes (text fields a string or null, verifiedLabel an object or null)") ??
        need(o!.previous === undefined || (obj(o!.previous) && profileValues(o!.previous)), "previous (text fields a string or null, verifiedLabel an object or null)")
      );
    case "account/callout":
    case "account/callout_update":
      return need(str(o!.caller?.username), "caller.username") ?? need(str(o!.token?.address), "token.address") ?? need(str(o!.calloutId), "calloutId");
    case "account/thesis":
      return need(str(o!.coin?.address), "coin.address") ?? need(str(o!.author?.id) && str(o!.author?.username), "author") ?? need(str(o!.thesis?.id) && typeof o!.thesis?.text === "string", "thesis");
    case "account/token_verified":
      return need(str(o!.coin?.address), "coin.address");
    default:
      return null;
  }
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
