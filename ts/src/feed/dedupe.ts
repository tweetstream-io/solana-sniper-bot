// First copy wins across connections. With an Ultra Speed key and a standard key the same event
// can arrive twice; the second copy is dropped before detection. Next: pipeline.ts routes the event.

import { eventRef, type Envelope } from "../protocol.ts";

// One post has one content and one delete; pins and account events carry a unique eventId
// or calloutId. tweet/update and tweet/meta can repeat for a post, so they are keyed by
// envelope id when present and otherwise pass through; rules fire once per post anyway.
function dedupeKey(event: Envelope): string | null {
  const pair = `${event.t}/${event.op}`;
  if (pair === "tweet/update" || pair === "tweet/meta") return event.id ? `${pair} id:${event.id}` : null;
  const eventId = (event.d as { eventId?: unknown } | null)?.eventId;
  return typeof eventId === "string" ? `${pair} ${eventId}` : eventRef(event);
}

export class EventDedupe {
  private readonly seen = new Set<string>();
  private readonly capacity: number;

  constructor(capacity = 50_000) {
    this.capacity = capacity;
  }

  first(event: Envelope): boolean {
    const key = dedupeKey(event);
    if (key === null) return true;
    if (this.seen.has(key)) return false;
    this.seen.add(key);
    if (this.seen.size > this.capacity) this.seen.delete(this.seen.values().next().value!);
    return true;
  }
}
