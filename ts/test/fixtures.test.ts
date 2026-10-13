// Runs every case in shared/fixtures through the Decider and compares the signal and decision JSON
// with the hand-written expectations. The Rust bot runs the same files.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { parseConfig } from "../src/setup/config.ts";
import { Ledger } from "../src/rules/ledger.ts";
import { Decider, decisionsJson, signalsJson } from "../src/pipeline.ts";
import type { Envelope } from "../src/protocol.ts";

type Fixture = {
  name: string;
  config?: unknown;
  positions?: Array<{ mint: string; postKey?: string; rule: string }>;
  events: Array<{ connection: string; receivedAtMs: number; envelope: Envelope }>;
  expectedSignals?: unknown[];
  expectedDecisions?: unknown[];
};

const dir = fileURLToPath(new URL("../../shared/fixtures/", import.meta.url));

for (const file of readdirSync(dir).filter((f) => f.endsWith(".json")).sort()) {
  const fixture = JSON.parse(readFileSync(dir + file, "utf8")) as Fixture;
  test(`fixture ${fixture.name}`, () => {
    const config = parseConfig(fixture.config ?? { rules: [{ name: "none", mode: "ca_scanner", accounts: ["1"] }] });
    const ledger = Ledger.open(null);
    for (const p of fixture.positions ?? []) {
      ledger.openPosition({ mint: p.mint, postKey: p.postKey, rule: p.rule, route: "pump", tokenProgram: "", tokenAmount: null, costLamports: "0", openedAt: 0, dryRun: true });
    }
    const decider = new Decider(config, ledger);
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
