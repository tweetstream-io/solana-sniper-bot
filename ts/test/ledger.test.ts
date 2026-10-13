// Reservations, spend caps, and persistence of state/ledger.json.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Ledger } from "../src/rules/ledger.ts";

const MINT = "FmKEFyePJyGgEXUxoDHMX28Ky1XeUJcqFy5LNGB43DDA";
const caps = { maxPerTradeSol: 0.1, maxPerHourSol: 0.25, maxTotalSol: 0.25 };

test("the first reservation for a mint wins until it finishes", () => {
  const ledger = Ledger.open(null);
  assert.equal(ledger.tryReserveMint(MINT, true, "a", 0), null);
  assert.match(ledger.tryReserveMint(MINT, true, "b", 0)!, /in flight/);
  ledger.finishMint(MINT, "rejected");
  assert.equal(ledger.tryReserveMint(MINT, true, "c", 0), null, "a rejected mint may be tried again");
  ledger.finishMint(MINT, "bought");
  assert.match(ledger.tryReserveMint(MINT, true, "d", 0)!, /already bought/);
  assert.equal(ledger.tryReserveMint(MINT, false, "e", 0), null, "skipIfAlreadyBought false allows a second buy");
});

test("an unknown outcome blocks a repeat buy", () => {
  const ledger = Ledger.open(null);
  ledger.tryReserveMint(MINT, true, "a", 0);
  ledger.finishMint(MINT, "unknown");
  assert.match(ledger.tryReserveMint(MINT, true, "b", 0)!, /already unknown/);
});

test("spend caps per trade, per rolling hour, and in total", () => {
  const ledger = Ledger.open(null);
  assert.match(ledger.trySpend(200_000_000n, caps, 0)!, /maxPerTradeSol/);
  assert.equal(ledger.trySpend(100_000_000n, caps, 0), null);
  assert.equal(ledger.trySpend(100_000_000n, caps, 1_000), null);
  assert.match(ledger.trySpend(100_000_000n, caps, 2_000)!, /maxPerHourSol/);
  assert.match(ledger.trySpend(100_000_000n, caps, 3_600_001)!, /maxTotalSol/, "the hour has passed, the total has not");
  ledger.refundSpend(100_000_000n, 1_000);
  assert.equal(ledger.trySpend(100_000_000n, caps, 3_600_002), null);
});

test("snipes, mints, and positions survive a restart", () => {
  const path = join(mkdtempSync(join(tmpdir(), "ledger-")), "ledger.json");
  const first = Ledger.open(path);
  first.tryReserveMint(MINT, true, "a", 0);
  first.finishMint(MINT, "bought");
  assert.equal(first.tryReserveSnipe("rule", 1), true);
  first.flushSync();
  const second = Ledger.open(path);
  assert.match(second.tryReserveMint(MINT, true, "b", 0)!, /already bought/);
  assert.equal(second.tryReserveSnipe("rule", 1), false);
});

test("a damaged ledger stops startup instead of being reset", () => {
  const path = join(mkdtempSync(join(tmpdir(), "ledger-")), "ledger.json");
  writeFileSync(path, "{not json");
  assert.throws(() => Ledger.open(path), /buy the same mints again/);
});
