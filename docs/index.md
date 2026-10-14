# Solana sniper bot for X/Twitter signals

This Solana sniper bot buys Solana and pump.fun tokens the moment a watched X account posts a contract address, changes its bio, follows someone, or when a pump.fun caller or FOMO user you track posts about a token. It runs on the [TweetStream](https://tweetstream.io/signup?offer=basic-monthly-trial&utm_source=github&utm_medium=referral&utm_campaign=solana-sniper-bot&utm_content=docs-index) real-time feed.

The code and a quickstart are on [GitHub](https://github.com/tweetstream-io/solana-sniper-bot). These pages explain how it works:

- [How it works](how-it-works.md): the path of one event through the code, the TweetStream events used, and the external APIs.
- [Rule modes](modes.md): the seven ways a rule can fire, with a minimal config for each.
- [Configuration](config.md): every field, its default, and what it costs in latency or risk.
- [Latency](latency.md): where the milliseconds go, where to run the bot, and how to measure your setup.
- [Safety and risks](safety.md): the checks the bot runs and the losses it cannot prevent.

Every TweetStream plan works with the bot. For competitive sniping, [Ultra Speed](https://tweetstream.io/signup?intent=ultra_speed&ultraBilling=MONTHLY&utm_source=github&utm_medium=referral&utm_campaign=solana-sniper-bot&utm_content=docs-index) gives first-send priority to 10 to 100 X accounts you select. Plans are on the [pricing page](https://tweetstream.io/pricing?utm_source=github&utm_medium=referral&utm_campaign=solana-sniper-bot&utm_content=docs-index).

This is example code, not financial advice. Trading memecoins loses money more often than it makes it.
