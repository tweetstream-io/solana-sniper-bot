// Config loading: defaults from the shared schema, the shipped example files, and error messages
// that say what to fix.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { ConfigError, handleWarnings, loadConfig, parseConfig } from "../src/setup/config.ts";

const root = (path: string) => fileURLToPath(new URL(`../../${path}`, import.meta.url));
const schema = JSON.parse(readFileSync(root("shared/config.schema.json"), "utf8"));

const message = (json: unknown) => {
  try {
    parseConfig(json);
  } catch (error) {
    assert.ok(error instanceof ConfigError);
    return error.message;
  }
  assert.fail("expected a ConfigError");
};


test("config.example.json needs only its placeholder replaced; the demo config loads as is", () => {
  const example = JSON.parse(readFileSync(root("config.example.json"), "utf8"));
  assert.match(message(example), /rule "dev-ca": replace the placeholder "YOUR_ACCOUNT"/);
  example.rules[0].accounts = ["44196397"];
  assert.equal(parseConfig(example).rules.filter((r) => r.enabled).length, 1);
  assert.equal(loadConfig(root("shared/demo/config.json")).rules.length, 7);
});

test("missing fields take the defaults published in shared/config.schema.json", () => {
  const config = parseConfig({ rules: [{ name: "r", mode: "ca_scanner", accounts: ["1"] }] });
  const ruleProps = schema.$defs.rule.properties;
  assert.equal(config.rules[0]!.filters.maxSnipes, ruleProps.filters.properties.maxSnipes.default);
  assert.equal(config.rules[0]!.fees.slippagePct, ruleProps.fees.properties.slippagePct.default);
  assert.equal(config.safety.budgetMs, schema.properties.safety.properties.budgetMs.default);
});

test("errors name the rule and the fix", () => {
  assert.match(message({ rules: [{ name: "pfp", mode: "pfp_update", accounts: ["1"] }] }), /rule "pfp": set token.mint .* "useDetectedCa": true/);
  assert.match(message({ rules: [{ name: "x", mode: "ca_scanner", accounts: ["not a handle!"] }] }), /X handle .* numeric account id/);
  assert.match(message({ rules: [{ name: "x", mode: "sniper" }] }), /mode must be one of "ca_scanner"/);
  assert.match(message({ rules: [{ name: "x", mode: "ca_scanner", acounts: ["1"] }] }), /unknown field "acounts"/);
  assert.match(message({ rules: [{ name: "x", mode: "ca_scanner", accounts: ["YOUR_HANDLE"] }] }), /replace the placeholder/);
  assert.match(message({ rules: [{ name: "x", mode: "new_tweet", accounts: ["1"], token: { mint: "4uQeVj5tqViQh7yWWGStvkEG1Zmhx6uasJtWCJziofL" } }] }), /does not decode to a 32-byte/);
  assert.match(message({ rules: [] }), /rules/);
});

test("handles produce a warning, ids do not", () => {
  const config = parseConfig({ rules: [{ name: "r", mode: "ca_scanner", accounts: ["@someone", "44196397"] }] });
  const warnings = handleWarnings(config);
  assert.equal(warnings.length, 1);
  assert.match(warnings[0]!, /@someone/);
});

test("docs/config.md matches the schema (run npm run docs:config after editing the schema)", async () => {
  const { renderConfigDocs } = await import("../scripts/config-docs.ts");
  assert.equal(readFileSync(root("docs/config.md"), "utf8"), renderConfigDocs());
});

test("a denylisted mint is named as such, not called a bad address", () => {
  const rule = (mint: string) => ({ rules: [{ name: "pfp", mode: "pfp_update", accounts: ["111"], token: { mint } }] });
  assert.equal(message(rule("So11111111111111111111111111111111111111112")), 'rule "pfp": "So11111111111111111111111111111111111111112" is the wrapped SOL mint, which shared/addresses.json denylists because it is never a token to snipe.');
  assert.match(message(rule("So1111111111111111111111111111111111111111")), /does not decode to a 32-byte Solana address/);
});
