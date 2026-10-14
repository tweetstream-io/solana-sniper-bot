// Rule lookup by account, built once at startup from the MODES table, so matching an event costs a
// few Map lookups however many rules there are. Next: rules/engine.ts runs the matchers found here.

import type { Rule } from "../setup/config.ts";
import type { Activity } from "../detect/types.ts";
import { actorKeys, MODES } from "./modes.ts";

export class RuleIndex {
  private readonly byKey = new Map<string, Rule[]>();
  private readonly order: Map<Rule, number>;

  constructor(rules: Rule[]) {
    this.order = new Map(rules.map((rule, i) => [rule, i]));
    for (const rule of rules.filter((r) => r.enabled)) {
      for (const [type, key] of MODES[rule.mode].listens(rule)) {
        const list = this.byKey.get(`${type}|${key}`) ?? [];
        if (!list.includes(rule)) list.push(rule);
        this.byKey.set(`${type}|${key}`, list);
      }
    }
  }

  candidates(activity: Activity): Rule[] {
    const keys = actorKeys(activity.actor);
    if (activity.type === "fomo") keys.push(`fc:${activity.coin}`);
    const found = new Set<Rule>();
    for (const key of keys) for (const rule of this.byKey.get(`${activity.type}|${key}`) ?? []) found.add(rule);
    // Config order, so decisions come out in the same order in every implementation.
    return [...found].sort((a, b) => this.order.get(a)! - this.order.get(b)!);
  }
}
