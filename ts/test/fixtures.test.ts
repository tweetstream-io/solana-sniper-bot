// Runs every case in shared/fixtures through the same code the bot uses and compares the result
// with the hand-written expectations. The Rust bot runs the same files.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { ConfigError, parseConfig } from "../src/setup/config.ts";
import { TradeBook } from "../src/execute/trades.ts";
import { Decider, decisionsJson, signalsJson } from "../src/pipeline.ts";
import type { Envelope } from "../src/protocol.ts";
import type { Actor } from "../src/detect/types.ts";

type EventFixture = {
  name: string;
  config?: unknown;
  positions?: Array<{ mint: string; postKey?: string; rule: string; actor: Actor }>;
  events: Array<{ connection: string; receivedAtMs: number; envelope: Envelope }>;
  expectedSignals?: unknown[];
  expectedDecisions?: unknown[];
};

const dir = fileURLToPath(new URL("../../shared/fixtures/", import.meta.url));
const json = (path: string) => JSON.parse(readFileSync(path, "utf8"));

for (const file of readdirSync(dir).filter((f) => f.endsWith(".json")).sort()) {
  const fixture = json(dir + file) as EventFixture;
  test(`fixture ${fixture.name}`, () => {
    const config = parseConfig(fixture.config ?? { rules: [{ name: "none", mode: "ca_scanner", accounts: ["1"] }] });
    const book = TradeBook.open(null, "dry_run");
    for (const p of fixture.positions ?? []) {
      const trigger = { rule: p.rule, fireKey: "", eventRef: "", postKey: p.postKey, actor: p.actor };
      book.openPosition({ mint: p.mint, route: "pump", tokenProgram: "", openedAt: 0, buyTradeId: "", costLamports: "0", tokenAmount: null, trigger });
    }
    const decider = new Decider(config, book);
    const signals: unknown[] = [];
    const decisions: unknown[] = [];
    for (const e of fixture.events) {
      const outcome = decider.handle(e.envelope, { wallMs: e.receivedAtMs });
      signals.push(...signalsJson(outcome));
      decisions.push(...decisionsJson(outcome));
    }
    if (fixture.expectedSignals) assert.deepEqual(signals, fixture.expectedSignals);
    if (fixture.expectedDecisions) assert.deepEqual(decisions, fixture.expectedDecisions);
  });
}

type ConfigFixture = { name: string; config: unknown; expected: { values: Record<string, unknown> } | { error: { rule: string | null; field: string | null } } };

const at = (value: unknown, path: string) => path.split(".").reduce<unknown>((v, k) => (v as Record<string, unknown>)?.[k], value);

for (const file of readdirSync(`${dir}config`).sort()) {
  const fixture = json(`${dir}config/${file}`) as ConfigFixture;
  test(`config fixture ${fixture.name}`, () => {
    if ("values" in fixture.expected) {
      const config = parseConfig(fixture.config);
      for (const [path, value] of Object.entries(fixture.expected.values)) assert.deepEqual(at(config, path), value, path);
      return;
    }
    try {
      parseConfig(fixture.config);
    } catch (error) {
      assert.ok(error instanceof ConfigError, String(error));
      assert.deepEqual({ rule: error.rule, field: error.field }, fixture.expected.error);
      return;
    }
    assert.fail("expected a config error");
  });
}
