// Finds contract addresses (CAs) in text and links. Used by detect/post.ts and detect/account.ts.
// A regex only finds candidates: every Solana candidate must decode to exactly 32 bytes, because
// many base58-looking strings (signatures, words, IDs) don't. A "pump" suffix proves nothing.

import bs58 from "bs58";

export type Chain = "solana" | "evm";

export type FoundAddress = {
  chain: Chain;
  address: string;
  // Where the address came from inside the string. A launchpad token link names the mint
  // directly; a chart link may name a pool instead, so it gets lower confidence.
  via: "address" | "pump.fun" | "birdeye" | "jupiter" | "dexscreener";
};

// Addresses that look like tokens but are never worth buying: base assets, stablecoins,
// and programs that show up in posts as part of explanations or screenshots.
export const DENYLIST: ReadonlySet<string> = new Set([
  "So11111111111111111111111111111111111111112", // wrapped SOL mint
  "11111111111111111111111111111111", // system program
  "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", // USDC
  "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", // USDT
  "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", // SPL Token program
  "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb", // Token-2022 program
  "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL", // associated token account program
  "ComputeBudget111111111111111111111111111111", // compute budget program
  "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P", // pump.fun program
  "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4", // Jupiter aggregator program
  "0x0000000000000000000000000000000000000000", // EVM zero address
]);

const BASE58_RUN = /[1-9A-HJ-NP-Za-km-z]{32,44}/g;
const EVM_RUN = /0x[0-9a-fA-F]{40}/g;
// A candidate must not be glued to more letters or digits, or it is a slice of a longer token
// (a transaction signature, a URL slug). Punctuation around it ("CA:", "$", "(", ".") is fine.
const WORD_CHAR = /[0-9A-Za-z]/;

export function isSolanaAddress(candidate: string): boolean {
  if (candidate.length < 32 || candidate.length > 44) return false;
  const bytes = bs58.decodeUnsafe(candidate);
  return bytes !== undefined && bytes.length === 32;
}

export function isEvmAddress(candidate: string): boolean {
  return /^0x[0-9a-fA-F]{40}$/.test(candidate);
}

function standalone(text: string, start: number, end: number): boolean {
  const before = start > 0 ? text[start - 1] : "";
  const after = end < text.length ? text[end] : "";
  return !(before && WORD_CHAR.test(before)) && !(after && WORD_CHAR.test(after));
}

type Located = FoundAddress & { at: number };

function locateAddresses(text: string): Located[] {
  const found: Located[] = [];
  for (const m of text.matchAll(EVM_RUN)) {
    if (standalone(text, m.index, m.index + m[0].length)) found.push({ chain: "evm", address: m[0].toLowerCase(), via: "address", at: m.index });
  }
  for (const m of text.matchAll(BASE58_RUN)) {
    if (standalone(text, m.index, m.index + m[0].length) && isSolanaAddress(m[0])) found.push({ chain: "solana", address: m[0], via: "address", at: m.index });
  }
  return found;
}

// Plain addresses written anywhere in free text (post text, bio, image text, display name).
export function findAddresses(text: string | undefined | null): FoundAddress[] {
  return text ? firstAppearance(locateAddresses(text)) : [];
}

// Token links. Only the pump.fun coin path is a simple, reliable URL-to-mint mapping; the
// others are best effort and still go through the same 32-byte validation.
const LINK_PATTERNS: Array<{ via: FoundAddress["via"]; host: RegExp; path: RegExp }> = [
  { via: "pump.fun", host: /(^|\.)pump\.fun$/, path: /^\/(?:coin\/)?([1-9A-HJ-NP-Za-km-z]{32,44})\/?$/ },
  { via: "birdeye", host: /(^|\.)birdeye\.so$/, path: /^\/(?:solana\/)?token\/([1-9A-HJ-NP-Za-km-z]{32,44})\/?$/ },
  { via: "jupiter", host: /(^|\.)jup\.ag$/, path: /^\/(?:swap\/)?[A-Za-z0-9]+-([1-9A-HJ-NP-Za-km-z]{32,44})\/?$/ },
  // Chart links usually carry a pool (pair) address, which is not the mint. Kept, with a low
  // confidence, so a rule can opt in; the safety gate rejects a pool because it is not a mint.
  { via: "dexscreener", host: /(^|\.)dexscreener\.com$/, path: /^\/solana\/([1-9A-HJ-NP-Za-km-z]{32,44})\/?$/ },
];

export function addressesFromUrl(raw: string): FoundAddress[] {
  let url: URL;
  try {
    url = new URL(raw.startsWith("http") ? raw : `https://${raw}`);
  } catch {
    return [];
  }
  const host = url.hostname.toLowerCase();
  for (const pattern of LINK_PATTERNS) {
    if (!pattern.host.test(host)) continue;
    const mint = pattern.path.exec(url.pathname)?.[1];
    if (mint && isSolanaAddress(mint) && !DENYLIST.has(mint)) {
      return [{ chain: "solana", address: mint, via: pattern.via }];
    }
  }
  return [];
}

// Links written into free text (bios and display names have no separate url list).
const URL_IN_TEXT = /(?:https?:\/\/)?(?:[a-z0-9-]+\.)*(?:pump\.fun|birdeye\.so|jup\.ag|dexscreener\.com)\/[^\s)\]}>"']+/gi;

// Every address in the text, in order of first appearance, one per address. When an address also
// appears inside a token link, the link form is kept, because a link names the mint more reliably.
export function findAll(text: string | undefined | null): FoundAddress[] {
  if (!text) return [];
  const links: Located[] = [...text.matchAll(URL_IN_TEXT)].flatMap((m) =>
    addressesFromUrl(m[0].replace(/[.,;:!?]+$/, "")).map((f) => ({ ...f, at: m.index })),
  );
  const linked = new Map(links.map((l) => [l.address, l.via]));
  const all = [...links, ...locateAddresses(text)].map((f) => ({ ...f, via: linked.get(f.address) ?? f.via }));
  return firstAppearance(all);
}

function firstAppearance(list: Located[]): FoundAddress[] {
  const seen = new Set<string>();
  return [...list]
    .sort((a, b) => a.at - b.at)
    .filter((f) => !DENYLIST.has(f.address) && !seen.has(f.address) && seen.add(f.address))
    .map(({ at: _at, ...f }) => f);
}
