// Finds contract addresses (CAs) in text and links, following the rules in shared/addresses.json.
// Used by detect/post.ts and detect/account.ts. A regex only finds candidates: a Solana candidate
// must decode to exactly 32 bytes, because many base58-looking strings (signatures, words) don't.

import { readFileSync } from "node:fs";
import bs58 from "bs58";
import type { Confidence } from "./types.ts";

type Rules = {
  solana: { pattern: string; decodedBytes: number };
  evm: { pattern: string };
  standalone: { wordChars: string };
  linkInText: { pattern: string; flags: string; stripTrailing: string };
  links: Array<{ via: string; host: string; path: string; confidence: Confidence }>;
  denylist: Array<{ address: string }>;
};
const RULES = JSON.parse(readFileSync(new URL("../../../shared/addresses.json", import.meta.url), "utf8")) as Rules;

export type Chain = "solana" | "evm";

export type FoundAddress = {
  chain: Chain;
  address: string;
  via: string; // "address" for a bare address, otherwise the link kind ("pump.fun", "dexscreener", ...)
  confidence: Confidence; // high for a bare address; a link's confidence from shared/addresses.json
};

const DENYLIST = new Set(RULES.denylist.map((d) => d.address));
const SOLANA_EXACT = new RegExp(`^${RULES.solana.pattern}$`);
const EVM_EXACT = new RegExp(`^${RULES.evm.pattern}$`);
const WORD_CHAR = new RegExp(RULES.standalone.wordChars);
const LINKS = RULES.links.map((l) => ({ ...l, host: new RegExp(l.host), path: new RegExp(l.path) }));
const STRIP_TRAILING = new RegExp(`[${RULES.linkInText.stripTrailing.replace(/[\]\\^-]/g, "\\$&")}]+$`);

// The one place that decides what an address is: "solana", "evm", or null (not an address, or
// on the denylist of base assets and programs). EVM addresses come back lowercased.
export function classifyAddress(candidate: string): { chain: Chain; address: string } | null {
  if (SOLANA_EXACT.test(candidate)) {
    const bytes = bs58.decodeUnsafe(candidate);
    if (bytes?.length !== RULES.solana.decodedBytes || DENYLIST.has(candidate)) return null;
    return { chain: "solana", address: candidate };
  }
  if (EVM_EXACT.test(candidate)) {
    const address = candidate.toLowerCase();
    return DENYLIST.has(address) ? null : { chain: "evm", address };
  }
  return null;
}

export const isSolanaAddress = (candidate: string) => classifyAddress(candidate)?.chain === "solana";

// A candidate glued to more letters or digits is a slice of something longer (a signature, a slug).
function standalone(text: string, start: number, end: number): boolean {
  const before = start > 0 ? text[start - 1]! : "";
  const after = end < text.length ? text[end]! : "";
  return !WORD_CHAR.test(before) && !WORD_CHAR.test(after);
}

type Located = FoundAddress & { at: number };

function locateAddresses(text: string): Located[] {
  const found: Located[] = [];
  for (const pattern of [RULES.evm.pattern, RULES.solana.pattern]) {
    for (const m of text.matchAll(new RegExp(pattern, "g"))) {
      if (!standalone(text, m.index, m.index + m[0].length)) continue;
      const hit = classifyAddress(m[0]);
      if (hit) found.push({ ...hit, via: "address", confidence: "high", at: m.index });
    }
  }
  return found;
}

// Plain addresses written anywhere in free text, in order of first appearance.
export function findAddresses(text: string | undefined | null): FoundAddress[] {
  return text ? firstAppearance(locateAddresses(text)) : [];
}

// A token link (pump.fun, birdeye, Jupiter, DexScreener) and the Solana mint it names.
export function addressesFromUrl(raw: string): FoundAddress[] {
  let url: URL;
  try {
    url = new URL(raw.startsWith("http") ? raw : `https://${raw}`);
  } catch {
    return [];
  }
  const host = url.hostname.toLowerCase();
  for (const link of LINKS) {
    if (!link.host.test(host)) continue;
    const hit = classifyAddress(link.path.exec(url.pathname)?.[1] ?? "");
    if (hit?.chain === "solana") return [{ ...hit, via: link.via, confidence: link.confidence }];
  }
  return [];
}

// Every address in the text, in order of first appearance, one per address. When an address also
// appears inside a token link, the link form is kept, because a link names the mint more reliably.
export function findAll(text: string | undefined | null): FoundAddress[] {
  if (!text) return [];
  const linkPattern = new RegExp(RULES.linkInText.pattern, RULES.linkInText.flags);
  const links: Located[] = [...text.matchAll(linkPattern)].flatMap((m) =>
    addressesFromUrl(m[0].replace(STRIP_TRAILING, "")).map((f) => ({ ...f, at: m.index })),
  );
  const linked = new Map(links.map((l) => [l.address, l]));
  const all = [...links, ...locateAddresses(text)].map((f) => {
    const link = linked.get(f.address);
    return link ? { ...f, via: link.via, confidence: link.confidence } : f;
  });
  return firstAppearance(all);
}

function firstAppearance(list: Located[]): FoundAddress[] {
  const seen = new Set<string>();
  return [...list]
    .sort((a, b) => a.at - b.at)
    .filter((f) => !seen.has(f.address) && seen.add(f.address))
    .map(({ at: _at, ...f }) => f);
}
