# How it works

One event, from the WebSocket to a confirmed transaction. Each file opens with a comment saying what it does and where the event goes next. The Rust bot has the same files under `rust/src/` with `.rs` names (`pipeline.rs`, `execute/trade.rs`, and so on). Read them in this order:

1. `ts/src/pipeline.ts`: `Decider.handle` is the synchronous part of the path. It checks the payload, drops a duplicate (`feed/dedupe.ts`), updates the stored post (`detect/store.ts`), runs the detectors (`detect/post.ts`, `detect/account.ts`, which find addresses with `detect/addresses.ts`), and asks the rule engine (`rules/engine.ts`, with one entry per mode in `rules/modes.ts`) what fires. `startBot` below it wires the feed (`feed/client.ts`) to the trader.
2. `ts/src/execute/trade.ts`: `Trader.buy` runs the checks in `filters/gate.ts`, builds and signs the transaction (pump.fun curve trades in `execute/pump.ts`, everything else through Jupiter), writes it to the state file as pending, sends it through `land/land.ts` or Jupiter, and records the outcome. `Trader.sell` and the reconciler, which finishes trades whose outcome was not known, follow the same steps.
3. `ts/src/execute/trades.ts`: the only code that writes `state/`. Its rules are in [shared/README.md](../shared/README.md#trade-state).
4. `ts/src/execute/cache.ts` keeps the blockhash, block height, epoch, fee levels, SOL price, and pump.fun fee tiers fresh in the background. `ts/src/exits/exits.ts` prices open positions. `ts/src/report/latency.ts` records each stage.

`ts/src/network.ts` holds every call to an outside service except sending: RPC reads, Jupiter orders and executes, Jito tip levels, DexScreener, Rugcheck. The demo, the benchmark, and the trade tests replace it with `ts/scripts/scenario-network.ts`, which answers from a scenario file.

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

The full event contract is at [tweetstream.io/docs/asyncapi.json](https://tweetstream.io/docs/asyncapi.json?utm_source=github&utm_medium=referral&utm_campaign=solana-sniper-bot&utm_content=docs-how-it-works). The bot uses only fields defined there, and skips an event that lacks a field it reads, naming the field in the log.

## External APIs

Each request shape was checked against the provider's documentation:

| Service | Request | Documentation |
| --- | --- | --- |
| Solana RPC | `getMultipleAccounts`, `getLatestBlockhash`, `getEpochInfo`, `getRecentPrioritizationFees`, `getTokenAccountsByOwner`, `getBalance`, `simulateTransaction`, `getSignatureStatuses`, `isBlockhashValid`, `getTransaction`, `sendTransaction`, `signatureSubscribe` | solana.com/docs/rpc |
| pump.fun program | `buy_exact_quote_in_v3` and `sell_v3` with 17 accounts, `BondingCurve` and `FeeConfig` layouts, fee tiers by market cap | github.com/pump-fun/pump-public-docs (`idl/pump.json`, `docs/instructions/TRADE_V3.md`, `docs/FEE_RECIPIENTS.md`, `docs/FEE_PROGRAM_README.md`) |
| Jupiter | `GET /swap/v2/order`, `POST /swap/v2/execute`, `GET /price/v3` | developers.jup.ag/docs/swap/order-and-execute, dev.jup.ag/docs/price/v3 |
| Jito | `sendTransaction` on `/api/v1/transactions`, tip accounts, `tip_floor` | docs.jito.wtf/lowlatencytxnsend |
| DexScreener | `GET /token-pairs/v1/solana/{mint}` | docs.dexscreener.com/api/reference |
| Rugcheck | `GET /v1/tokens/{mint}/report/summary` | api.rugcheck.xyz/swagger |

Details that differ from what is commonly written about these APIs:

- pump.fun has a v3 trade interface with 17 accounts that keeps protocol and creator fees on the curve, so the creator vault and fee recipient accounts of older guides are not needed. Cashback coins and curves quoted in a token other than SOL reject or do not fit v3 buys, so the bot skips them (`unsupported_curve`).
- pump.fun charges protocol and creator fees chosen by the coin's market cap from the `FeeConfig` account. The bot reads that account in the background and quotes after fees, so `slippagePct` is a real tolerance.
- Jupiter's `/order` returns `mode: "ultra"` only when no optional parameters are sent. Adding `slippageBps` or a priority fee switches to `manual` and can restrict routing, so the bot leaves both to Jupiter, and the rule's `slippagePct` and `priorityFeeSol` apply only to pump.fun trades. Jupiter lands the transaction itself through `/execute`, so the bot's own senders and Jito tip are not used on that route. Its documented `/execute` codes split three ways (the map is in `shared/trading.json`): 0 is success; -1, -2, -3 and -1002 to -1004, -2002 to -2004 reject the transaction before it lands; -1000, -2000 ("failed to land") and -1001, -2001 ("unknown error") prove nothing, so the trade stays pending and the bot checks the signature later. `/order` has no fee ceiling, so the bot simulates a Jupiter buy once and refuses it when the wallet would lose more than the buy reserved. Without an API key Jupiter allows 0.5 requests per second.
- Jito's tip accounts are listed in its documentation and do not change, so the bot keeps them in `shared/trading.json` instead of fetching them. `getTipAccounts` is served at `/api/v1/getTipAccounts`. Jito's `sendTransaction` takes only an `encoding` option and always skips preflight. Its default rate limit is one request per second per IP and region.
- `getRecentPrioritizationFees` with an account list reports only transactions that lock every listed account as writable. The accounts all pump.fun trades share are read-only, so the bot asks without a filter.
- Jupiter's Price API omits tokens it cannot price reliably, which includes most tokens minutes old; that is why curve tokens are priced from the curve.
