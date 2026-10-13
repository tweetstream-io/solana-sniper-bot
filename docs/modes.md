# Rule modes

Each rule in `config.json` has one `mode`. The examples below are complete rules: everything left out takes its default from [config.md](config.md). Put a rule in the `rules` list and set `"enabled": true` (the default).

Account fields take a numeric X account id (`"44196397"`) or a handle (`"@name"`). Prefer ids: a handle can be renamed or taken over by someone else, and the bot warns at startup for every handle it watches. The accounts must also be tracked in your TweetStream dashboard, or no events arrive for them.

Every mode can buy (`"action": "buy"`, spending `amountSol`) or sell the whole position in a token (`"action": "sell"`). Before anything is sent, the rule's filters and the safety checks run; see [safety.md](safety.md).

## ca_scanner

Use it when an account posts contract addresses you want to buy straight away: a token's developer, a project account, a trader you follow.

```json
{ "name": "dev-ca", "mode": "ca_scanner", "accounts": ["44196397"], "token": { "amountSol": 0.05 } }
```

It listens to `tweet/content` and `tweet/update` (address in the text, in a token link, or in a quoted, replied, or reposted post), `tweet/meta` (tokens TweetStream detected, and text read from images), `tweet/pin`, and `account/profile_update` (an address newly added to the bio, name, or website). Limit them with `sources`, and raise `minConfidence` to `high` to ignore image text and quoted posts.

Latency: an address in the post text fires on the first `tweet/content`. Image text arrives in a later `tweet/meta` event, so an OCR signal fires later than a text signal for the same post. Set `token.mint` to act only on one known address.

## engagement

Use it when a buy signal is an account reacting to something: a well-known account quoting a launch post, or replying to a developer.

```json
{
  "name": "kol-quotes-dev",
  "mode": "engagement",
  "reactor": "44196397",
  "reactedTo": ["@devhandle"],
  "actions": ["quote", "reply"],
  "excludeKeywords": { "words": ["scam", "rug"], "scanDepth": 1 },
  "useDetectedCa": true,
  "token": { "amountSol": 0.05 }
}
```

It fires on a `tweet/content` from `reactor` whose `kind` is `retweet`, `quote`, or `reply`, when the post it references is `targetTweetId` or was written by an account in `reactedTo`. Likes are private on X, so they are not available.

Keyword filters read text by depth: `scanDepth` 0 reads only the reactor's own text, 1 adds the post it reacted to, 2 adds the post that one references. With `useDetectedCa` the rule buys the first address in those texts; otherwise set `token.mint`.

## new_tweet

Use it to buy a known token when an account posts at all, or posts a word. A celebrity posting "dog" when a dog token already trades is the usual case.

```json
{
  "name": "keyword-post",
  "mode": "new_tweet",
  "accounts": ["44196397"],
  "events": ["post"],
  "includeKeywords": { "words": ["dog"], "scanDepth": 0 },
  "token": { "mint": "PASTE_THE_MINT", "amountSol": 0.05 }
}
```

`events` picks the actions that count: `post`, `retweet`, `quote`, `reply`, and `bio` (a bio change, read from `account/profile_update`). Several new tokens often share one word, so the rule names the mint itself. Set `useDetectedCa` to buy an address found in the post instead.

## follow

Use it when an account following someone is the signal, for example a developer following a new project account whose bio holds the token address.

```json
{ "name": "follow-bio", "mode": "follow", "accounts": ["44196397"], "useDetectedCa": true, "token": { "amountSol": 0.05 } }
```

It listens to `account/follow` and `account/unfollow` (`followEvents`), optionally only for some followed accounts (`targets`). With `useDetectedCa` it buys the first address in the followed account's bio or website when the event includes the bio. When it does not, the rule logs `bio_not_in_payload`; this example does not call a profile lookup API. An unfollow with `"action": "sell"` exits a position.

## pfp_update

Use it when an account changing its picture tends to come before a launch.

```json
{ "name": "pfp-change", "mode": "pfp_update", "accounts": ["44196397"], "fields": ["avatar"], "token": { "mint": "PASTE_THE_MINT", "amountSol": 0.05 } }
```

It listens to `account/profile_update` and fires when one of `fields` changed: `avatar`, `banner`, `name`, `bio`, or `websiteUrl`. A value that did not change does not count.

## callout

Use it to follow pump.fun callers: when a caller you track calls a token, buy that token.

```json
{ "name": "pump-callouts", "mode": "callout", "callers": ["CallerName"], "maxMultiple": 2, "token": { "amountSol": 0.05 }, "filters": { "maxMarketCapUsd": 250000 } }
```

It listens to `account/callout`. Add the callers in the dashboard's Callouts tab too. `calloutKinds` chooses which posts count (`callout` by default; `author_update`, `reply`, and `repost` are later posts about an earlier call). `maxMultiple` skips a call when the event says the token already ran past that multiple. `requireCallerPosition` fires only when the event shows the caller holding the token. The callout's own market cap feeds the `maxMarketCapUsd` filter.

`account/callout_update` only adds detail to an earlier call, so it never buys. With `sellWhenCallerExits`, an update or callout from the same caller that reports `position.amountHeld` of 0 sells the position this rule bought. Callouts are live only.

## fomo

Use it to follow FOMO users or coins: a thesis from a user you track, any thesis on a coin you track, or a coin's first observed verification.

```json
{ "name": "fomo-theses", "mode": "fomo", "fomoUsers": ["username"], "fomoCoins": [], "fomoEvents": ["thesis"], "excludeKeywords": { "words": ["exit"], "scanDepth": 0 }, "token": { "amountSol": 0.05 } }
```

It listens to `account/thesis` and `account/token_verified`. Track users and coins in the dashboard's FOMO tab too. Keyword filters read the thesis text. `token_verified` fires only for coins in `fomoCoins`, once per coin; TweetStream reports when it first observed the verification, which is not a safety or investment signal. With `sellWhenAuthorExits`, a thesis from a watched author that reports `position.closedAt` sells this rule's position and never buys. FOMO events are live only.
