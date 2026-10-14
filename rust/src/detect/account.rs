//! Detectors for account events: follow and unfollow, profile updates, pins, pump.fun callouts,
//! and FOMO theses and token verifications. Pure and synchronous. Next: `rules/engine.rs`.

use crate::protocol::{
    AccountCallout, AccountFollow, AccountProfileUpdate, AccountThesis, Field, FomoCoin, ProfileFields, TweetPin, normalize_handle,
};

use super::addresses::{FoundAddress, addresses_from_url, classify_address, find_addresses, find_all};
use super::post::{SignalContext, first_per_mint, x_actor};
use super::store::KnownProfile;
use super::types::{
    Activity, ActivityKind, Actor, CalloutActivity, CalloutKind, CalloutOp, Confidence, FollowActivity, FollowOp, FomoActivity, FomoOp,
    Namespace, ProfileActivity, ProfileField, Signal, SignalSource,
};

/// Returns the activity of `account/follow` or `account/unfollow`. The target's bio is used only
/// when the payload has it; this example does not call a profile lookup API.
pub fn follow_activity(d: &AccountFollow, op: FollowOp, platform: &str, event_ref: &str) -> Activity {
    let actor = x_actor(Some(&d.actor), platform);
    let target = Actor { namespace: Namespace::X, id: d.target.id.clone(), handle: normalize_handle(d.target.handle.as_deref()) };
    let mut found = find_all(d.target.bio.as_deref());
    if let Some(website) = &d.target.website_url {
        found.extend(addresses_from_url(website));
    }
    let ctx = SignalContext { actor: &actor, event_ref, post_key: None, market_cap_usd: None };
    let detail = format!("in bio of @{}", target.handle.as_deref().unwrap_or(""));
    let mut signals = Vec::new();
    for f in &found {
        signals.push(ctx.signal(SignalSource::FollowBio, f, Confidence::Medium, detail.clone()));
    }
    let kind = ActivityKind::Follow(FollowActivity { op, target, bio_in_payload: d.target.bio.is_some() });
    Activity { actor, event_ref: event_ref.to_string(), signals: first_per_mint(signals), kind }
}

fn change(fields: &ProfileFields, field: ProfileField) -> &Field {
    match field {
        ProfileField::Avatar => &fields.avatar,
        ProfileField::Banner => &fields.banner,
        ProfileField::Name => &fields.name,
        ProfileField::Bio => &fields.bio,
        ProfileField::WebsiteUrl => &fields.website_url,
    }
}

fn known(profile: &KnownProfile, field: ProfileField) -> &Field {
    match field {
        ProfileField::Avatar => &profile.avatar,
        ProfileField::Banner => &profile.banner,
        ProfileField::Name => &profile.name,
        ProfileField::Bio => &profile.bio,
        ProfileField::WebsiteUrl => &profile.website_url,
    }
}

const PROFILE_FIELDS: [ProfileField; 5] =
    [ProfileField::Avatar, ProfileField::Banner, ProfileField::Name, ProfileField::Bio, ProfileField::WebsiteUrl];
const CA_FIELDS: [ProfileField; 3] = [ProfileField::Bio, ProfileField::Name, ProfileField::WebsiteUrl];

/// Returns the activity of `account/profile_update`: the fields whose value changed and the
/// addresses that are new. Both compare against one baseline per field: the payload's `previous`
/// when it has the field, else the value this process saw earlier.
pub fn profile_activity(d: &AccountProfileUpdate, seen: Option<&KnownProfile>, platform: &str, event_ref: &str) -> Activity {
    let actor = x_actor(Some(&d.actor), platform);
    let baseline = |field: ProfileField| -> &Field {
        let previous = d.previous.as_ref().map(|p| change(p, field)).filter(|value| value.is_present());
        previous.or_else(|| seen.map(|s| known(s, field))).unwrap_or(&Field::Absent)
    };
    let mut changed = Vec::new();
    for field in PROFILE_FIELDS {
        let value = change(&d.changes, field);
        if value.is_present() && value != baseline(field) {
            changed.push(field);
        }
    }
    let ctx = SignalContext { actor: &actor, event_ref, post_key: None, market_cap_usd: None };
    let mut signals = Vec::new();
    for field in CA_FIELDS {
        let value = change(&d.changes, field);
        if !value.is_present() {
            continue;
        }
        let prior = baseline(field);
        let prior_known = prior.is_present();
        let old: Vec<FoundAddress> = field_addresses(field, prior.text());
        for found in field_addresses(field, value.text()) {
            if old.iter().any(|o| o.address == found.address) {
                continue;
            }
            let confidence = match (prior_known, field) {
                (false, _) => Confidence::Low,
                (true, ProfileField::Name) => Confidence::Medium,
                (true, ProfileField::Avatar | ProfileField::Banner | ProfileField::Bio | ProfileField::WebsiteUrl) => Confidence::High,
            };
            let note = if prior_known { "" } else { " (no earlier value seen)" };
            signals.push(ctx.signal(SignalSource::Profile, &found, confidence, format!("added to {}{note}", field.as_str())));
        }
    }
    let bio = changed.contains(&ProfileField::Bio).then(|| d.changes.bio.text().unwrap_or("").to_string());
    Activity {
        actor,
        event_ref: event_ref.to_string(),
        signals: first_per_mint(signals),
        kind: ActivityKind::Profile(ProfileActivity { changed, bio }),
    }
}

fn field_addresses(field: ProfileField, value: Option<&str>) -> Vec<FoundAddress> {
    let Some(value) = value else {
        return Vec::new();
    };
    match field {
        ProfileField::WebsiteUrl => {
            let mut found = addresses_from_url(value);
            found.extend(find_addresses(Some(value)));
            found
        }
        ProfileField::Avatar | ProfileField::Banner | ProfileField::Name | ProfileField::Bio => find_all(Some(value)),
    }
}

/// Returns the activity of `tweet/pin`: the addresses in the pinned post's links and text.
pub fn pin_activity(d: &TweetPin, post_key: &str, platform: &str, event_ref: &str) -> Activity {
    let actor = x_actor(d.author.as_ref(), platform);
    let mut found = Vec::new();
    let urls = d.tweet.as_ref().and_then(|t| t.urls.as_ref());
    for url in urls.into_iter().flatten() {
        found.extend(addresses_from_url(&url.url));
    }
    let text = d.tweet.as_ref().and_then(|t| t.text.as_deref()).or(d.text.as_deref());
    found.extend(find_all(text));
    let ctx = SignalContext { actor: &actor, event_ref, post_key: Some(post_key), market_cap_usd: None };
    let mut signals = Vec::new();
    for f in &found {
        signals.push(ctx.signal(SignalSource::Pin, f, Confidence::High, "in pinned post".to_string()));
    }
    Activity { actor, event_ref: event_ref.to_string(), signals: first_per_mint(signals), kind: ActivityKind::Pin }
}

/// The one address a callout or FOMO event names, if it is a valid Solana or EVM address.
fn token_signal(source: SignalSource, address: &str, ctx: &SignalContext, detail: String) -> Vec<Signal> {
    let Some((chain, address)) = classify_address(address) else {
        return Vec::new();
    };
    let found = FoundAddress { chain, address, via: "address".to_string(), confidence: Confidence::High };
    vec![ctx.signal(source, &found, Confidence::High, detail)]
}

/// Returns the activity of `account/callout` or `account/callout_update`. An update enriches an
/// earlier callout; rules never treat it as a new call. `position.amountHeld` is a decimal string;
/// `"0"` means the caller holds none.
pub fn callout_activity(d: &AccountCallout, op: CalloutOp, event_ref: &str) -> Activity {
    let actor = Actor { namespace: Namespace::PumpFun, id: None, handle: Some(d.caller.username.clone()) };
    let kind_name = d.kind.as_deref().unwrap_or("callout");
    let detail = format!("{kind_name} {}", d.token.symbol.as_deref().unwrap_or("")).trim().to_string();
    let ctx = SignalContext { actor: &actor, event_ref, post_key: None, market_cap_usd: d.market_cap_usd };
    let signals = token_signal(SignalSource::Callout, &d.token.address, &ctx, detail);
    let held = d.position.as_ref().and_then(|p| p.amount_held.as_deref());
    let callout = CalloutActivity {
        kind: CalloutKind::parse(d.kind.as_deref()),
        op,
        multiple: d.multiple,
        caller_holds: held.map(|h| decimal(h) > 0.0),
    };
    Activity { actor, event_ref: event_ref.to_string(), signals, kind: ActivityKind::Callout(callout) }
}

/// Parses a decimal string the way the protocol writes amounts; anything unparseable counts as 0.
fn decimal(text: &str) -> f64 {
    let text = text.trim();
    if text.is_empty() {
        return 0.0;
    }
    text.parse::<f64>().ok().filter(|v| v.is_finite()).unwrap_or(0.0)
}

/// Returns the activity of `account/thesis` (with the thesis) or `account/token_verified`
/// (without). Market values are decimal strings observed at one moment, not a live quote.
pub fn fomo_activity(coin: &FomoCoin, thesis: Option<&AccountThesis>, event_ref: &str) -> Activity {
    let (op, actor) = match thesis {
        Some(t) => {
            (FomoOp::Thesis, Actor { namespace: Namespace::Fomo, id: Some(t.author.id.clone()), handle: Some(t.author.username.clone()) })
        }
        None => (FomoOp::TokenVerified, Actor { namespace: Namespace::None, id: None, handle: None }),
    };
    let cap = coin.market_cap_usd.as_deref().and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite());
    let what = match op {
        FomoOp::Thesis => "thesis on",
        FomoOp::TokenVerified => "verified",
    };
    let detail = format!("{what} {}", coin.symbol.as_deref().unwrap_or("coin"));
    let ctx = SignalContext { actor: &actor, event_ref, post_key: None, market_cap_usd: cap };
    let signals = token_signal(SignalSource::Fomo, &coin.address, &ctx, detail);
    let fomo = FomoActivity {
        op,
        coin: coin.address.clone(),
        text: thesis.map(|t| t.thesis.text.clone()).unwrap_or_default(),
        author_exited: thesis.and_then(|t| t.position.as_ref()).is_some_and(|p| p.closed_at),
    };
    Activity { actor, event_ref: event_ref.to_string(), signals, kind: ActivityKind::Fomo(fomo) }
}
