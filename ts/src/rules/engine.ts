// Matches one activity against the rules that listen to its account, then applies the limits
// that need memory: fire once per post or event, cooldown, and max snipes. Synchronous: no await
// happens between "this rule fires" and "the slot is taken". Next: filters/gate.ts.

import type { Config, Rule } from "../setup/config.ts";
import type { Activity } from "../detect/types.ts";
import type { Ledger } from "./ledger.ts";
import type { RuleIndex } from "./index.ts";
import { matchCallout, matchCaScanner, matchEngagement, matchFollow, matchFomo, matchNewTweet, matchPfp, type Exit, type Match, type Result, type Skip } from "./modes.ts";

type EvalContext = {
  now: number;
  ageMs: number | null; // publish -> receipt age of the post, when known
  knownBio: string | undefined; // last bio seen for the acting account
};

const MAX_FIRED_KEYS = 100_000;

export class RuleEngine {
  private readonly fired = new Set<string>();
  private readonly lastFire = new Map<string, number>(); // rule name -> time of last trigger

  private readonly index: RuleIndex;
  private readonly ledger: Ledger;
  private readonly config: Pick<Config, "maxSignalAgeMs">;

  constructor(index: RuleIndex, ledger: Ledger, config: Pick<Config, "maxSignalAgeMs">) {
    this.index = index;
    this.ledger = ledger;
    this.config = config;
  }

  evaluate(activity: Activity, ctx: EvalContext): { triggers: Match[]; skips: Skip[]; exits: Exit[] } {
    const triggers: Match[] = [];
    const skips: Skip[] = [];
    const exits: Exit[] = [];
    const rules = this.index.candidates(activity);
    if (!rules.length) return { triggers, skips, exits };
    if (ctx.ageMs !== null && ctx.ageMs > this.config.maxSignalAgeMs) {
      const detail = `post is ${ctx.ageMs} ms old, maxSignalAgeMs is ${this.config.maxSignalAgeMs}`;
      for (const rule of rules) skips.push({ kind: "skip", rule, code: "stale_post", detail });
      return { triggers, skips, exits };
    }
    for (const rule of rules) {
      for (const result of this.match(rule, activity, ctx)) {
        if (result.kind === "skip") skips.push(result);
        else if (result.kind === "exit") exits.push(result);
        else this.admit(result, ctx.now, triggers, skips);
      }
    }
    return { triggers, skips, exits };
  }

  private match(rule: Rule, activity: Activity, ctx: EvalContext): Array<Match | Skip | Exit> {
    const one = (r: Result) => (r ? [r] : []);
    switch (rule.mode) {
      case "ca_scanner":
        return matchCaScanner(rule, activity, ctx.knownBio);
      case "engagement":
        return activity.type === "post" ? one(matchEngagement(rule, activity)) : [];
      case "new_tweet":
        return activity.type === "post" || activity.type === "profile" ? one(matchNewTweet(rule, activity)) : [];
      case "follow":
        return activity.type === "follow" ? one(matchFollow(rule, activity)) : [];
      case "pfp_update":
        return activity.type === "profile" ? one(matchPfp(rule, activity)) : [];
      case "callout":
        return activity.type === "callout" ? matchCallout(rule, activity) : [];
      case "fomo":
        return activity.type === "fomo" ? matchFomo(rule, activity) : [];
    }
  }

  private admit(match: Match, now: number, triggers: Match[], skips: Skip[]): void {
    const { rule } = match;
    if (this.fired.has(match.fireKey)) return; // the same post or event again: already handled
    const last = this.lastFire.get(rule.name);
    const cooldownMs = rule.filters.cooldownSec * 1000;
    if (last !== undefined && now - last < cooldownMs) {
      skips.push({ kind: "skip", rule, code: "cooldown", detail: `${Math.ceil((cooldownMs - (now - last)) / 1000)} s left` });
      return;
    }
    if (!this.ledger.tryReserveSnipe(rule.name, rule.filters.maxSnipes)) {
      skips.push({ kind: "skip", rule, code: "max_snipes", detail: `maxSnipes ${rule.filters.maxSnipes} reached` });
      return;
    }
    this.fired.add(match.fireKey);
    if (this.fired.size > MAX_FIRED_KEYS) this.fired.delete(this.fired.values().next().value!);
    this.lastFire.set(rule.name, now);
    triggers.push(match);
  }
}
