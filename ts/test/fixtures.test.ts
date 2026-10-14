// Runs every case in shared/fixtures through the same code the bot uses and compares the result
// with the hand-written expectations. The Rust bot runs the same files.

import { test } from "node:test";
import assert from "node:assert/strict";
import { copyFileSync, mkdtempSync, readdirSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { ConfigError, parseConfig } from "../src/setup/config.ts";
import { TradeBook } from "../src/execute/trades.ts";
import { Decider, decisionsJson, signalsJson } from "../src/pipeline.ts";
import type { Envelope } from "../src/protocol.ts";

type EventFixture = {
  name: string;
  config?: unknown;
  positions?: Array<{ mint: string; postKey?: string; rule: string }>;
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
    // Positions held before the first event, recorded the way a dry-run buy records them.
    const caps = { maxPerTradeSol: 1, maxPerHourSol: 1, maxTotalSol: 1 };
    for (const p of fixture.positions ?? []) {
      const trigger = p.postKey ? { eventRef: "", postKey: p.postKey } : { eventRef: "" };
      const held = { mint: p.mint, route: "pump" as const, reservedLamports: "0", amountLamports: "0", rule: p.rule, trigger, tokenProgram: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA" };
      assert.equal(book.recordDryRunBuy(`held-${p.mint}`, held, null, caps, 0), null);
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

// Load, write back with no other change, compare as JSON values.
for (const file of readdirSync(`${dir}ledger`).sort()) {
  test(`ledger fixture ${file}`, () => {
    const source = json(`${dir}ledger/${file}`) as { mode: "live" | "dry_run" };
    const copy = join(mkdtempSync(join(tmpdir(), "ledger-fixture-")), "state.json");
    copyFileSync(`${dir}ledger/${file}`, copy);
    const book = TradeBook.open(copy, source.mode);
    book.flush();
    book.close();
    assert.deepEqual(json(copy), source);
  });
}
