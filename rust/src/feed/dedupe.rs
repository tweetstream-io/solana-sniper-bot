//! First copy wins across connections. With an Ultra Speed key and a standard key the same event
//! arrives twice, with a different envelope `ts` and receipt times; the second copy is dropped
//! before detection. Next: `pipeline.rs` routes the event.
//!
//! An event is named by its `t/op` and a hash of its `d` with object keys sorted, so two copies
//! match however their fields are ordered. Each connection stamps its copy with its own
//! `receivedAt`, `observedAt`, and `deletedAt`, so keys with those names are left out at any depth.
//! Envelope ids cannot name an event: TweetStream sets the id of every `tweet/update` and
//! `tweet/meta` to the post id, and later patches of one post must all get through.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashSet, VecDeque};
use std::hash::{Hash, Hasher};

use serde_json::Value;
use serde_json::value::RawValue;

/// Keys whose values differ between two copies of one event.
const PER_COPY: [&str; 3] = ["receivedAt", "observedAt", "deletedAt"];

/// Returns the key two copies of one event share: `pair` (`t/op`) and the canonical hash of `d`.
pub fn dedupe_key(pair: &str, d: Option<&RawValue>) -> u64 {
    let mut hasher = DefaultHasher::new();
    pair.hash(&mut hasher);
    match d.map(|raw| serde_json::from_str::<Value>(raw.get())) {
        Some(Ok(value)) => hash_value(&value, &mut hasher),
        Some(Err(_)) => d.map(RawValue::get).hash(&mut hasher),
        None => 0u8.hash(&mut hasher),
    }
    hasher.finish()
}

fn hash_value(value: &Value, hasher: &mut DefaultHasher) {
    match value {
        Value::Null => 0u8.hash(hasher),
        Value::Bool(b) => (1u8, b).hash(hasher),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => (2u8, i).hash(hasher),
            (None, Some(u), _) => (8u8, u).hash(hasher),
            (None, None, f) => (7u8, f.map(f64::to_bits)).hash(hasher),
        },
        Value::String(s) => (3u8, s).hash(hasher),
        Value::Array(items) => {
            (4u8, items.len()).hash(hasher);
            for item in items {
                hash_value(item, hasher);
            }
        }
        // serde_json keeps object keys sorted, so the order on the wire never reaches the hash.
        Value::Object(map) => {
            5u8.hash(hasher);
            for (key, item) in map.iter().filter(|(k, _)| !PER_COPY.contains(&k.as_str())) {
                key.hash(hasher);
                hash_value(item, hasher);
            }
            6u8.hash(hasher);
        }
    }
}

/// Remembers the keys of recent events, bounded and evicted oldest first.
#[derive(Debug)]
pub struct EventDedupe {
    seen: HashSet<u64>,
    order: VecDeque<u64>,
    capacity: usize,
}

impl Default for EventDedupe {
    fn default() -> Self {
        EventDedupe { seen: HashSet::new(), order: VecDeque::new(), capacity: 50_000 }
    }
}

impl EventDedupe {
    /// Returns true for the first copy of a key, false for a repeat.
    pub fn first(&mut self, key: u64) -> bool {
        if !self.seen.insert(key) {
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
