// First copy wins across connections. With an Ultra Speed key and a standard key the same event
// arrives twice; the second copy is dropped before detection. Next: pipeline.ts routes the event.

import { createHash } from "node:crypto";
import type { Envelope } from "../protocol.ts";

// An event repeats when its type, op, and payload repeat. The envelope is no help: the two copies
// differ in ts, and every tweet/update and tweet/meta of a post carries the same id (the tweetId).
function dedupeKey(event: Envelope): string {
  return createHash("sha256").update(`${event.t}/${event.op}\n${canonical(event.d)}`).digest("base64");
}

// JSON with object keys sorted, so key order on the wire does not matter.
function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value !== null && typeof value === "object") {
    const entries = Object.entries(value).filter(([, v]) => v !== undefined);
    return `{${entries.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)).map(([k, v]) => `${JSON.stringify(k)}:${canonical(v)}`).join(",")}}`;
  }
  return JSON.stringify(value) ?? "null";
}

export class EventDedupe {
  private readonly seen = new Set<string>();
  private readonly capacity: number;

  constructor(capacity = 50_000) {
    this.capacity = capacity;
  }

  first(event: Envelope): boolean {
    const key = dedupeKey(event);
    if (this.seen.has(key)) return false;
    this.seen.add(key);
    if (this.seen.size > this.capacity) this.seen.delete(this.seen.values().next().value!);
    return true;
  }
}
