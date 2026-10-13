// npm run docs:config: writes docs/config.md from shared/config.schema.json, so the field table can
// never disagree with the schema both bots validate against. A test fails when the file is stale.

import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { SCHEMA } from "../src/setup/config.ts";

type Node = {
  $ref?: string;
  type?: string | string[];
  enum?: unknown[];
  default?: unknown;
  description?: string;
  $comment?: string;
  properties?: Record<string, Node>;
  items?: Node;
  oneOf?: Node[];
  $defs?: Record<string, Node>;
};

const schema = SCHEMA as Node;
const resolve = (n: Node): Node => (n.$ref ? resolve(schema.$defs![n.$ref.replace("#/$defs/", "")]!) : n);
const cell = (text: string) => text.replace(/\|/g, "\\|").replace(/\n/g, " ");
const show = (value: unknown) => (value === undefined ? "" : `\`${JSON.stringify(value)}\``);

function rows(node: Node, prefix: string): string[] {
  const out: string[] = [];
  for (const [key, raw] of Object.entries(resolve(node).properties ?? {})) {
    if (key === "$schema") continue;
    const child = resolve(raw);
    const path = prefix ? `${prefix}.${key}` : key;
    const fallback = raw.default ?? child.default;
    if (child.properties && fallback === undefined) {
      out.push(...rows(child, path));
      continue;
    }
    // An object with its own default (a keyword filter) is one row that lists its parts.
    const parts = child.properties ? Object.entries(child.properties).map(([k, v]) => ` \`${k}\`: ${v.description ?? ""}`).join("") : "";
    const description = (raw.description ?? child.description ?? "") + parts;
    const impact = raw.$comment ?? child.$comment ?? "";
    out.push(`| \`${path}\` | ${show(fallback)} | ${cell(description)} | ${cell(impact)} |`);
    if (child.items && resolve(child.items).properties) out.push(...rows(child.items, `${path}[]`));
  }
  return out;
}

export function renderConfigDocs(): string {
  const header = "| Field | Default | What it does | Latency and safety |\n| --- | --- | --- | --- |";
  const top = { ...schema, properties: Object.fromEntries(Object.entries(schema.properties!).filter(([k]) => k !== "rules")) };
  return `# Configuration

\`config.json\` sits in the repository root. Copy \`config.example.json\` to start. Both bots validate it against [shared/config.schema.json](../shared/config.schema.json), and every field below has a default, so a working config only needs its rules. \`npm run docs:config\` in \`ts/\` writes this page from that schema.

Secrets and endpoints (API keys, wallet, RPC URLs) live in \`.env\`, never in this file. See [.env.example](../.env.example).

## Rules

Each entry in \`rules\` has one \`mode\`. A rule reads only the fields for its mode; [modes.md](modes.md) shows a minimal rule for each.

${header}
${rows(schema.$defs!.rule!, "rules[]").join("\n")}

## Everything else

${header}
${rows(top, "").join("\n")}
`;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  writeFileSync(fileURLToPath(new URL("../../docs/config.md", import.meta.url)), renderConfigDocs());
  console.log("wrote docs/config.md");
}
