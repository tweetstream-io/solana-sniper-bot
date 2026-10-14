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
| Transfer fee | Token-2022 transfer fee config | `maxPoolTaxPct` 0 |
| On a pump.fun curve | the curve account: the coin must still be on its bonding curve and quoted in SOL; anything else is skipped as `unsupported_curve` | always on |
| Market cap, liquidity, age | the curve itself; the event's own market cap for callouts and FOMO; DexScreener for the age | off until a rule sets a limit |
| Rugcheck | `api.rugcheck.xyz` report summary: danger-level risks and the normalised score | off |

pump.fun creates curve mints itself, with authorities its program fixes, so a creator cannot keep the ability to mint more or freeze holders. That is why the bot buys only on the curve and needs no authority checks.

Rugcheck rarely has a report for a token minutes old inside a 150 ms budget, so `safety.rugcheck: "advisory"` rejects only on a danger result that arrives in time, and `"required"` treats no answer as a failure. Market data from DexScreener can also lag a new launch; a rule with a market filter and `onMissingData: "reject"` skips tokens it cannot price yet.

## Limits that hold across restarts

`state/live.json` records the mints bought, the SOL spent, open positions, and every trade sent whose outcome is not known yet. Dry runs use `state/dry-run.json`, so a dry run never uses up live limits. Each write goes to a temp file, is flushed to disk, and replaces the old file, so a crash leaves either the old state or the new one. If the file is damaged, of another version, or in use by another running bot, the bot refuses to start, because deleting it lets the bot buy the same tokens again.

- A transaction is written to the state file as pending, with its signature and blockhash, before it is sent. If that write fails the bot does not send. After a crash, the bot checks every pending signature on chain; a buy that landed opens its position then.
- A pending buy keeps its spend and blocks the mint until its signature confirms, or until its blockhash is proven expired with the signature never seen.
- One buy per mint, ever: a mint stays bought after the position is sold.
- `filters.maxSnipes` (1 by default) counts each rule's buys; `filters.cooldownSec` spaces them out.
- `spend.maxPerTradeSol`, `spend.maxPerHourSol`, and `spend.maxTotalSol` count the largest amount a buy can take from the wallet: the amount, fees, the tip, and account rent. The reservation becomes the actual cost once it is known, and is refunded only when the transaction expired. A sell reserves only its fees and tip and is never refused by the caps, so an exit always goes out.
- `maxSignalAgeMs` (5 seconds) skips posts that arrive late, for example after a reconnect.

## Exits

Open positions are priced every `exits.pollMs`: from one read of the curve, or from a Jupiter quote once the curve has graduated, and then they are sold through Jupiter. The bot sells at `takeProfitPct`, `stopLossPct`, or after `maxHoldSec`, and when the post it bought on is deleted (`sellOnDelete`). An exit stays on the position until a sell confirms, so a sell that fails is tried again. A position has one sell in flight at a time. In dry run every exit is logged and nothing is sent.

## Going live

1. Run in dry run until every signal and skip in the log is one you expected.
2. Create a new wallet for the bot and fund it with what you can lose. Put its base58 secret key in `WALLET_SECRET_KEY` in `.env`, which git ignores. The key never leaves your machine: the bot signs locally and sends signed transactions to your own RPC and Jito endpoints.
3. Set the spend caps in `config.json` to amounts you accept losing in full.
4. Set `DRY_RUN=false`. The bot prints a banner with the wallet and the caps before it connects.
