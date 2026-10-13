# Safety and risks

Sniping memecoins loses money more often than it makes it. This bot reduces some avoidable losses; it cannot make a bad token good. Nothing here is financial advice.

## Risks no code removes

Hijacked accounts. Attackers take over well-known X accounts and post a contract address for a token they created, then sell into the buyers. A sniper that buys every address a famous account posts is the target of this attack. Watch accounts by numeric id, keep amounts small, and consider `requireMintInBio`, which also requires the address in the account's bio: an attacker who only posts rarely edits the bio too.

Scam and copycat tokens. Many tokens share a name or ticker. The bot buys addresses, never names, and an address posted by the right account can still be a scam.

Rugs and dumps. Developers and early holders can sell minutes after launch. The bonding curve price moves with every buy, so a bot that buys late buys someone else's exit.

Losses from speed. Faster bots buy before you and sell to you. Slippage that is too low makes fast-moving buys fail; too high, and you pay far more than the price you saw.

Your own mistakes. A wrong handle, a stale config, or a rule that matches more than you meant can buy tokens you never wanted. Run in dry run first and read the logs.

## What the bot checks

All checks for one buy start together and share one time budget, `safety.budgetMs` (150 ms by default). A check that fails, errors, or runs out of time counts as a failure unless you set `safety.onUnknown` to `allow`. Unknown never counts as safe by default.

| Check | Where the data comes from | Default |
| --- | --- | --- |
| Address is a mint | the mint account must be owned by the SPL Token or Token-2022 program; chart links sometimes point at a pool | always on |
| Mint authority | mint account; someone could still create more tokens | reject |
| Freeze authority | mint account; someone could freeze your tokens | reject |
| Risky Token-2022 extensions | mint account: transfer hook, permanent delegate, default-frozen accounts, pausable, non-transferable | reject |
| Transfer fee | Token-2022 transfer fee config | `maxPoolTaxPct` 0 |
| Market cap, liquidity, age, venue | the curve itself for pump.fun tokens; the event's own market cap for callouts and FOMO; DexScreener for other tokens | off until a rule sets a limit |
| Rugcheck | `api.rugcheck.xyz` report summary: danger-level risks and the normalised score | off |

pump.fun curve tokens skip the authority and extension checks: pump.fun creates those mints itself, with authorities its program fixes, so a creator cannot keep the ability to mint more or freeze holders. The checks run for every other token.

Rugcheck rarely has a report for a token minutes old inside a 150 ms budget, so `safety.rugcheck: "advisory"` rejects only on a danger result that arrives in time, and `"required"` treats no answer as a failure. Market data from DexScreener can also lag a new launch; a rule with a market filter and `onMissingData: "reject"` skips tokens it cannot price yet.

## Limits that hold across restarts

`state/ledger.json` records every mint the bot traded, the snipes each rule used, the SOL spent, and open positions. It is written after every change with a temp file and a rename, so a crash never leaves half a file. If it is damaged the bot refuses to start, because deleting it lets the bot buy the same tokens again.

- One trade per mint: the first signal reserves the mint before any network call, so two signals in the same millisecond cannot both buy. A buy whose outcome is unknown (sent, never confirmed) also blocks the mint.
- `filters.maxSnipes` (1 by default) and `filters.cooldownSec` per rule.
- `spend.maxPerTradeSol`, `spend.maxPerHourSol`, and `spend.maxTotalSol`, reserved before the transaction is built and refunded only when nothing was sent or the transaction failed on chain.
- `maxSignalAgeMs` (5 seconds) skips posts that arrive late, for example after a reconnect.

## Exits

Open positions are priced every `exits.pollMs`: pump.fun curve tokens from one account read, others from a Jupiter quote. The bot sells at `takeProfitPct`, `stopLossPct`, or after `maxHoldSec`. It also sells when the post it bought on is deleted (`sellOnDelete`), when a tracked pump.fun caller reports holding none of the token (`sellWhenCallerExits`), and when a tracked FOMO author closes the position (`sellWhenAuthorExits`). In dry run every exit is logged and nothing is sent.

## Going live

1. Run in dry run until every signal and skip in the log is one you expected.
2. Create a new wallet for the bot and fund it with what you can lose. Put its base58 secret key in `WALLET_SECRET_KEY` in `.env`, which git ignores. The key never leaves your machine: the bot signs locally and sends signed transactions to your own RPC and Jito endpoints.
3. Set the spend caps in `config.json` to amounts you accept losing in full.
4. Set `DRY_RUN=false`. The bot prints a banner with the wallet and the caps before it connects.
