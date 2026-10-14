# Configuration

`config.json` sits in the repository root. Copy `config.example.json` to start. Both bots validate it against [shared/config.schema.json](../shared/config.schema.json), and every field below has a default, so a working config only needs its rules. `npm run docs:config` in `ts/` writes this page from that schema.

Secrets and endpoints (API keys, wallet, RPC URLs) live in `.env`, never in this file. See [.env.example](../.env.example).

## Rules

Each entry in `rules` has one `mode`. A rule reads only the fields for its mode; [modes.md](modes.md) shows a minimal rule for each.

| Field | Default | What it does | Latency and safety |
| --- | --- | --- | --- |
| `rules[].name` |  | Unique name, used in logs and in the state file. |  |
| `rules[].enabled` | `true` | Turn the rule on or off. |  |
| `rules[].mode` |  | What makes the rule fire. See docs/modes.md. |  |
| `rules[].accounts` | `[]` | Accounts to watch: numeric X account id or @handle. | Ids survive handle changes; handles do not. Prefer ids. |
| `rules[].sources` | `["text","url","meta","ocr","reference","pin","profile"]` | ca_scanner: where an address may come from. | meta and ocr arrive in a later tweet/meta event, so they fire later than text. |
| `rules[].minConfidence` | `"medium"` | ca_scanner: lowest signal confidence to act on. |  |
| `rules[].requireMintInBio` | `false` | ca_scanner: also require the address in the account's last known bio. | Safety: a hijacked account rarely edits its bio first. |
| `rules[].reactor` |  | engagement: the account whose retweet, quote, or reply fires the rule. |  |
| `rules[].targetTweetId` | `""` | engagement: id of the post the reactor must engage with. Empty means match by reactedTo. |  |
| `rules[].reactedTo` | `[]` | engagement: accounts whose posts count as targets when targetTweetId is empty. |  |
| `rules[].actions` | `["retweet","quote","reply"]` | engagement: which kinds of engagement count. Likes are private on X, so they cannot be watched. |  |
| `rules[].includeKeywords` | `{"words":[],"scanDepth":0}` | engagement, new_tweet, fomo: fire only when one of these words appears within scanDepth (fomo reads the thesis text). `words`: Whole words, matched without case. `scanDepth`: 0 reads only the account's own text. 1 also reads the post it quotes, replies to, or reposts. 2 goes one level deeper. |  |
| `rules[].excludeKeywords` | `{"words":[],"scanDepth":0}` | engagement, new_tweet, fomo: never fire when one of these words appears within scanDepth. `words`: Whole words, matched without case. `scanDepth`: 0 reads only the account's own text. 1 also reads the post it quotes, replies to, or reposts. 2 goes one level deeper. |  |
| `rules[].events` | `["post","quote","reply"]` | new_tweet: which actions fire the rule. bio fires on a bio change. |  |
| `rules[].followEvents` | `["follow"]` | follow: fire on follows, unfollows, or both. |  |
| `rules[].targets` | `[]` | follow: only fire when the followed account is one of these. Empty matches any. |  |
| `rules[].fields` | `["avatar"]` | pfp_update: which profile fields fire the rule. |  |
| `rules[].callers` | `[]` | callout: pump.fun usernames whose callouts fire the rule. They must also be tracked in the dashboard's Callouts tab. |  |
| `rules[].calloutKinds` | `["callout"]` | callout: which callout kinds fire the rule. author_update, reply and repost are later posts about an earlier call. |  |
| `rules[].maxMultiple` | `null` | callout: skip when the event reports the token already ran more than this multiple since the call. null turns it off. | Safety: buying after a 5x run buys someone else's exit. |
| `rules[].requireCallerPosition` | `false` | callout: fire only when the event reports that the caller holds the token (position.amountHeld above 0). |  |
| `rules[].fomoUsers` | `[]` | fomo: FOMO usernames or author ids whose theses fire the rule. Track them in the dashboard's FOMO tab too. |  |
| `rules[].fomoCoins` | `[]` | fomo: coin addresses. A thesis by any author on one of these fires the rule, and so does the coin's first observed verification. |  |
| `rules[].fomoEvents` | `["thesis"]` | fomo: fire on theses, on first observed token verification of a watched coin, or both. |  |
| `rules[].useDetectedCa` | `false` | engagement, new_tweet, follow, pfp_update: trade the first contract address found in the event (post text, token link, image text, the followed account's bio, or the new bio) when token.mint is empty. |  |
| `rules[].token.mint` | `""` | The token to trade. ca_scanner, callout and fomo: leave empty to trade the address in the event, or set it to act only on that token. Other modes: required unless useDetectedCa is true. |  |
| `rules[].token.amountSol` | `0.01` | SOL to spend on the buy. | Also capped by spend.maxPerTradeSol. |
| `rules[].filters.maxSnipes` | `1` | How many tokens this rule may buy, counted from the buys in the state file. 0 means no limit. | Safety: caps losses from a rule that fires on a compromised account. |
| `rules[].filters.cooldownSec` | `0` | Seconds to wait after this rule fires before it may fire again. | Safety: stops a burst of posts from buying many tokens. |
| `rules[].filters.minTokenAgeSec` | `null` | Skip tokens younger than this. null turns the check off. | Needs market data; see onMissingData. |
| `rules[].filters.maxTokenAgeSec` | `null` | Skip tokens older than this. null turns the check off. | Needs market data; see onMissingData. |
| `rules[].filters.minMarketCapUsd` | `null` | Skip tokens with a smaller market cap. null turns the check off. | pump.fun curve tokens are priced from the curve read; other venues need market data. |
| `rules[].filters.maxMarketCapUsd` | `null` | Skip tokens with a larger market cap. null turns the check off. | Same data as minMarketCapUsd. |
| `rules[].filters.minLiquidityUsd` | `null` | Skip tokens with less liquidity. null turns the check off. | Same data as minMarketCapUsd. |
| `rules[].filters.maxPoolTaxPct` | `0` | Skip tokens whose transfer fee is above this percent. null turns the check off. | Read from the mint account, so it costs no extra request. |
| `rules[].filters.platforms` | `[]` | Venues this rule may trade on, such as pumpfun, pumpswap, raydium, meteora, orca, moonshot. Empty allows any. | pumpfun is built locally; every other venue goes through Jupiter. |
| `rules[].filters.skipSuspiciousPools` | `false` | Skip tokens that the safety check service flags as dangerous. | Needs safety.rugcheck turned on. |
| `rules[].filters.onMissingData` | `"reject"` | What to do when a filter needs data that is not available in time. | reject is the safe choice: unknown never counts as passing. |
| `rules[].fees.slippagePct` | `20` | Largest price move you accept, in percent. | Too low and fast-moving launches fail; too high and you overpay. |
| `rules[].fees.priorityFeeSol` | `"auto"` | Priority fee for the whole transaction in SOL, or auto for a recent-fee estimate. | auto reads a cached estimate refreshed in the background, so it adds no latency. The whole fee counts against the spend caps. |
| `rules[].fees.tipSol` | `"auto"` | Tip paid to the Jito block engine in SOL, or auto for the recent landed-tip level. | Only paid when antiMev is on. |
| `rules[].fees.antiMev` | `true` | Send only through the Jito block engine, with a tip, which protects the transaction from sandwiching. Off: send to the RPC URLs instead. | Jupiter trades are landed by Jupiter and ignore this. |

## Everything else

| Field | Default | What it does | Latency and safety |
| --- | --- | --- | --- |
| `maxSignalAgeMs` | `5000` | Ignore posts older than this at receipt (publish time from the post id). | Safety: a replayed or delayed post must not trigger a buy at a stale price. |
| `safety.budgetMs` | `150` | Time limit for all checks together, in milliseconds. | Latency: the most a check can add before the send. A check that misses it counts as unknown. |
| `safety.onUnknown` | `"reject"` | What a check that failed or timed out means. | reject is the safe choice. |
| `safety.rugcheck` | `"off"` | off: skip. advisory: reject only on a danger result that arrives in time. required: no answer counts as unknown. | Latency: an extra HTTPS request in the parallel check set. |
| `safety.maxRugcheckScore` | `null` | Reject when the normalised risk score is above this. null turns the score test off. |  |
| `safety.rejectMintAuthority` | `true` | Reject tokens whose supply can still be increased. | Skipped for pump.fun curve tokens, whose authorities are fixed by the program. |
| `safety.rejectFreezeAuthority` | `true` | Reject tokens whose holder accounts can be frozen. | Skipped for pump.fun curve tokens. |
| `safety.rejectRiskyExtensions` | `true` | Reject Token-2022 mints with transfer hooks, permanent delegates, default-frozen accounts, pausing, or non-transferable tokens. |  |
| `spend.maxPerTradeSol` | `0.1` | Largest single buy. |  |
| `spend.maxPerHourSol` | `0.5` | Most SOL spent in any rolling hour. |  |
| `spend.maxTotalSol` | `1` | Most SOL this bot will ever spend. |  |
| `exits.takeProfitPct` | `100` | Sell when the position is up this many percent. null turns it off. |  |
| `exits.stopLossPct` | `50` | Sell when the position is down this many percent. null turns it off. |  |
| `exits.maxHoldSec` | `1800` | Sell after this many seconds. null turns it off. |  |
| `exits.pollMs` | `2000` | How often to price open positions. | Each poll is one RPC read per pump.fun position, one quote per other position. |
| `exits.sellOnDelete` | `true` | Sell when the post that triggered the buy is deleted. |  |
| `exits.sellSlippagePct` | `30` | Slippage for exit sells, in percent. |  |
| `execution.computeUnitLimit` | `120000` | Compute unit limit for locally built pump.fun trades. | A tight limit makes the priority fee per unit higher for the same SOL. |
| `execution.maxAutoPriorityFeeSol` | `0.005` | Ceiling for priorityFeeSol auto. |  |
| `execution.maxAutoTipSol` | `0.005` | Ceiling for tipSol auto. |  |
| `execution.blockhashRefreshMs` | `400` | How often the background loop fetches a fresh blockhash. | Latency: keeps the blockhash fetch off the hot path. |
| `execution.rebroadcastMs` | `1500` | How often an unconfirmed transaction is sent again. |  |
| `execution.confirmTimeoutMs` | `60000` | Give up waiting for confirmation after this long and mark the outcome unknown. |  |
