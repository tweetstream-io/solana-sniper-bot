// Rule lookup by account, built once at startup so matching an event costs a few Map lookups
// however many rules there are. Next: rules/engine.ts runs the matchers for the rules found here.

import type { Mode, Rule } from "../setup/config.ts";
import type { Activity, Actor } from "../detect/types.ts";

// X accounts are matched by numeric id ("id:") or handle ("h:"), pump.fun callers by username
// ("pf:"), FOMO authors by id or username ("fu:"), FOMO coins by address ("fc:"). Names compare
// without case.
function xSpecKey(spec: string): string {
  const value = spec.trim();
  return /^\d+$/.test(value) ? `id:${value}` : `h:${value.replace(/^@/, "").toLowerCase()}`;
}

function actorKeys(actor: Actor): string[] {
  const keys: string[] = [];
  const prefix = { x: "", pump_fun: "pf:", fomo: "fu:", none: "" }[actor.namespace];
  if (actor.namespace === "x") {
    if (actor.id) keys.push(`id:${actor.id}`);
    if (actor.handle) keys.push(`h:${actor.handle.toLowerCase()}`);
  } else if (prefix) {
    if (actor.id) keys.push(`${prefix}${actor.id.toLowerCase()}`);
    if (actor.handle) keys.push(`${prefix}${actor.handle.toLowerCase()}`);
  }
  return keys;
}

export function matchesAccount(spec: string, actor: Actor): boolean {
  return actor.namespace === "x" && actorKeys(actor).includes(xSpecKey(spec));
}

// Which activity types each mode listens to, and under which keys.
function listenKeys(rule: Rule): Array<[Activity["type"], string]> {
  const x = (types: Array<Activity["type"]>, specs: string[]) => types.flatMap((t) => specs.map((s): [Activity["type"], string] => [t, xSpecKey(s)]));
  const modes: Record<Mode, () => Array<[Activity["type"], string]>> = {
    ca_scanner: () => x(["post", "profile", "pin"], rule.accounts),
    engagement: () => x(["post"], [rule.reactor]),
    new_tweet: () => x(["post", "profile"], rule.accounts),
    follow: () => x(["follow"], rule.accounts),
    pfp_update: () => x(["profile"], rule.accounts),
    callout: () => rule.callers.map((c) => ["callout", `pf:${c.toLowerCase()}`]),
    fomo: () => [...rule.fomoUsers.map((u): [Activity["type"], string] => ["fomo", `fu:${u.toLowerCase()}`]), ...rule.fomoCoins.map((c): [Activity["type"], string] => ["fomo", `fc:${c}`])],
  };
  return modes[rule.mode]();
}

export class RuleIndex {
  private readonly byKey = new Map<string, Rule[]>();
  private readonly order: Map<Rule, number>;

  constructor(rules: Rule[]) {
    this.order = new Map(rules.map((rule, i) => [rule, i]));
    for (const rule of rules.filter((r) => r.enabled)) {
      for (const [type, key] of listenKeys(rule)) {
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
