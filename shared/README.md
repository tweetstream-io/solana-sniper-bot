# Shared contract

The TypeScript bot (`ts/`) and the Rust bot (`rust/`) must behave the same way. The files in this folder are the contract between them; neither implementation keeps its own copy of what is defined here.

| File | What it defines |
| --- | --- |
| `config.schema.json` | the config format and every default |
| `addresses.json` | contract address extraction: patterns, the 32-byte rule, token link patterns, the denylist |
| `ledger.schema.json` | the trade state file (`state/live.json`, `state/dry-run.json`), version 2 |
| `trading.json` | Jito tip accounts and the Jupiter `/execute` code map for sells |
| `fixtures/*.json` | event conformance cases: feed events in, signals and rule decisions out |
| `fixtures/config/*.json` | config conformance cases: defaults filled, errors reported |
| `fixtures/ledger/*.json` | state files that must load and write back unchanged |
| `demo/scenario.json`, `demo/config.json` | the scripted feed and config that `npm run demo` uses |

Sections: [Environment](#environment), [Config](#config), [Detection and matching](#detection-and-matching), [Signal JSON](#signal-json), [Decision JSON](#decision-json), [Trade state](#trade-state), [Money bound](#money-bound), [Event fixtures](#event-fixtures), [Demo feed](#demo-feed).

## Environment

Both bots read these from the environment, after loading `.env` in the repository root (values already in the environment win). Relative paths resolve against the repository root.

| Variable | Default | Meaning |
| --- | --- | --- |
| `TWEETSTREAM_API_KEY` | none | standard key; at least one of the two keys is required |
| `TWEETSTREAM_ULTRA_API_KEY` | none | Ultra Speed key; both connections run when both are set |
| `TWEETSTREAM_WS_URL` | `wss://ws-iad.tweetstream.io/ws` | feed endpoint |
| `DRY_RUN` | `true` | `true` or `false`; anything else is an error |
| `WALLET_SECRET_KEY` | none | base58, 64 bytes; required when `DRY_RUN=false` |
| `SOLANA_RPC_URLS` | `https://api.mainnet-beta.solana.com` | comma separated; the first serves reads, every one gets sends |
| `SOLANA_WS_URL` | first RPC URL with `ws`/`wss` | signature subscriptions |
| `JITO_URL` | `https://ny.mainnet.block-engine.jito.wtf` | `off` disables Jito |
| `JUPITER_API_KEY` | none | sent as `x-api-key` |
| `SNIPER_CONFIG` | `config.json` | config path |
| `STATE_DIR` | `state` | holds `live.json` and `dry-run.json` |

## Config

Defaults live only in `config.schema.json`. To apply them, walk the schema from the root. For every property missing from the config: if the property has a `default` (even `null`), insert a copy; otherwise if its `$ref` target has a `default`, insert that; otherwise if the target is an object with `properties`, insert `{}` and continue into it. Resolve `$ref` (always `#/$defs/<name>`) before reading `properties`. Walk into arrays through `items`. Validate against the schema after filling.

Then run the checks the schema cannot express, in this order:

1. Rule names are unique (error field `name` on the second rule with the name).
2. For each enabled rule, in order: `token.mint` and each `fomoCoins` entry, when not empty, decode from base58 to exactly 32 bytes (field `token.mint` or `fomoCoins.<i>`); then `reactor`, `accounts`, `reactedTo`, `targets`, `callers`, `fomoUsers`, in that order, contain no value matching `/^@?YOUR_/i` (field such as `accounts.0`). Disabled rules are not checked, so they may keep placeholders. `config.example.json` ships with a placeholder in its enabled rule on purpose: the bot refuses it until the user puts in a real account.
3. At least one rule is enabled (rule `null`, field `rules`).

A config error carries `rule` (the rule's name, or `null` outside the rules) and `field` (the dot-separated path inside that rule, or inside the config when `rule` is `null`). For a schema error, the path is the innermost failing value; an unknown field is reported before any other schema error, as the path of the unknown key.

`fixtures/config/*.json` holds the cases:

```json
{ "name": "missing-mint", "description": "...", "config": { ... }, "expected": { "error": { "rule": "pfp", "field": "token.mint" } } }
{ "name": "defaults", "description": "...", "config": { ... }, "expected": { "values": { "rules.0.filters.maxSnipes": 1, "safety.budgetMs": 150 } } }
```

`values` paths are dot separated, with array indexes as numbers; each value must equal the filled config exactly.

## Detection and matching

Contract addresses, from `addresses.json`:

- A Solana candidate matches `solana.pattern` and decodes from base58 to exactly `decodedBytes` (32) bytes. An EVM candidate matches `evm.pattern` and is lowercased. Anything on `denylist` (compared after lowercasing EVM) is not an address.
- In free text, a candidate counts only when the characters directly before and after it are not in `standalone.wordChars`. Punctuation such as `CA:`, `$`, `(` and `.` around it is fine.
- Token links in free text match `linkInText` (case-insensitive); trailing `stripTrailing` characters are removed before parsing. A link (from the text or from a `urls[].url`) is parsed as a URL (prefixing `https://` when it has no scheme); the first entry of `links` whose `host` matches the lowercased hostname and whose `path` matches the path gives the mint from capture group 1, with that entry's `confidence`.
- Within one text, addresses are reported in order of first appearance (a link's position is where the link starts), one per address. When an address appears both inside a link and bare, the link's kind and confidence are used.

Keywords: a word matches when it appears without case (Unicode case folding) and the characters before and after it are not letters, digits, or `_` (or it is at the edge of the text). `scanDepth` 0 reads only the account's own text; depth n also reads n levels of referenced posts (level 1 is `ref`; level n+1 is each level-n post's `quoted` and `subtweet`; at most 4 levels in total).

Accounts:

- Every account is qualified by its own platform. A post author's platform is `author.platform`, else the event's `d.platform`, else `twitter`; for `tweet/update` and `tweet/meta` the event's platform is the platform in the post key. A referenced post's author uses `ref.author.platform`, else the outer author's platform. A follow or profile actor uses `actor.platform`, else `twitter`; a follow target uses `target.platform`, else the actor's platform.
- The namespace is `x` for `twitter`, otherwise the platform (`truth_social`, `binance_square`). Rule account specs (`accounts`, `reactor`, `reactedTo`, `targets`) only match namespace `x`: a numeric spec matches `id`, any other spec matches the handle without its leading `@` and without case.
- pump.fun callers match `callers` by username, FOMO authors match `fomoUsers` by id or username, both without case. FOMO coins match `fomoCoins` exactly.
- Profile baselines are kept per `(platform, id)`. A field's previous value is the event's `previous` value for that field when present, else the last value seen for that account, from earlier profile updates (`changes`) or post authors (`bio`, `name`, `handle`). The same baseline decides which fields changed (a field changed when its new value differs) and which addresses are new.

Which activities each mode listens to (the rest of a mode's behavior is in [docs/modes.md](../docs/modes.md)):

| Mode | Activities | Matched on |
| --- | --- | --- |
| `ca_scanner` | posts, profile updates, pins | `accounts` |
| `engagement` | posts | `reactor` |
| `new_tweet` | posts, profile updates | `accounts` |
| `follow` | follows and unfollows | `accounts` |
| `pfp_update` | profile updates | `accounts` |
| `callout` | callouts and callout updates | `callers` |
| `fomo` | theses and token verifications | `fomoUsers`, or the coin in `fomoCoins` |

Payload checks: before detection, the bot checks the fields it reads for that pair (for example `tweetId`, `author`, and `kind` on `tweet/content`; `eventId` on follows and profile updates; `caller.username`, `token.address`, and `calloutId` on callouts). `tweet/content` needs a `kind`, and a `tweet/update` that carries one needs a valid one; there is no default. Every value in a profile update's `changes` and `previous` must be a string or `null`. An event that fails a check is skipped with the field's name in the log, produces no signals and no decisions, and does not stop later events.

## Signal JSON

One contract address found in one event.

```json
{ "event": "tweet/content twitter:2026541002650550272", "source": "text", "chain": "solana", "mint": "...", "confidence": "high", "actor": { "namespace": "x", "id": "111", "handle": "dev_one" } }
```

- `event`: `"<t>/<op> <key>"`. For `tweet/*` the key is `<platform>:<tweetId>`, with platform `d.platform`, else `d.author.platform`, else `twitter`. For other events it is `d.eventId`, else the envelope `id`, else an id from the payload: `<calloutId>:<kind or "callout">:<replyId or "">` for callouts, `thesis.id` for theses, `coin.address` for token verifications. Without any of these the key is empty.
- `source`: `text`, `url`, `meta`, `ocr`, `reference`, `pin`, `profile`, `follow_bio`, `callout`, `fomo`.
- `chain`: `solana` or `evm`.
- `confidence`: `text`, `pin`, `callout`, `fomo`: high. `url`: the link's confidence. `meta`: high when the token's `sources` include `text`, else medium. `ocr`, `reference`, `follow_bio`: medium. `profile`: high for `bio` and `websiteUrl`, medium for `name`, low when the field has no baseline.
- `actor`: `namespace` is `x`, `truth_social`, `binance_square`, `pump_fun`, `fomo`, or `none`; `id` and `handle` appear only when known. Handles lose their leading `@`. pump.fun callers have only `handle`. FOMO authors have `id` and `handle` (the username). `token_verified` has `{ "namespace": "none" }`.

Signals per event, in this order, one per address:

- posts (`tweet/content`, `tweet/update`, `tweet/meta`): addresses from `urls[].url`, then the text, then referenced posts level by level, then for a stored `tweet/meta`: `detected.tokens` in order (Solana addresses only), then `ocr.text`; an address already reported from the post is not repeated. A `tweet/meta` or `tweet/update` produces nothing until the post has a `kind` and an author with an id or handle; after that every post event reports the signals of the whole merged post.
- `tweet/pin`: `d.tweet.urls`, then `d.tweet.text` (or `d.text`).
- `account/profile_update`: addresses in `bio`, `name`, and `websiteUrl` of `changes` that were not in the baseline of the same field.
- `account/follow` and `account/unfollow`: addresses in the target's `bio`, then a token link in `websiteUrl`.
- `account/callout`, `account/callout_update`: `token.address`. `account/thesis`, `account/token_verified`: `coin.address`.

## Decision JSON

What each rule did with an event. A rule the event does not concern produces nothing.

```json
{ "event": "tweet/content twitter:...", "rule": "dev-ca", "decision": "fire", "mint": "...", "source": "text" }
{ "event": "tweet/content twitter:...", "rule": "dev-ca", "decision": "skip", "code": "low_confidence" }
{ "event": "tweet/delete twitter:...", "decision": "exit", "mint": "...", "code": "post_deleted" }
```

- `fire`: `source` is the signal source that supplied the mint, or `null` when the rule names `token.mint`.
- `skip` codes: `stale_post`, `keyword_include_missing`, `keyword_exclude_found`, `no_contract_address`, `evm_not_supported`, `low_confidence`, `mint_not_in_bio`, `bio_not_in_payload`, `update_not_new`, `multiple_too_high`, `caller_not_holding`, `cooldown`, `max_snipes`.
- `stale_post`: when a post is older than `maxSignalAgeMs` at receipt, each rule whose matcher would have fired or skipped reports one `stale_post` instead. Rules the event does not concern stay silent.
- `exit`: code `post_deleted`, with no `rule` key, for each position whose trigger post was deleted.
- Order within one event: all `fire`, then all `skip`, then all `exit`. Within each group, rules in config order; within a rule, signals in signal order.
- A rule fires at most once per post (`engagement`, `new_tweet` on posts), per event (`follow`, `pfp_update`, `new_tweet` on bios), or per mint (`ca_scanner`, `callout`, `fomo`). A repeat is silent. A skip does not use up that chance.
- `maxSnipes` (0 = no limit) counts a rule's buys (see Trade state) and `cooldownSec` spaces out its fires. A fire that sends nothing is given back: its snipe and its fire-once key are released.
- A repeated event produces nothing, whichever connection it came on: `tweet/update` and `tweet/meta` repeat by envelope `id` (never deduplicated without one); events with `d.eventId` repeat by that id; every other event repeats by its `event` key, and an event whose key is empty is never deduplicated.

## Trade state

The bot keeps one JSON file per mode: `state/live.json`, or `state/dry-run.json` in dry run. The file follows `ledger.schema.json`, version 2. It holds:

- `purchases`: every mint bought, with its rule
- `spend`: the total and the last hour
- `positions`: tokens held
- `pending`: trades written before sending whose outcome is not known yet

Loading:

- Only one process may use a file. Before loading, a bot writes `{"pid": <pid>, "startedAt": <ms>}` to a temp file and hard-links it to `state/live.lock` (or `state/dry-run.lock`), so the lock never exists without its content. If the lock already exists, the bot refuses to start, whatever it holds. The message names the pid and whether that process is running, and tells the user to check that no other bot uses the state directory, then delete the lock. The lock is deleted on a clean exit.
- A missing file is created empty. A version 1 file stops startup with a message to move or delete it; there is no migration. So does a file that fails the schema or belongs to the other mode.
- Each write goes to a temp file, is flushed with fsync, renamed over the old file, and the directory is fsynced.

Buying:

- The bot buys only pump.fun coins still on their bonding curve and quoted in SOL, with a transaction it builds itself. Any other coin (graduated, quoted in another token, or a cashback coin) is skipped with `unsupported_curve`. If the mint or curve cannot be read within the safety budget, the coin is skipped with `check_unknown`.
- One buy per mint, ever. A buy is refused when the mint is in `purchases`, has a pending trade, or has a buy in progress in this process.
- `maxSnipes` counts a rule's `purchases`, its pending buys, and its buys in progress.
- Before the transaction is sent, the bot writes its pending entry and adds the spend reservation (see Money bound) in one write. That write first checks the spend caps: per trade, per rolling hour, total. If the caps refuse or the write fails, nothing is sent.
- The pending entry carries `signature`, `blockhash`, `lastValidBlockHeight`, `amountLamports` (the amount spent on the token, which becomes the position's cost), and `pendingExits`.
- A failed buy is final; the bot does not retry it another way.

Selling:

- A position on a live curve is sold with a locally built pump.fun transaction. A position whose curve graduated is sold through Jupiter `/order` and `/execute`.
- A sell reserves only its fees and tip in `spend` and is never refused by the spend caps, so an exit always goes out.
- Jupiter sells: the pending entry carries the signed transaction's first signature, or `null` when it is all zeros. A Jupiter sell counts as landed only when `/execute` answers `status: "Success"`, a code listed as landed in `jupiterExecuteCodes` (`trading.json`), and a signature (the one returned, or the stored one). A code listed as rejected ends it like an expiry. Anything else, a timeout, or an unreadable answer leaves it pending, with the returned signature stored when there is one.

Outcomes, each in one write:

| Outcome | Buy | Sell |
| --- | --- | --- |
| Confirmed without error | Pending removed. The mint goes into `purchases` and the position opens with the pending entry's `pendingExits`. Spend settles to the actual debit when it is known. | Pending removed, position removed. Spend keeps the reservation. |
| Confirmed with an error | Pending removed. Not a purchase. Spend keeps the actual debit, or the whole reservation when unknown. | Pending removed. The position and its `pendingExits` stay. Spend keeps the reservation. |
| Expired, or rejected by Jupiter | Pending removed and the reservation refunded. | Same as a buy. |
| Unknown | Stays pending. Nothing is refunded. | Stays pending. |

Settling replaces the trade's reservation in `spend.totalLamports` even when its `spend.recent` entry has already left the hour window.

Reconciler, at startup and every 2 seconds (a run that starts while another runs returns at once). For each pending trade this process is not sending:

1. Ask `getSignatureStatuses` (with `searchTransactionHistory: true`). A confirmed signature resolves the trade. A signature that is seen but not confirmed keeps it pending.
2. When the signature is not found, ask `isBlockhashValid` for the trade's `blockhash` (at `processed`). Anything but `false`, including a failed call, keeps the trade pending.
3. Only after `false`, ask `getSignatureStatuses` once more. The trade expires only when this second lookup also finds nothing; a confirmed signature resolves it instead. The order matters: a transaction can confirm between the first lookup and the blockhash check.

A pending trade with `signature: null` (only a Jupiter sell can have one) waits for manual review, and the bot logs it once with these steps. Stop the bot. Look up the wallet's transactions for that mint. If the sell landed, delete `positions[<mint>]` and the pending entry in the same edit. If it did not land, delete the pending entry, subtract its `reservedLamports` from `spend.totalLamports`, and delete its entry in `spend.recent` (the one whose `tradeId` is the trade id), all in the same edit. Start the bot again. `fixtures/ledger/jupiter-review.json` is a complete example; the two entries look like this:

```json
"positions": {
  "<mint>": { "rule": "fomo", "trigger": { "eventRef": "account/thesis th-1" }, "tokenProgram": "...", "openedAt": 1772000000000, "costLamports": "10000000", "tokenAmount": "350000000000", "pendingExits": [ { "code": "max_hold", "at": 1772001800000 } ] }
},
"pending": {
  "<trade id>": { "side": "sell", "mint": "<mint>", "route": "jupiter", "signature": null, "blockhash": "...", "lastValidBlockHeight": null, "reservedLamports": "5005000", "amountLamports": "0", "rule": "fomo", "trigger": { "eventRef": "account/thesis th-1" }, "tokenProgram": "...", "sentAt": 1772001800000, "pendingExits": [] }
}
```

Exits:

- An exit for a held mint is added to the position's `pendingExits`. A delete of the trigger post that arrives while the buy is pending is added to the pending buy's `pendingExits`, and the position takes it over when the buy confirms. Either way it stays until a sell confirms. One sell at a time per position.
- Exits come from a delete of the trigger post (`post_deleted`), take profit (`take_profit`), stop loss (`stop_loss`), and max hold time (`max_hold`).
- A position with `pendingExits`, a balance above 0, and no pending trade is sold; the reconciler reads a missing balance first.
- Exit pricing: a position on a live curve is priced from the curve after fees; a graduated one from a Jupiter quote to SOL. With no price, take profit and stop loss wait for the next check; the time limit still applies.

Where a transaction goes: locally built transactions go to every `SOLANA_RPC_URLS` entry, or with `antiMev` only to Jito, carrying the Jito tip. Jupiter sells go to Jupiter `/execute` alone.

Caches: a blockhash older than 10 s is never used. The epoch reading expires after 60 s; without a fresh epoch, the higher of a Token-2022 mint's two transfer fees applies. pump.fun fee tiers older than 5 minutes are replaced by the single tier with the highest fee.

Ledger fixtures (`fixtures/ledger/*.json`): load each file in the mode inside it, write it back, and the written file must equal the original as a JSON value.

## Money bound

A buy reserves its largest possible wallet debit against the spend caps: the amount, the 5,000-lamport signature fee, the priority fee (`computeUnitLimit` times the unit price, rounded up), the Jito tip, rent for the token account (2,074,080 lamports), rent for pump.fun's volume accumulator (1,844,400 lamports), and the curve's rent top-up ((115 minus the curve account's length) times 6,960 lamports, when positive).

A local sell reserves the signature fee, the priority fee, and the Jito tip. A Jupiter sell reserves the signature fee and `execution.maxAutoPriorityFeeSol`, because Jupiter sets its own fee.

## Event fixtures

```json
{
  "name": "engagement",
  "description": "What the case covers.",
  "config": { "rules": [ ... ] },
  "positions": [ { "mint": "...", "postKey": "twitter:123", "rule": "rule-name" } ],
  "events": [ { "connection": "standard", "receivedAtMs": 1772000001100, "envelope": { "v": 1, "t": "tweet", "op": "content", "ts": 1772000001100, "d": { ... } } } ],
  "expectedSignals": [ ... ],
  "expectedDecisions": [ ... ]
}
```

- `config` is a config file as a user would write it; apply defaults before use. When absent, use any valid config with no rule that matches the events.
- `positions` are open positions held before the first event, each with the rule and post it was bought on (`postKey` optional). They only matter for delete exits.
- `events` are fed in order to one fresh decision pipeline. `receivedAtMs` is the receipt time used for post age and cooldowns.
- `expectedSignals` and `expectedDecisions` are each optional. When present, the concatenation of every event's output, in event order, must equal the list exactly.
- Every expected value was written by hand from the protocol and the rules above, never produced by either implementation. To add a case, write a new file with one behavior per event, derive the output by hand, and run both test suites.

## Demo feed

`ts/scripts/demo-feed.ts` is a local stand-in for the TweetStream WebSocket that either bot can connect to:

```sh
cd ts
node scripts/demo-feed.ts --port 8787 --scenario ../shared/demo/scenario.json --clients 2 --speed 1
```

- `--port` (default 8787; 0 picks a free port), `--scenario` (default `shared/demo/scenario.json`), `--clients` (connections that must send `auth_ping` before the script starts; default 2), `--speed` (divides every `afterMs`; default 1). It listens on `ws://127.0.0.1:<port>/ws` and exits 1.5 s after the last step.
- Handshake: the client offers subprotocols `tweetstream.v1` and `tweetstream.auth.token.<key>`. The server selects `tweetstream.v1`, answers 400 without it and 401 for a key not in `keys`. Every `{"op":"auth_ping"}` gets a `control/auth_pong` envelope.

Scenario format:

```json
{
  "description": "...",
  "keys": { "standard": "demo-standard-key", "ultra": "demo-ultra-key" },
  "ultraLeadMs": 3,
  "chain": {
    "solUsd": 150, "priorityFeeMicroLamports": 40000, "tipFloorSol": 0.00002, "blockHeight": 300000000, "epoch": 850,
    "feeTiers": [ { "marketCapLamportsThreshold": "0", "protocolFeeBps": 95, "creatorFeeBps": 30 } ],
    "mints": { "<mint>": { "tokenProgram": "token-2022", "mintAuthority": false, "freezeAuthority": false, "supply": "1000000000000000", "decimals": 6, "curve": { "virtualTokenReserves": "...", "virtualQuoteReserves": "...", "realTokenReserves": "...", "realQuoteReserves": "...", "tokenTotalSupply": "...", "complete": false } } }
  },
  "steps": [ { "afterMs": 400, "to": "both", "note": "what this step shows", "envelope": { "v": 1, "t": "tweet", "op": "content", "ts": "{{now}}", "d": { ... } } } ]
}
```

- Each step waits `afterMs` after the previous one. `to` is `both`, `standard`, or `ultra`; `both` sends to the Ultra connection first and to the standard one `ultraLeadMs` later, so the bot must drop a duplicate.
- In envelopes, the string `"{{now}}"` becomes the current time in Unix ms (a number), and `{{tweet:<name>}}` inside a string becomes an X post id whose embedded publish time is when that name is first used. The same name maps to the same id within a run.
- `chain` is what the demo's stand-in network answers: for each mint, a mint account owned by SPL Token (`spl`) or Token-2022 with the given authorities, supply, and decimals; when `curve` is not null, a pump.fun bonding curve account; pump.fun's fee config with `feeTiers`; `blockHeight` and `epoch` from `getEpochInfo`, with each blockhash valid for 150 blocks. Amounts are decimal strings.
