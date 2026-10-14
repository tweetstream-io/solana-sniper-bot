# Rust version

The same bot as `ts/`, in Rust. It reads the same `.env` and `config.json` from the repository root, validates the config against `shared/config.schema.json`, loads the address rules from `shared/addresses.json`, and keeps its trade state in `state/live.json` or `state/dry-run.json` in the format of `shared/ledger.schema.json`. The two bots can read each other's state files. It passes every case in `shared/fixtures/`.

The modules follow the TypeScript files one for one (`src/feed/client.rs` for `ts/src/feed/client.ts`, and so on), so the two can be read side by side. [docs/how-it-works.md](../docs/how-it-works.md) walks through the path of one event.

## Run it

Install Rust with [rustup](https://rustup.rs); `rust-toolchain.toml` picks the stable toolchain. From `rust/`:

```sh
cargo run --release -- --demo   # the local demo, no key needed
cargo run --release             # the live feed, in dry run until DRY_RUN=false
```

The demo starts the TypeScript demo feed (`ts/scripts/demo-feed.ts`), so it needs Node.js 22.18 or later and `npm install` run once in `ts/`. It replays `shared/demo/scenario.json` in dry run, answers chain reads from the scenario, and makes the same decisions as `npm run demo`.

Setup is the same as for the TypeScript bot: see [Run it on live X data](../README.md#run-it-on-live-x-data) and [docs/safety.md](../docs/safety.md) before setting `DRY_RUN=false`.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo bench
```

`cargo test` runs the shared event, config, and ledger fixtures (`tests/fixtures.rs`), the trade state file and its lock (`tests/trades.rs`), trades end to end against local stand-ins for an RPC, Jito, and the RPC WebSocket (`tests/trade.rs`), and the on-chain layouts, the caches, the safety gate, landing, and the feed handshake.

## Speed

`cargo bench` feeds 2,000 posts, each with a new mint, through the decider and the trader in dry run, the same events as `npm run bench`. The chain read is answered from memory after a 1 ms wait that stands in for a nearby RPC. Both ran on the same machine, an AMD EPYC 9354P with other work on it (load average about 20), Rust 1.99 and Node 22.23. Your numbers will differ.

| Stage | Rust p50 | Rust p95 | TypeScript p50 | TypeScript p95 |
| --- | --- | --- | --- | --- |
| receipt->decision (JSON parse, dedupe, detect, rules) | 10.9 µs | 19.0 µs | 97.1 µs | 415.9 µs |
| build+sign | 0.030 ms | 0.047 ms | 0.457 ms | 0.697 ms |
| decision->signed (includes the 1 ms read) | 1.095 ms | 1.118 ms | 1.505 ms | 2.367 ms |

Most of the remaining Rust time between receipt and a signed buy is the chain read itself.

## How it is built

- tokio on its multi-thread runtime: each feed connection reads on its own task and runs the decision inline, so the two connections, the background refreshes, and trades in flight do not wait for each other.
- tokio-tungstenite 0.30 with rustls and TCP_NODELAY; permessage-deflate is not offered, so frames arrive uncompressed.
- reqwest 0.13 with rustls, a shared keep-alive pool, and HTTP/2 when the endpoint offers it.
- serde_json 1, parsing the envelope once and each payload only after routing on `t` and `op`.
- The split Solana crates: solana-pubkey 4.4, solana-instruction 4.0, solana-message 5.1, solana-transaction 5.1, solana-keypair 4.0, solana-signer 4.0, solana-signature 3.6, solana-hash 4.7, solana-compute-budget-interface 3.1, solana-system-interface 3.3, with wincode 0.6 for the wire format.
- jsonschema 0.58 for `config.schema.json` and `ledger.schema.json`.
- libc, for the one call that asks whether the process named in a state lock still runs.

A trade is written to the state file with fsync before it is sent, as `shared/README.md` requires. The write happens outside the lock that the rule engine takes, so a decision never waits on the disk, and the reconciler only sees a trade once its write has completed.
