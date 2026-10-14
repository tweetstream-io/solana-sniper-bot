//! First copy wins across connections. With an Ultra Speed key and a standard key the same event
//! can arrive twice; the second copy is dropped before detection. Next: `pipeline.rs` routes the
//! event.

use std::collections::{HashSet, VecDeque};

/// The identities an event can carry, strongest first.
#[derive(Debug, Clone, Copy, Default)]
pub struct EventIds<'a> {
    /// The envelope `id`.
    pub envelope: Option<&'a str>,
    /// The payload's `eventId`.
    pub event: Option<&'a str>,
    /// The post key of a `tweet/*` event.
    pub post: Option<&'a str>,
    /// An id the payload type carries, such as a thesis id or a callout id.
    pub payload: Option<&'a str>,
}

/// Returns the key two copies of one event share, or `None` when the event has no stable identity
/// and must pass through.
///
/// `tweet/update` and `tweet/meta` can repeat for a post, so they are keyed by envelope id only.
/// Other events use the payload's `eventId`, then the post key, then the envelope id, then an id
/// the payload type carries.
pub fn dedupe_key(pair: &str, ids: EventIds) -> Option<String> {
    let EventIds { envelope, event, post, payload } = ids;
    if pair == "tweet/update" || pair == "tweet/meta" {
        return envelope.map(|id| format!("{pair} id:{id}"));
    }
    if let Some(id) = event.or(post) {
        return Some(format!("{pair} {id}"));
    }
    envelope.or(payload).map(|id| format!("{pair} id:{id}"))
}

/// Remembers the keys of recent events, bounded and evicted oldest first.
#[derive(Debug)]
pub struct EventDedupe {
    seen: HashSet<String>,
    order: VecDeque<String>,
    capacity: usize,
}

impl Default for EventDedupe {
    fn default() -> Self {
        EventDedupe { seen: HashSet::new(), order: VecDeque::new(), capacity: 50_000 }
    }
}

impl EventDedupe {
    /// Returns true for the first copy of a key, false for a repeat. `None` always passes.
    pub fn first(&mut self, key: Option<String>) -> bool {
        let Some(key) = key else {
            return true;
        };
        if !self.seen.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        if self.seen.len() > self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            self.seen.remove(&oldest);
        }
        true
    }
}
