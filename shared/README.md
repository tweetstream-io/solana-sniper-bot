# Shared contract

The TypeScript bot (`ts/`) and the Rust bot (`rust/`) must behave the same way. The files in this folder are the contract between them; neither implementation keeps its own copy of what is defined here.

| File | What it defines |
| --- | --- |
| `config.schema.json` | the config format and every default. Both bots fill missing fields from the schema's `default` values, then validate against the schema. |
| `fixtures/*.json` | conformance cases: feed events in, signals and rule decisions out. |
| `demo/scenario.json` | the scripted feed that `npm run demo` replays, plus the chain state the demo answers reads from. |
| `demo/config.json` | the config the demo runs with. |

## Config defaults

Defaults live only in `config.schema.json`. To apply them, walk the schema from the root: for every property missing from the config that has a `default`, insert a copy of it; for a missing property whose schema is an object with `properties`, insert `{}` and continue into it. Resolve `$ref` (always `#/$defs/<name>`) before reading `properties` or `default`, and also read `default` from the property itself before the resolved target. Walk into arrays through `items`. Validate after filling. `config.example.json` and `demo/config.json` must pass.

Checks the schema cannot express, which both bots run after validation: every non-empty mint (`token.mint`, `fomoCoins`) and sender tip account decodes from base58 to exactly 32 bytes; rule names are unique; at least one rule is enabled; enabled rules contain no account starting with `YOUR_`.

## Fixture format

```json
{
  "name": "engagement",
  "description": "What the case covers.",
  "config": { "rules": [ ... ] },
  "positions": [ { "mint": "...", "postKey": "twitter:123", "rule": "rule-name" } ],
  "events": [
    { "connection": "standard", "receivedAtMs": 1772000001100, "envelope": { "v": 1, "t": "tweet", "op": "content", "ts": 1772000001100, "d": { ... } } }
  ],
  "expectedSignals": [ ... ],
  "expectedDecisions": [ ... ]
}
```

- `config` is a config file as a user would write it; apply schema defaults before use. When absent, use any valid config with no rule that matches the events.
- `positions` are open positions held before the first event (`postKey` is optional). They only matter for exits.
- `events` are fed in order to one fresh decision pipeline. `receivedAtMs` is the wall-clock receipt time used for post age (`maxSignalAgeMs`) and cooldowns; `connection` is `standard` or `ultra`.
- `expectedSignals` and `expectedDecisions` are each optional. When present, the concatenation of every event's output, in event order, must equal the list exactly.
- Every expected value was written by hand from the protocol and the rules below, never produced by either implementation.

To add a case, write a new file with one behavior per event, derive the expected output by hand, and run both test suites.

## Signal JSON

One contract address found in one event.

```json
{ "event": "tweet/content twitter:2026541002650550272", "source": "text", "chain": "solana", "mint": "...", "confidence": "high", "actor": { "namespace": "x", "id": "111", "handle": "dev_one" } }
```

- `event`: `"<t>/<op> <key>"`. For `tweet/*` the key is `<platform>:<tweetId>`, with platform `d.platform`, else `d.author.platform`, else `twitter`. For other events it is `d.eventId`, else the envelope `id`.
- `source`: `text`, `url`, `meta`, `ocr`, `reference`, `pin`, `profile`, `follow_bio`, `callout`, `fomo`.
- `chain`: `solana` (base58 that decodes to exactly 32 bytes) or `evm` (`0x` and 40 hex digits, lowercased).
- `confidence`: `high`, `medium`, or `low`:
  - `text`, `pin`, `callout`, `fomo`: high.
  - `url`: high, except DexScreener links (they may name a pool): low.
  - `meta`: high when the token's `sources` include `text`, else medium. `ocr`, `reference`, `follow_bio`: medium.
  - `profile`: high for `bio` and `websiteUrl`, medium for `name`; low when no previous value is known (neither the event's `previous` nor an earlier event for that account).
- `actor`: `namespace` is `x`, `pump_fun`, `fomo`, or `none`; `id` and `handle` appear only when known. X handles lose their leading `@`. pump.fun callers have only `handle` (the username). FOMO authors have `id` and `handle` (the username). `token_verified` has `{ "namespace": "none" }`.

Signals per event, in this order, one per address (the first occurrence wins, and a link form beats a bare address for the same mint):

- posts (`tweet/content`, `tweet/update`, `tweet/meta`): addresses from `urls[].url`, then the text in order of appearance, then referenced posts level by level (`ref`, then each level's `quoted` and `subtweet`, at most 4 levels), then for a stored `tweet/meta`: `detected.tokens` in order, then `ocr.text`. A `tweet/meta` or `tweet/update` produces nothing until the post has an author (from `tweet/content` or an update that carries one); after that every post event reports the signals of the whole merged post.
- `tweet/pin`: `d.tweet.urls`, then `d.tweet.text` (or `d.text`).
- `account/profile_update`: addresses in `bio`, `name`, and `websiteUrl` of `changes` that were not in the previous value of the same field.
- `account/follow` and `account/unfollow`: addresses in the target's `bio`, then a token link in `websiteUrl`.
- `account/callout`, `account/callout_update`: `token.address`. `account/thesis`, `account/token_verified`: `coin.address`.

Denied addresses never produce a signal: wrapped SOL, the system program, USDC, USDT, the SPL Token, Token-2022, associated token account, and compute budget programs, the pump.fun program, Jupiter's aggregator program, and the EVM zero address.

## Decision JSON

What each rule did with an event. A rule that the event does not concern produces nothing.

```json
{ "event": "tweet/content twitter:...", "rule": "dev-ca", "decision": "fire", "mint": "...", "action": "buy", "source": "text" }
{ "event": "tweet/content twitter:...", "rule": "dev-ca", "decision": "skip", "code": "low_confidence" }
{ "event": "tweet/delete twitter:...", "decision": "exit", "mint": "...", "code": "post_deleted" }
{ "event": "account/thesis th-5", "rule": "fomo", "decision": "exit", "mint": "...", "code": "author_exited" }
```

- `fire`: `source` is the signal source that supplied the mint, or `null` when the rule names `token.mint`.
- `skip` codes: `stale_post`, `keyword_include_missing`, `keyword_exclude_found`, `no_contract_address`, `evm_not_supported`, `low_confidence`, `mint_not_in_bio`, `bio_not_in_payload`, `update_not_new`, `multiple_too_high`, `caller_not_holding`, `cooldown`, `max_snipes`.
- `exit` codes: `post_deleted` (no `rule` key), `caller_exited`, `author_exited`. An exit is reported only when a position in that mint is open, and for rule exits only when that rule opened it.
- Order within one event: all `fire`, then all `skip`, then all `exit`. Within each group, rules in config order, and within a rule, signals in signal order.
- A rule fires at most once per post (`engagement`, `new_tweet` on posts), per event (`follow`, `pfp_update`, `new_tweet` on bios), or per mint (`ca_scanner`, `callout`, `fomo`). A repeat is silent. A skip does not use up that chance.
- `maxSnipes` (0 = no limit) and `cooldownSec` count fires per rule; a fire that the later trading stages reject gives its snipe back, which fixtures do not cover.
- A repeated event produces nothing, whichever connection it came on. `tweet/update` and `tweet/meta` repeat by envelope `id` (never deduplicated without one); events with a `d.eventId` repeat by that id; every other event repeats by its `event` key.

## Demo feed

`ts/scripts/demo-feed.ts` is a local stand-in for the TweetStream WebSocket that either bot can connect to:

```sh
cd ts
node scripts/demo-feed.ts --port 8787 --scenario ../shared/demo/scenario.json --clients 2 --speed 1
```

- `--port` (default 8787; 0 picks a free port), `--scenario` (default `shared/demo/scenario.json`), `--clients` (connections that must send `auth_ping` before the script starts; default 2), `--speed` (divides every `afterMs`; default 1). It listens on `ws://127.0.0.1:<port>/ws` and exits 1.5 s after the last step.
- Handshake: the client must offer subprotocols `tweetstream.v1` and `tweetstream.auth.token.<key>`. The server selects `tweetstream.v1`, answers 400 without it and 401 for a key not in `keys`. Every `{"op":"auth_ping"}` gets a `control/auth_pong` envelope.

Scenario format:

```json
{
  "description": "...",
  "keys": { "standard": "demo-standard-key", "ultra": "demo-ultra-key" },
  "ultraLeadMs": 3,
  "chain": { "solUsd": 150, "priorityFeeMicroLamports": 40000, "tipFloorSol": 0.00002, "blockHeight": 300000000, "mints": { "<mint>": { "tokenProgram": "token-2022", "mintAuthority": false, "freezeAuthority": false, "supply": "1000000000000000", "decimals": 6, "curve": { "virtualTokenReserves": "...", "virtualQuoteReserves": "...", "realTokenReserves": "...", "realQuoteReserves": "...", "tokenTotalSupply": "...", "complete": false } } } },
  "steps": [ { "afterMs": 400, "to": "both", "note": "what this step shows", "envelope": { "v": 1, "t": "tweet", "op": "content", "ts": "{{now}}", "d": { ... } } } ]
}
```

- Each step waits `afterMs` after the previous one. `to` is `both`, `standard`, or `ultra`; `both` sends to the Ultra connection first and to the standard one `ultraLeadMs` later, so the bot must drop a duplicate.
- In envelopes, the string `"{{now}}"` becomes the current time in Unix ms (a number), and `{{tweet:<name>}}` inside a string becomes an X post id whose embedded publish time is when that name is first used. The same name always maps to the same id within a run.
- `chain` is what the demo's stand-in network answers: for each mint, a mint account owned by SPL Token (`spl`) or Token-2022 with the given authorities, supply, and decimals, and, when `curve` is not null, a pump.fun bonding curve account. Amounts are decimal strings. `blockHeight` sets the blockhash's validity window.
