// First copy wins across connections. With an Ultra Speed key and a standard key the same event
// arrives twice; the second copy is dropped before detection. Next: pipeline.ts routes the event.

import { createHash } from "node:crypto";
import type { Envelope } from "../protocol.ts";

// An event repeats when its type, op, and payload repeat, timing fields aside. The envelope is no
// help: copies differ in ts, and every tweet/update and tweet/meta of a post has the same id.
function dedupeKey(event: Envelope): string {
  return createHash("sha256").update(`${event.t}/${event.op}\n${canonical(event.d)}`).digest("base64");
}

// Timing values that may differ between copies of the same event, at any depth.
const TIMING = new Set(["receivedAt", "observedAt", "deletedAt"]);

// JSON with object keys sorted and timing fields left out, so neither key order nor timing makes
// two copies look different.
function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value !== null && typeof value === "object") {
    const entries = Object.entries(value).filter(([k, v]) => v !== undefined && !TIMING.has(k));
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
