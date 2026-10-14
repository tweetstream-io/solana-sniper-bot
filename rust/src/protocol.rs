//! TweetStream v1 message types (only the fields this bot reads), post keys, event references,
//! and publish times from X post ids. Full contract: <https://tweetstream.io/docs/asyncapi.json>

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// One server message. Route on `t` (channel) and `op` (operation) before reading `d`.
#[derive(Debug, Deserialize)]
pub struct Envelope<'a> {
    /// Protocol version; this bot reads only version 1.
    pub v: Option<u64>,
    /// Channel: `tweet`, `account`, or `control`.
    #[serde(borrow)]
    pub t: Cow<'a, str>,
    /// Operation within the channel.
    #[serde(borrow)]
    pub op: Cow<'a, str>,
    /// Envelope id, when the server sets one.
    #[serde(borrow, default)]
    pub id: Option<Cow<'a, str>>,
    /// The payload, parsed only after routing.
    #[serde(borrow, default)]
    pub d: Option<&'a RawValue>,
}

/// A post author, account actor, or follow target. Handles arrive with a leading `@` in some
/// events, so rules match by id whenever they can.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Author {
    /// Numeric account id.
    pub id: Option<String>,
    /// Handle, with or without the leading `@`.
    pub handle: Option<String>,
    /// Display name.
    pub name: Option<String>,
    /// Profile bio.
    pub bio: Option<String>,
    /// `twitter`, `truth_social`, or `binance_square`.
    pub platform: Option<String>,
    /// Profile website, on account events.
    pub website_url: Option<String>,
}

/// A link attached to a post.
#[derive(Debug, Clone, Deserialize)]
pub struct TweetUrl {
    /// The expanded URL.
    pub url: String,
}

/// A post that this one quotes, replies to, or reposts. It can nest through `quoted` or `subtweet`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reference {
    /// `quote`, `reply`, or `retweet`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Id of the referenced post.
    pub tweet_id: String,
    /// Author of the referenced post.
    pub author: Option<Author>,
    /// Text of the referenced post.
    pub text: Option<String>,
    /// A post the referenced post quotes.
    pub quoted: Option<Box<Reference>>,
    /// A post the referenced post replies to or reposts.
    pub subtweet: Option<Box<Reference>>,
}

/// What a post is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PostKind {
    /// An original post.
    Post,
    /// A quote of another post.
    Quote,
    /// A reply.
    Reply,
    /// A repost.
    Retweet,
    /// A kind this bot does not know; no rule matches it.
    #[serde(other)]
    Other,
}

/// `tweet/content` and `tweet/update`. An update carries only the fields that changed;
/// `textUpdateType: "completion"` brings the full text of a post that first arrived shortened.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TweetPost {
    /// Post id.
    pub tweet_id: String,
    /// Platform, on updates.
    pub platform: Option<String>,
    /// Post text.
    pub text: Option<String>,
    /// Publish time in Unix ms, used off X where the id has no timestamp.
    pub created_at: Option<f64>,
    /// What the post is.
    pub kind: Option<PostKind>,
    /// Who wrote it.
    pub author: Option<Author>,
    /// The post it quotes, replies to, or reposts.
    #[serde(rename = "ref")]
    pub reference: Option<Reference>,
    /// Links attached to the post.
    pub urls: Option<Vec<TweetUrl>>,
}

/// A token TweetStream detected in a post.
#[derive(Debug, Clone, Deserialize)]
pub struct MetaToken {
    /// The contract address.
    pub contract: Option<String>,
    /// Ticker symbol.
    pub symbol: Option<String>,
    /// Where it was found: `text`, `ocr`, or both.
    pub sources: Option<Vec<String>>,
}

/// Tokens detected in a post.
#[derive(Debug, Clone, Deserialize)]
pub struct Detected {
    /// The detected tokens, in order.
    pub tokens: Option<Vec<MetaToken>>,
}

/// Text read from the post's images.
#[derive(Debug, Clone, Deserialize)]
pub struct Ocr {
    /// The image text.
    pub text: Option<String>,
}

/// `tweet/meta`: a complete snapshot. Replace the stored one, never merge.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TweetMeta {
    /// Post id.
    pub tweet_id: String,
    /// Platform of the post.
    pub platform: Option<String>,
    /// Tokens TweetStream detected.
    pub detected: Option<Detected>,
    /// Text read from images.
    pub ocr: Option<Ocr>,
}

/// `tweet/delete`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TweetDelete {
    /// Post id.
    pub tweet_id: String,
    /// Platform of the post.
    pub platform: Option<String>,
    /// Author of the post.
    pub author: Option<Author>,
    /// Unique event id.
    pub event_id: Option<String>,
}

/// The pinned post carried by a pin event.
#[derive(Debug, Deserialize)]
pub struct PinnedTweet {
    /// Post text.
    pub text: Option<String>,
    /// Links attached to the post.
    pub urls: Option<Vec<TweetUrl>>,
}

/// `tweet/pin`: a post was just pinned.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TweetPin {
    /// Post id.
    pub tweet_id: String,
    /// Platform of the post.
    pub platform: Option<String>,
    /// Who pinned it.
    pub author: Option<Author>,
    /// Unique event id.
    pub event_id: Option<String>,
    /// Post text, when the event has no full post.
    pub text: Option<String>,
    /// The pinned post.
    pub tweet: Option<PinnedTweet>,
}

/// `account/follow` and `account/unfollow`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountFollow {
    /// Who followed.
    pub actor: Author,
    /// Who was followed. `bio` and `websiteUrl` are present only when TweetStream has them.
    pub target: Author,
    /// Unique event id.
    pub event_id: Option<String>,
}

/// A profile field that distinguishes absent from `null`, because a change to `null` counts as a
/// change and an absent field does not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Field {
    /// Not in the payload.
    #[default]
    Absent,
    /// Present as `null`.
    Null,
    /// Present with a value.
    Value(String),
}

impl Field {
    /// Returns the value when it is a non-empty string.
    pub fn text(&self) -> Option<&str> {
        match self {
            Field::Value(text) if !text.is_empty() => Some(text),
            Field::Value(_) | Field::Null | Field::Absent => None,
        }
    }

    /// Returns true unless the field was absent.
    pub fn is_present(&self) -> bool {
        match self {
            Field::Absent => false,
            Field::Null | Field::Value(_) => true,
        }
    }
}

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Option::<String>::deserialize(deserializer)?;
        Ok(match value {
            Some(text) => Field::Value(text),
            None => Field::Null,
        })
    }
}

/// Profile fields in `account/profile_update` `changes` and `previous`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileFields {
    /// Avatar URL.
    #[serde(default)]
    pub avatar: Field,
    /// Banner URL.
    #[serde(default)]
    pub banner: Field,
    /// Bio text.
    #[serde(default)]
    pub bio: Field,
    /// Handle.
    #[serde(default)]
    pub handle: Field,
    /// Display name.
    #[serde(default)]
    pub name: Field,
    /// Website.
    #[serde(default)]
    pub website_url: Field,
}

/// `account/profile_update`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountProfileUpdate {
    /// Whose profile changed.
    pub actor: Author,
    /// The new values of the fields that changed.
    pub changes: ProfileFields,
    /// The values before the change, when TweetStream knows them.
    pub previous: Option<ProfileFields>,
    /// Unique event id.
    pub event_id: Option<String>,
}

/// A pump.fun caller.
#[derive(Debug, Deserialize)]
pub struct Caller {
    /// pump.fun username.
    pub username: String,
}

/// The token a callout is about.
#[derive(Debug, Deserialize)]
pub struct CalloutToken {
    /// Token address.
    pub address: String,
    /// Ticker symbol.
    pub symbol: Option<String>,
}

/// What a callout reports about the caller's own position.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallerPosition {
    /// Raw token amount as a decimal string; `"0"` means the caller holds none.
    pub amount_held: Option<String>,
}

/// `account/callout` and `account/callout_update`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountCallout {
    /// The caller.
    pub caller: Caller,
    /// The token called.
    pub token: CalloutToken,
    /// `callout`, `author_update`, `reply`, or `repost`.
    pub kind: Option<String>,
    /// Market cap at the time of the event.
    pub market_cap_usd: Option<f64>,
    /// How far the token ran since the call.
    pub multiple: Option<f64>,
    /// The caller's position.
    pub position: Option<CallerPosition>,
    /// Id of the call.
    pub callout_id: Option<String>,
    /// Id of the reply, for `reply` callouts.
    pub reply_id: Option<String>,
    /// Unique event id.
    pub event_id: Option<String>,
}

/// A FOMO coin. Market values are decimal strings observed at one moment.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FomoCoin {
    /// Coin address.
    pub address: String,
    /// Ticker symbol.
    pub symbol: Option<String>,
    /// Market cap as a decimal string.
    pub market_cap_usd: Option<String>,
}

/// A FOMO thesis author.
#[derive(Debug, Deserialize)]
pub struct FomoAuthor {
    /// Author id.
    pub id: String,
    /// Username.
    pub username: String,
}

/// The thesis itself.
#[derive(Debug, Deserialize)]
pub struct ThesisBody {
    /// Thesis id.
    pub id: Option<String>,
    /// Thesis text.
    pub text: String,
}

/// The author's position on the coin. Any `closedAt`, even `null`, means it was closed.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThesisPosition {
    /// When the author closed the position.
    #[serde(default, deserialize_with = "present")]
    pub closed_at: bool,
}

fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer)?;
    Ok(true)
}

/// `account/thesis`: a FOMO user's thesis on a coin.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountThesis {
    /// The coin.
    pub coin: FomoCoin,
    /// Who wrote the thesis.
    pub author: FomoAuthor,
    /// The thesis.
    pub thesis: ThesisBody,
    /// The author's position.
    pub position: Option<ThesisPosition>,
    /// Unique event id.
    pub event_id: Option<String>,
}

/// `account/token_verified`: the first observed verification of a tracked coin.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountTokenVerified {
    /// The coin.
    pub coin: FomoCoin,
    /// Unique event id.
    pub event_id: Option<String>,
}

/// Returns the post key `<platform>:<tweetId>`. Content carries the platform on its author;
/// later operations carry it on `d.platform`. A missing platform means twitter.
pub fn post_key(tweet_id: &str, platform: Option<&str>, author: Option<&Author>) -> String {
    let author_platform = author.and_then(|a| a.platform.as_deref());
    format!("{}:{tweet_id}", platform.or(author_platform).unwrap_or("twitter"))
}

/// Returns the handle without surrounding space or a leading `@`, or `None` when nothing is left.
pub fn normalize_handle(handle: Option<&str>) -> Option<String> {
    let handle = handle?.trim();
    let handle = handle.strip_prefix('@').unwrap_or(handle);
    if handle.is_empty() { None } else { Some(handle.to_string()) }
}

const TWITTER_EPOCH_MS: u64 = 1_288_834_974_657;

/// Returns the publish time in Unix ms embedded in an X post id: `(id >> 22) + epoch`.
///
/// X ids are 64-bit; the id must be 1 to 20 digits and below 2^64.
///
/// # Examples
///
/// ```
/// use solana_sniper_bot::protocol::snowflake_ms;
/// assert_eq!(snowflake_ms("2026541002650550272"), Some(1_772_000_001_000));
/// assert_eq!(snowflake_ms("not-a-number"), None);
/// ```
pub fn snowflake_ms(tweet_id: &str) -> Option<i64> {
    if tweet_id.is_empty() || tweet_id.len() > 20 || !tweet_id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id: u64 = tweet_id.parse().ok()?;
    i64::try_from((id >> 22) + TWITTER_EPOCH_MS).ok()
}

/// Returns the publish time of a post: from the id on X, from `createdAt` elsewhere.
pub fn published_at_ms(platform: &str, tweet_id: &str, created_at: Option<f64>) -> Option<i64> {
    let from_id = if platform == "twitter" { snowflake_ms(tweet_id) } else { None };
    from_id.or(created_at.map(|ms| ms as i64))
}

/// A payload routed by its `t` and `op`.
#[derive(Debug)]
pub enum Payload {
    /// `tweet/content`.
    Content(TweetPost),
    /// `tweet/update`.
    Update(TweetPost),
    /// `tweet/meta`.
    Meta(TweetMeta),
    /// `tweet/pin`.
    Pin(TweetPin),
    /// `tweet/delete`.
    Delete(TweetDelete),
    /// `account/follow`.
    Follow(AccountFollow),
    /// `account/unfollow`.
    Unfollow(AccountFollow),
    /// `account/profile_update`.
    Profile(AccountProfileUpdate),
    /// `account/callout`.
    Callout(AccountCallout),
    /// `account/callout_update`.
    CalloutUpdate(AccountCallout),
    /// `account/thesis`.
    Thesis(AccountThesis),
    /// `account/token_verified`.
    Verified(AccountTokenVerified),
}

fn parse<T: serde::de::DeserializeOwned>(envelope: &Envelope) -> Result<T, String> {
    let text = envelope.d.map_or("{}", |d| d.get());
    serde_json::from_str(text).map_err(|e| e.to_string())
}

/// Parses the payload of a pair this bot reads and checks the fields it needs. Returns `None` for
/// any other pair, and `Some(Err)` with the reason for a payload that does not match the protocol.
pub fn decode(envelope: &Envelope) -> Option<Result<Payload, String>> {
    let decoded = match (envelope.t.as_ref(), envelope.op.as_ref()) {
        ("tweet", "content") => parse(envelope).map(Payload::Content),
        ("tweet", "update") => parse(envelope).map(Payload::Update),
        ("tweet", "meta") => parse(envelope).map(Payload::Meta),
        ("tweet", "pin") => parse(envelope).map(Payload::Pin),
        ("tweet", "delete") => parse(envelope).map(Payload::Delete),
        ("account", "follow") => parse(envelope).map(Payload::Follow),
        ("account", "unfollow") => parse(envelope).map(Payload::Unfollow),
        ("account", "profile_update") => parse(envelope).map(Payload::Profile),
        ("account", "callout") => parse(envelope).map(Payload::Callout),
        ("account", "callout_update") => parse(envelope).map(Payload::CalloutUpdate),
        ("account", "thesis") => parse(envelope).map(Payload::Thesis),
        ("account", "token_verified") => parse(envelope).map(Payload::Verified),
        _ => return None,
    };
    Some(decoded.and_then(|payload| payload_problem(&payload).map_or(Ok(payload), Err)))
}

/// Returns the first field the detectors read that is missing or empty. `tweet/content` must carry
/// the whole post: text, author, and a known kind.
fn payload_problem(payload: &Payload) -> Option<String> {
    let text = |value: Option<&String>| value.is_some_and(|v| !v.is_empty());
    let need = |ok: bool, what: &str| if ok { None } else { Some(format!("missing or invalid {what}")) };
    match payload {
        Payload::Content(d) => need(!d.tweet_id.is_empty(), "tweetId")
            .or_else(|| need(d.text.is_some(), "text"))
            .or_else(|| need(d.author.is_some(), "author"))
            .or_else(|| need(d.kind.is_some_and(|k| k != PostKind::Other), "kind")),
        Payload::Update(d) => need(!d.tweet_id.is_empty(), "tweetId").or_else(|| need(d.kind != Some(PostKind::Other), "kind")),
        Payload::Meta(d) => need(!d.tweet_id.is_empty(), "tweetId"),
        Payload::Delete(d) => need(!d.tweet_id.is_empty(), "tweetId"),
        Payload::Pin(d) => need(!d.tweet_id.is_empty(), "tweetId")
            .or_else(|| need(d.author.is_some(), "author"))
            .or_else(|| need(text(d.event_id.as_ref()), "eventId")),
        Payload::Follow(d) | Payload::Unfollow(d) => {
            need(text(d.target.handle.as_ref()), "target.handle").or_else(|| need(text(d.event_id.as_ref()), "eventId"))
        }
        Payload::Profile(d) => need(text(d.event_id.as_ref()), "eventId"),
        Payload::Callout(d) | Payload::CalloutUpdate(d) => need(!d.caller.username.is_empty(), "caller.username")
            .or_else(|| need(!d.token.address.is_empty(), "token.address"))
            .or_else(|| need(text(d.callout_id.as_ref()), "calloutId")),
        Payload::Thesis(d) => need(!d.coin.address.is_empty(), "coin.address")
            .or_else(|| need(!d.author.id.is_empty() && !d.author.username.is_empty(), "author"))
            .or_else(|| need(text(d.thesis.id.as_ref()), "thesis")),
        Payload::Verified(d) => need(!d.coin.address.is_empty(), "coin.address"),
    }
}

/// The name of an event and the identities dedupe compares.
#[derive(Debug, Clone, Default)]
pub struct EventKeys {
    /// `"<t>/<op> <key>"`, used in logs, the latency report, and `shared/fixtures`.
    pub event_ref: String,
    /// The payload's `eventId`.
    pub event_id: Option<String>,
    /// The post key of a `tweet/*` event.
    pub post_key: Option<String>,
    /// An id the payload type carries: `<calloutId>:<kind>:<replyId>`, a thesis id, a coin.
    pub payload_id: Option<String>,
}

/// Returns the event reference and identities of a payload. Posts are named by post key; other
/// events by `d.eventId`, else the envelope id, else an id from the payload.
pub fn event_ref(pair: &str, envelope_id: Option<&str>, payload: &Payload) -> EventKeys {
    let (event_id, post, payload_id) = match payload {
        Payload::Content(d) | Payload::Update(d) => (None, Some(post_key(&d.tweet_id, d.platform.as_deref(), d.author.as_ref())), None),
        Payload::Meta(d) => (None, Some(post_key(&d.tweet_id, d.platform.as_deref(), None)), None),
        Payload::Pin(d) => (d.event_id.clone(), Some(post_key(&d.tweet_id, d.platform.as_deref(), d.author.as_ref())), None),
        Payload::Delete(d) => (d.event_id.clone(), Some(post_key(&d.tweet_id, d.platform.as_deref(), d.author.as_ref())), None),
        Payload::Follow(d) | Payload::Unfollow(d) => (d.event_id.clone(), None, None),
        Payload::Profile(d) => (d.event_id.clone(), None, None),
        Payload::Callout(d) | Payload::CalloutUpdate(d) => {
            let call = d
                .callout_id
                .as_ref()
                .map(|id| format!("{id}:{}:{}", d.kind.as_deref().unwrap_or("callout"), d.reply_id.as_deref().unwrap_or("")));
            (d.event_id.clone(), None, call)
        }
        Payload::Thesis(d) => (d.event_id.clone(), None, d.thesis.id.clone()),
        Payload::Verified(d) => (d.event_id.clone(), None, Some(d.coin.address.clone())),
    };
    let name = match &post {
        Some(key) => key.clone(),
        None => event_id.clone().or(envelope_id.map(str::to_string)).or(payload_id.clone()).unwrap_or_default(),
    };
    EventKeys { event_ref: format!("{pair} {name}"), event_id, post_key: post, payload_id }
}
