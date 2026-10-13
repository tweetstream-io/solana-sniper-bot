// The bot's local view of each post and each account profile. tweet/update patches the stored
// post, tweet/meta replaces its snapshot. Next: detect/post.ts reads the merged post.

import type { Author, ProfileFields, Reference, TweetContent, TweetMeta, TweetUpdate, TweetUrl } from "../protocol.ts";

export type Post = {
  tweetId: string;
  text?: string;
  kind?: TweetContent["kind"];
  author?: Author;
  ref?: Reference;
  urls?: TweetUrl[];
  createdAt?: number;
};

export type Entry = { post: Post | null; meta: TweetMeta | null };

const MAX_POSTS = 20_000;
const MAX_PROFILES = 20_000;

export class PostStore {
  private readonly posts = new Map<string, Entry>();
  private readonly profiles = new Map<string, ProfileFields>(); // account id -> last known fields

  get(key: string): Entry | undefined {
    return this.posts.get(key);
  }

  // tweet/content. An update that arrived before its content still wins for the fields it set.
  applyContent(key: string, d: TweetContent): Post {
    const entry = this.entry(key);
    const { receivedAt: _received, ...content } = d;
    entry.post = mergePost(content, entry.post);
    this.rememberAuthor(d.author);
    return entry.post;
  }

  // tweet/update: patch only the fields present. A ref in the update replaces the stored ref.
  applyUpdate(key: string, d: TweetUpdate): Post {
    const entry = this.entry(key);
    const { platform: _platform, textUpdateType: _type, receivedAt: _received, ...patch } = d;
    entry.post = mergePost(entry.post ?? { tweetId: d.tweetId }, patch);
    return entry.post;
  }

  // tweet/meta is a complete snapshot: replace, never merge.
  applyMeta(key: string, d: TweetMeta): Entry {
    const entry = this.entry(key);
    entry.meta = d;
    return entry;
  }

  profile(accountId: string | undefined): ProfileFields | undefined {
    return accountId ? this.profiles.get(accountId) : undefined;
  }

  updateProfile(accountId: string | undefined, fields: ProfileFields): void {
    if (!accountId) return;
    const merged = { ...this.profiles.get(accountId), ...defined(fields) };
    this.profiles.delete(accountId); // re-insert so the newest profile is evicted last
    this.profiles.set(accountId, merged);
    if (this.profiles.size > MAX_PROFILES) this.profiles.delete(this.profiles.keys().next().value!);
  }

  // Posts carry the author's current bio and name, which seeds the profile diff baseline.
  private rememberAuthor(author: Author | undefined): void {
    if (!author?.id) return;
    this.updateProfile(author.id, { bio: author.bio, name: author.name, handle: author.handle });
  }

  private entry(key: string): Entry {
    let entry = this.posts.get(key);
    if (!entry) {
      entry = { post: null, meta: null };
      this.posts.set(key, entry);
      if (this.posts.size > MAX_POSTS) this.posts.delete(this.posts.keys().next().value!); // bound memory
    }
    return entry;
  }
}

function mergePost(base: Post, patch: Partial<Post> | null): Post {
  if (!patch) return base;
  return { ...base, ...defined(patch), author: { ...base.author, ...defined(patch.author ?? {}) } };
}

// Spreading an object with undefined values would erase stored fields.
function defined<T extends object>(value: T): Partial<T> {
  return Object.fromEntries(Object.entries(value).filter(([, v]) => v !== undefined)) as Partial<T>;
}
