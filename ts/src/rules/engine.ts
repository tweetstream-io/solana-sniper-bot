// Matches one activity against the rules that listen to its account, then applies the limits
// that need memory: fire once per key, cooldown, and max snipes. Synchronous: no await happens
// between "this rule fires" and "the slot is taken". Next: execute/trade.ts runs each fire.

import type { Config } from "../setup/config.ts";
import type { Activity } from "../detect/types.ts";
import type { TradeBook } from "../execute/trades.ts";
import type { RuleIndex } from "./index.ts";
import { MODES, type Fire, type Skip } from "./modes.ts";

type EvalContext = {
  now: number;
  ageMs: number | null; // publish -> receipt age of the post, when known
  knownBio: string | undefined; // last bio seen for the acting account
};

const MAX_FIRED_KEYS = 100_000;

export class RuleEngine {
  private readonly fired = new Set<string>();
  private readonly lastFire = new Map<string, number>(); // rule name -> time of last fire
  private readonly index: RuleIndex;
  private readonly book: TradeBook;
  private readonly config: Pick<Config, "maxSignalAgeMs">;

  constructor(index: RuleIndex, book: TradeBook, config: Pick<Config, "maxSignalAgeMs">) {
    this.index = index;
    this.book = book;
    this.config = config;
  }

  evaluate(activity: Activity, ctx: EvalContext): { fires: Fire[]; skips: Skip[] } {
    const fires: Fire[] = [];
    const skips: Skip[] = [];
    const stale = ctx.ageMs !== null && ctx.ageMs > this.config.maxSignalAgeMs;
    for (const rule of this.index.candidates(activity)) {
      const decisions = MODES[rule.mode].match(rule, activity, ctx.knownBio);
      // A late post is reported only to rules it concerns, once per rule.
      if (stale && decisions.length) {
        skips.push({ kind: "skip", rule, code: "stale_post", detail: `post is ${ctx.ageMs} ms old, maxSignalAgeMs is ${this.config.maxSignalAgeMs}` });
        continue;
      }
      for (const r of decisions) {
        if (r.kind === "skip") skips.push(r);
        else this.admit(r, ctx.now, fires, skips);
      }
    }
    return { fires, skips };
  }

  // Undo a fire that sent nothing (filters or a limit refused it): the rule may fire again on the
  // same key and gets its snipe back. Called at most once per fire.
  release(fire: Fire): void {
    this.fired.delete(fire.fireKey);
    this.book.releaseSnipe(fire.rule.name);
  }

  private admit(fire: Fire, now: number, fires: Fire[], skips: Skip[]): void {
    const { rule } = fire;
    if (this.fired.has(fire.fireKey)) return; // the same post or event again: already handled
    const last = this.lastFire.get(rule.name);
    const cooldownMs = rule.filters.cooldownSec * 1000;
    if (last !== undefined && now - last < cooldownMs) {
      skips.push({ kind: "skip", rule, code: "cooldown", detail: `${Math.ceil((cooldownMs - (now - last)) / 1000)} s left` });
      return;
    }
    if (!this.book.claimSnipe(rule.name, rule.filters.maxSnipes)) {
      skips.push({ kind: "skip", rule, code: "max_snipes", detail: `maxSnipes ${rule.filters.maxSnipes} reached` });
      return;
    }
    this.fired.add(fire.fireKey);
    if (this.fired.size > MAX_FIRED_KEYS) this.fired.delete(this.fired.values().next().value!);
    this.lastFire.set(rule.name, now);
    fires.push(fire);
  }
}
