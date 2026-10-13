# How it works

One event, from the WebSocket to a confirmed transaction, passes through these files in order. Each file opens with a comment saying what it does and where the event goes next.

1. `ts/src/feed/client.ts` opens one WebSocket per API key with the `tweetstream.v1` and `tweetstream.auth.token.<key>` subprotocols, sends `auth_ping`, reconnects with jittered backoff, and stamps each frame on receipt.
2. `ts/src/feed/dedupe.ts` drops the second copy of an event that arrived on both connections.
3. `ts/src/pipeline.ts` holds the `Decider`, the synchronous part of the path. It routes the event by `t` and `op`, updates the stored post (`detect/store.ts`), runs the detectors, and asks the rule engine what fires.
4. `ts/src/detect/addresses.ts` finds contract addresses: Solana base58 strings that decode to exactly 32 bytes, EVM `0x` addresses (logged, never traded), and token links. `detect/post.ts` and `detect/account.ts` turn events into signals (one address in one event) and activities (what an account did).
5. `ts/src/rules/index.ts` looks up the rules that watch the acting account; `rules/modes.ts` has one matcher per mode; `rules/engine.ts` applies max snipes, cooldowns, and fire-once; `rules/ledger.ts` holds the durable state.
6. `ts/src/filters/gate.ts` runs the chain read, safety checks, and rule filters under one time budget and picks the route.
7. `ts/src/execute/trade.ts` reserves the mint and the spend, builds and signs the transaction, and keeps the background caches. `execute/pump.ts` builds pump.fun curve trades locally.
8. `ts/src/land/land.ts` sends the signed transaction to every sender, rebroadcasts it, and waits for `signatureSubscribe`.
9. `ts/src/report/latency.ts` records each stage; `ts/src/exits/exits.ts` watches open positions.

`ts/src/network.ts` holds every outside read (RPC, Jupiter, Jito tip levels, DexScreener, Rugcheck). The demo and benchmark replace it with `ts/scripts/scenario-network.ts`, which answers from a scenario file.

## TweetStream events used

| Event | Used for |
| --- | --- |
| `tweet/content` | the post: text, author, `kind` (post, quote, reply, retweet), `ref` chain, `urls` |
| `tweet/update` | patches the stored post; `textUpdateType: "completion"` brings the full text of a post that arrived shortened, and a new `ref` replaces the old one |
| `tweet/meta` | a full snapshot of detected tokens (`detected.tokens`) and image text (`ocr.text`); replaces the previous snapshot |
| `tweet/pin` | a newly pinned post and its text |
| `tweet/delete` | sells positions bought on that post |
| `account/follow`, `account/unfollow` | who followed whom; the target's `bio` and `websiteUrl` when present |
| `account/profile_update` | `changes` and `previous` for `bio`, `name`, `websiteUrl`, `avatar`, `banner`, `handle` |
| `account/callout`, `account/callout_update` | pump.fun callouts: caller, token, kind, market cap, multiple, caller position |
| `account/thesis`, `account/token_verified` | FOMO theses and first observed verifications |
| `control/auth_pong` | confirms the connection is authenticated |

Posts are keyed by `(platform, tweetId)`. Post ids are parsed with `BigInt`, never `Number`: X ids are 64-bit, and a JavaScript number keeps only 53 bits. The publish time comes from the id (`(id >> 22) + 1288834974657`).

The full event contract is at [tweetstream.io/docs/asyncapi.json](https://tweetstream.io/docs/asyncapi.json). The bot uses only fields defined there.

## External APIs

Each request shape was checked against the provider's documentation:

| Service | Request | Documentation |
| --- | --- | --- |
| Solana RPC | `getMultipleAccounts`, `getLatestBlockhash`, `getRecentPrioritizationFees`, `getTokenAccountsByOwner`, `sendTransaction`, `signatureSubscribe` | solana.com/docs/rpc |
| pump.fun program | `buy_exact_quote_in_v3` and `sell_v3` with 17 accounts, `BondingCurve` layout | github.com/pump-fun/pump-public-docs (`idl/pump.json`, `docs/instructions/TRADE_V3.md`, `docs/FEE_RECIPIENTS.md`) |
| PumpPortal | `POST https://pumpportal.fun/api/trade-local` returns an unsigned transaction | pumpportal.fun/local-trading-api/trading-api |
| Jupiter | `GET /swap/v2/order`, `POST /swap/v2/execute`, `GET /price/v3` | developers.jup.ag/docs/swap/order-and-execute, dev.jup.ag/docs/price/v3 |
| Jito | `sendTransaction` on `/api/v1/transactions`, tip accounts, `tip_floor` | docs.jito.wtf/lowlatencytxnsend |
| DexScreener | `GET /token-pairs/v1/solana/{mint}` | docs.dexscreener.com/api/reference |
| Rugcheck | `GET /v1/tokens/{mint}/report/summary` | api.rugcheck.xyz/swagger |

Details that differ from what is commonly written about these APIs:

- pump.fun has a v3 trade interface with 17 accounts that keeps protocol and creator fees on the curve, so the creator vault and fee recipient accounts of older guides are not needed. Cashback coins reject v3 trades; the bot sends those through PumpPortal.
- PumpPortal's `pool` defaults to `pump`; the bot sends `auto`.
- Jupiter's `/order` returns `mode: "ultra"` only when no optional parameters are sent. Adding `slippageBps` or a priority fee switches to `manual` and can restrict routing, so the bot leaves both to Jupiter, and the rule's `slippagePct` and `priorityFeeSol` apply only to pump.fun trades. Jupiter lands the transaction itself through `/execute`, so the bot's own senders and Jito tip are not used on that route. Without an API key Jupiter allows 0.5 requests per second.
- Jito's tip accounts are listed in its documentation and do not change, so the bot does not fetch them. `getTipAccounts` is served at `/api/v1/getTipAccounts`. Jito's `sendTransaction` takes only an `encoding` option and always skips preflight. Its default rate limit is one request per second per IP and region.
- `getRecentPrioritizationFees` with an account list reports only transactions that lock every listed account as writable. The accounts all pump.fun trades share are read-only, so the bot asks without a filter.
- Jupiter's Price API omits tokens it cannot price reliably, which includes most tokens minutes old; that is why curve tokens are priced from the curve.
