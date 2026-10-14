//! Rule lookup by account, built once at startup from each mode's listen keys (`rules/modes.rs`),
//! so matching an event costs a few map lookups however many rules there are. Next:
//! `rules/engine.rs` runs the matchers for the rules found here.

use std::collections::{HashMap, HashSet};

use crate::detect::types::{Activity, ActivityKind};
use crate::setup::config::Rule;

use super::modes::{ActivityType, activity_type, actor_keys, listens};

/// The enabled rules by the activity type and account key they listen to.
#[derive(Debug)]
pub struct RuleIndex {
    by_key: HashMap<(ActivityType, String), Vec<usize>>,
}

impl RuleIndex {
    /// Builds the index over the enabled rules.
    pub fn new(rules: &[Rule]) -> RuleIndex {
        let mut by_key: HashMap<(ActivityType, String), Vec<usize>> = HashMap::new();
        for (i, rule) in rules.iter().enumerate() {
            if !rule.enabled {
                continue;
            }
            for key in listens(rule) {
                let list = by_key.entry(key).or_default();
                if !list.contains(&i) {
                    list.push(i);
                }
            }
        }
        RuleIndex { by_key }
    }

    /// Returns the indexes of the rules that watch this activity's actor, in config order, so
    /// decisions come out in the same order in every implementation.
    pub fn candidates(&self, activity: &Activity) -> Vec<usize> {
        let t = activity_type(&activity.kind);
        let mut keys = actor_keys(&activity.actor);
        if let ActivityKind::Fomo(fomo) = &activity.kind {
            keys.push(format!("fc:{}", fomo.coin));
        }
        let mut found = HashSet::new();
        for key in keys {
            for &rule in self.by_key.get(&(t, key)).into_iter().flatten() {
                found.insert(rule);
            }
        }
        let mut found: Vec<usize> = found.into_iter().collect();
        found.sort_unstable();
        found
    }
}
