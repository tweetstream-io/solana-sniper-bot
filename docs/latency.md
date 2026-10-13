# Latency

A snipe is a race from the moment an account posts to the moment your transaction lands. This page lists every stage the bot controls, what it does about each, and how to measure your own setup.

## Run it next to the feed

Run the bot in Ashburn, Virginia (IAD); a Teraswitch IAD1 server is a good choice. That puts it a short hop from TweetStream's US endpoint, `wss://ws-iad.tweetstream.io/ws` (the default in `.env.example`), and from the validators and transaction landing services in the US East. Outside the US, `wss://ws-global.tweetstream.io/ws` is the other endpoint; set `TWEETSTREAM_WS_URL`.

Pick an RPC provider with a node in the same region, put it first in `SOLANA_RPC_URLS`, and keep `JITO_URL` on the New York block engine, which is the closest Jito region to Ashburn.

Then measure from your server:

```sh
cd ts
npm run latency:probe
```

It times the TLS connect and warm round trips to each RPC, the RPC WebSocket, the Jito block engine, and any extra sender in `config.json`, and, only when `TWEETSTREAM_API_KEY` is set, the WebSocket connect and an `auth_ping` round trip to TweetStream. It never builds or sends a transaction. A trial key allows one WebSocket connection, so stop the bot before probing with it.

## The stages

The bot logs these for every signal and prints p50 and p95 on exit (Ctrl+C):

| Stage | Clock | What it covers |
| --- | --- | --- |
| `publish->receipt` | wall clock | the post's publish time (from its X id) to receipt on your machine; includes clock skew between X and you |
| `receipt->decision` | monotonic | JSON parse, dedupe, detection, rule matching, reservations |
| `safety read` | monotonic | the parallel checks: the one RPC read, plus market data and Rugcheck if a rule or `safety` needs them |
| `build+sign` | monotonic | the part of `decision->signed` after the checks returned |
| `decision->signed` | monotonic | everything between deciding and having signed bytes |
| `signed->first ack` | monotonic | until the first sender accepts the transaction; the report names that sender |
| `send->confirmed` | monotonic | until the RPC WebSocket reports the signature confirmed |

`publish->receipt` measures the feed hop, which [TweetStream's speed check](https://tweetstream.io/speedcheck) reports from X timestamp to TweetStream server receipt. The other stages are the bot's own. Image text (`tweet/meta`) arrives after the post, so a signal from OCR shows a larger `publish->receipt`.

Which sender's copy landed cannot be observed: every sender gets the same signed transaction, so the chain shows one signature. The report names the first sender to accept it.

## What the bot does at each stage

Feed. One WebSocket per key, with permessage-deflate off (inflating small JSON frames costs CPU) and TCP_NODELAY on. Each frame is parsed once and routed on `t` and `op` before the payload is read. With an Ultra Speed key and a standard key both connections stay open and warm, and the first copy of each event wins; the second is dropped before detection. Receipt is stamped with `process.hrtime` before parsing. `auth_ping` every 30 seconds catches a half-open connection that would otherwise go quiet for minutes.

Decision. No network call happens between receipt and the decision. Detectors are synchronous functions, the rule index (by account id, handle, and event type) is built at startup, and buy counts, cooldowns, and spend caps are reserved in memory before the first `await`. Logging happens after the trade has started, because console output is synchronous.

Chain read. One `getMultipleAccounts` call at `processed` commitment fetches the mint and its pump.fun bonding curve together. That read decides the route (local curve buy, PumpPortal, or Jupiter), the token program, the market cap and liquidity of a curve token, and the transfer fee. It is the only network call before the send on the pump.fun route. While it is in flight, the bot derives the buy's remaining program addresses (each derivation costs 100 to 200 µs in JavaScript).

pump.fun buys are built locally with `buy_exact_quote_in_v3`, following the official program IDL and docs (pump-fun/pump-public-docs). This saves the HTTPS round trip to a trade API. The PumpPortal trade-local API stays available as a fallback (`execution.pumpBuilder: "pumpportal"`) and is used automatically for curve coins the local builder does not handle (cashback coins and coins not paired with SOL). Tokens that left the curve go through Jupiter's order and execute API, which costs two round trips and lands the transaction itself.

Safety checks. pump.fun creates curve mints itself, with authorities the program fixes, so the mint and freeze authority checks are skipped on the curve. Elsewhere they run in parallel with any market data and Rugcheck call, all under `safety.budgetMs`.

Transaction. The blockhash is refreshed every 400 ms in the background, the priority fee estimate (75th percentile of nonzero fees in recent slots) every 2 seconds, the Jito tip level every 10 seconds, and the SOL price every 30 seconds; a buy reads them from memory. The compute unit limit is set (`execution.computeUnitLimit`), so the priority fee per unit is known. Signing uses Node's native Ed25519, about 5 times faster than the JavaScript signer.

Landing. The signed transaction goes to every RPC in `SOLANA_RPC_URLS`, to Jito, and to every extra sender in `senders`, all at once, with `skipPreflight: true` and `maxRetries: 0`. The bot rebroadcasts it every 1.5 seconds until it confirms or its blockhash expires. One transaction carries every tip (Jito and any sender's), so the same bytes can go everywhere; you pay each tip whichever sender lands it.

Confirmation. A `signatureSubscribe` subscription on the RPC WebSocket, opened at startup and subscribed before the send, pushes the result. Polling would add up to one poll interval.

## Benchmark

```sh
cd ts
npm run bench
```

It feeds 2,000 posts, each with a new mint, through the real decision and trading code in dry run. The chain read is answered from memory after a 1 ms wait that stands in for a nearby RPC. Our run, on an AMD EPYC 9354P with Node 22.23 (your numbers will differ):

```text
2000 posts, each with a new mint, on linux x64, Node v22.23.1
receipt->decision (JSON.parse + dedupe + detect + rules): p50 43.0 us, p95 54.6 us
safety read: p50 0.899 ms, p95 1.772 ms, n=2000
build+sign: p50 0.44 ms, p95 0.553 ms, n=2000
decision->signed: p50 1.441 ms, p95 2.327 ms, n=2000
```

So from receipt to a signed pump.fun buy the bot spends about 0.5 ms of its own time, plus one RPC round trip. Most of `build+sign` is compiling the v0 message and deriving the addresses the read did not cover.
