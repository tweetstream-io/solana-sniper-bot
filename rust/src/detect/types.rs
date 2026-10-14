//! What the detectors produce. A [`Signal`] is one contract address found in one event; an
//! [`Activity`] is what an account did (posted, followed, changed its profile) plus the signals it
//! carried. Next: `rules/engine.rs` matches activities against the configured rules.

use serde::{Deserialize, Serialize};

use crate::protocol::PostKind;

/// How much a signal can be trusted to name the token the account meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// No baseline or a chart link that may name a pool.
    Low,
    /// Image text, quoted posts, followed bios, display names.
    Medium,
    /// Post text, token links, pins, callouts, theses.
    High,
}

/// Where the address was found. These names are the `sources` a CA scanner rule can allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalSource {
    /// Written in the post text.
    Text,
    /// A token link in the post's URLs or text.
    Url,
    /// Detected by TweetStream enrichment (`tweet/meta` `detected.tokens`).
    Meta,
    /// Read from an image (`tweet/meta` `ocr.text`).
    Ocr,
    /// Inside the post that was quoted, replied to, or reposted.
    Reference,
    /// In a newly pinned post.
    Pin,
    /// Newly added to the account's bio, name, or website.
    Profile,
    /// In the bio of an account that was just followed.
    FollowBio,
    /// The token of an `account/callout` event.
    Callout,
    /// The coin of an `account/thesis` or `account/token_verified` event.
    Fomo,
}

/// The chain an address belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Chain {
    /// Base58 that decodes to exactly 32 bytes.
    Solana,
    /// `0x` and 40 hex digits; logged, never traded.
    Evm,
}

/// Which kind of account acted. Post authors are qualified by platform, so an X rule never matches
/// a Truth Social or Binance Square author that shares an id or handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Namespace {
    /// An X account.
    X,
    /// A post author on another platform, named as the protocol names it (`truth_social`, ...).
    Platform(String),
    /// A pump.fun caller.
    PumpFun,
    /// A FOMO author.
    Fomo,
    /// No actor, as in a token verification.
    None,
}

impl Namespace {
    /// Returns the namespace of a post author on a platform; `twitter` is `x`.
    pub fn for_platform(platform: &str) -> Namespace {
        if platform == "twitter" { Namespace::X } else { Namespace::Platform(platform.to_string()) }
    }

    /// Returns the name used in signal JSON and logs.
    pub fn as_str(&self) -> &str {
        match self {
            Namespace::X => "x",
            Namespace::Platform(platform) => platform,
            Namespace::PumpFun => "pump_fun",
            Namespace::Fomo => "fomo",
            Namespace::None => "none",
        }
    }

    /// Returns the platform whose profiles this namespace's ids name, for the profile store.
    pub fn profile_platform(&self) -> Option<&str> {
        match self {
            Namespace::X => Some("twitter"),
            Namespace::Platform(platform) => Some(platform),
            Namespace::PumpFun | Namespace::Fomo | Namespace::None => None,
        }
    }
}

impl Serialize for Namespace {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Who acted. Post authors have an id and a handle, pump.fun callers a username, FOMO authors an id
/// and a username. Token verifications have no actor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Actor {
    /// Which kind of account.
    pub namespace: Namespace,
    /// Account id, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Handle or username without a leading `@`, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
}

/// One contract address found in one event.
#[derive(Debug, Clone)]
pub struct Signal {
    /// Where it was found.
    pub source: SignalSource,
    /// Solana or EVM.
    pub chain: Chain,
    /// The address; EVM addresses are lowercased.
    pub mint: String,
    /// Who posted it.
    pub actor: Actor,
    /// `"tweet/content twitter:123"`, for logs and the latency report.
    pub event_ref: String,
    /// How much to trust it.
    pub confidence: Confidence,
    /// Free text for logs.
    pub detail: String,
    /// The post that carried it, so deleting that post can trigger an exit.
    pub post_key: Option<String>,
    /// Market cap, only when the event itself carries one.
    pub market_cap_usd: Option<f64>,
}

/// The post a post quotes, replies to, or reposts (the direct reference only).
#[derive(Debug, Clone)]
pub struct PostTarget {
    /// Id of the referenced post.
    pub tweet_id: String,
    /// Its author.
    pub account: Actor,
}

/// An account posted, quoted, replied, or reposted.
#[derive(Debug, Clone)]
pub struct PostActivity {
    /// What the post is.
    pub kind: PostKind,
    /// Post key of this post.
    pub post_key: String,
    /// The direct reference.
    pub target: Option<PostTarget>,
    /// `texts[0]` is the post's own text, `texts[1]` the referenced post, `texts[2]` one level deeper.
    pub texts: Vec<Vec<String>>,
}

/// Follow or unfollow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FollowOp {
    /// `account/follow`.
    Follow,
    /// `account/unfollow`.
    Unfollow,
}

/// An account followed or unfollowed someone.
#[derive(Debug, Clone)]
pub struct FollowActivity {
    /// Follow or unfollow.
    pub op: FollowOp,
    /// Who was followed.
    pub target: Actor,
    /// Whether the payload carried the target's bio.
    pub bio_in_payload: bool,
}

/// A profile field a pfp_update rule can watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProfileField {
    /// Profile picture.
    Avatar,
    /// Header image.
    Banner,
    /// Display name.
    Name,
    /// Bio text.
    Bio,
    /// Website.
    WebsiteUrl,
}

/// An account changed its profile.
#[derive(Debug, Clone)]
pub struct ProfileActivity {
    /// The fields whose value changed.
    pub changed: Vec<ProfileField>,
    /// The new bio, when it changed.
    pub bio: Option<String>,
}

/// The kind of a pump.fun callout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalloutKind {
    /// A new call.
    Callout,
    /// The caller's later post about the call.
    AuthorUpdate,
    /// A reply about the call.
    Reply,
    /// A repost of the call.
    Repost,
}

impl CalloutKind {
    /// Parses the protocol name, defaulting to `callout` when absent.
    pub fn parse(kind: Option<&str>) -> Option<CalloutKind> {
        match kind.unwrap_or("callout") {
            "callout" => Some(CalloutKind::Callout),
            "author_update" => Some(CalloutKind::AuthorUpdate),
            "reply" => Some(CalloutKind::Reply),
            "repost" => Some(CalloutKind::Repost),
            _ => None,
        }
    }
}

/// Whether a callout event is a new call or enriches an earlier one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalloutOp {
    /// `account/callout`.
    Call,
    /// `account/callout_update`: never a new call.
    Update,
}

/// A pump.fun caller posted about a token.
#[derive(Debug, Clone)]
pub struct CalloutActivity {
    /// Kind of post; `None` for a kind this bot does not know.
    pub kind: Option<CalloutKind>,
    /// Call or update.
    pub op: CalloutOp,
    /// How far the token ran since the call.
    pub multiple: Option<f64>,
    /// From `position.amountHeld`; `None` when the event has no position.
    pub caller_holds: Option<bool>,
}

/// The FOMO event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FomoOp {
    /// `account/thesis`.
    Thesis,
    /// `account/token_verified`.
    TokenVerified,
}

/// A FOMO thesis or a token verification.
#[derive(Debug, Clone)]
pub struct FomoActivity {
    /// Thesis or verification.
    pub op: FomoOp,
    /// The coin address as sent.
    pub coin: String,
    /// Thesis text; empty for a verification.
    pub text: String,
    /// The thesis position reports `closedAt`.
    pub author_exited: bool,
}

/// What happened, by event type.
#[derive(Debug, Clone)]
pub enum ActivityKind {
    /// A post.
    Post(PostActivity),
    /// A follow or unfollow.
    Follow(FollowActivity),
    /// A profile change.
    Profile(ProfileActivity),
    /// A pin; only the CA scanner reads it.
    Pin,
    /// A pump.fun callout.
    Callout(CalloutActivity),
    /// A FOMO thesis or verification.
    Fomo(FomoActivity),
}

/// What an account did, plus the signals the event carried.
#[derive(Debug, Clone)]
pub struct Activity {
    /// Who acted.
    pub actor: Actor,
    /// `"<t>/<op> <key>"`.
    pub event_ref: String,
    /// Addresses found, strongest evidence first.
    pub signals: Vec<Signal>,
    /// What happened.
    pub kind: ActivityKind,
}
