//! Matches one activity against the rules that listen to its account, then applies the limits that
//! need memory: fire once per post or event, cooldown, and max snipes. Synchronous: nothing else
//! runs between "this rule fires" and "the slot is taken". Next: `filters/gate.rs`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::detect::types::Activity;
use crate::execute::trades::TradeBook;
use crate::setup::config::Config;

use super::index::RuleIndex;
use super::modes::{Fire, RuleExit, Skip, SkipCode, Verdict, match_rule};

/// What the engine needs to know about the moment of the event.
#[derive(Debug, Clone, Copy)]
pub struct EvalContext<'a> {
    /// Wall clock at receipt, in Unix ms.
    pub now_ms: i64,
    /// Publish-to-receipt age of the post, when known.
    pub age_ms: Option<i64>,
    /// Last bio seen for the acting account.
    pub known_bio: Option<&'a str>,
}

/// The engine's answer for one activity.
#[derive(Debug, Default)]
pub struct Evaluation {
    /// Rules that fire, in config order.
    pub fires: Vec<Fire>,
    /// Rules that looked and declined, in config order.
    pub skips: Vec<Skip>,
    /// Rules that want to sell a position they opened.
    pub exits: Vec<RuleExit>,
}

const MAX_FIRED_KEYS: usize = 100_000;

/// The rule index plus fire-once keys and per-rule cooldown clocks.
pub struct RuleEngine {
    config: Arc<Config>,
    index: RuleIndex,
    book: Arc<TradeBook>,
    fired: HashSet<String>,
    fired_order: VecDeque<String>,
    last_fire: HashMap<usize, i64>,
}

impl RuleEngine {
    /// Builds the engine and its rule index.
    pub fn new(config: Arc<Config>, book: Arc<TradeBook>) -> RuleEngine {
        let index = RuleIndex::new(&config.rules);
        RuleEngine { config, index, book, fired: HashSet::new(), fired_order: VecDeque::new(), last_fire: HashMap::new() }
    }

    /// Returns what every rule watching the activity's actor makes of it.
    pub fn evaluate(&mut self, activity: &Activity, ctx: EvalContext) -> Evaluation {
        let mut out = Evaluation::default();
        let stale = self.stale_detail(ctx.age_ms);
        for index in self.index.candidates(activity) {
            let mut decisions = Vec::new();
            for verdict in match_rule(index, &self.config.rules[index], activity, ctx.known_bio) {
                match verdict {
                    Verdict::Exit(exit) => out.exits.push(exit),
                    Verdict::Fire(_) | Verdict::Skip(_) => decisions.push(verdict),
                }
            }
            if let Some(detail) = stale.as_ref().filter(|_| !decisions.is_empty()) {
                out.skips.push(Skip { rule: index, code: SkipCode::StalePost, detail: detail.clone() });
                continue;
            }
            for verdict in decisions {
                match verdict {
                    Verdict::Skip(skip) => out.skips.push(skip),
                    Verdict::Fire(hit) => self.admit(hit.fire(activity.actor.clone()), ctx.now_ms, &mut out),
                    Verdict::Exit(_) => {}
                }
            }
        }
        out
    }

    /// A post older than `maxSignalAgeMs` is reported once to each rule it concerns, in place of
    /// that rule's decisions.
    fn stale_detail(&self, age_ms: Option<i64>) -> Option<String> {
        let max = self.config.max_signal_age_ms;
        let age = age_ms.filter(|age| *age > max)?;
        Some(format!("post is {age} ms old, maxSignalAgeMs is {max}"))
    }

    /// Undoes a fire that sent nothing (the filters rejected it, a sell found no position): the
    /// rule may fire again on the same key and gets its snipe back. Call at most once per fire.
    pub fn release(&mut self, fire: &Fire) {
        self.fired.remove(&fire.fire_key);
        self.book.release_snipe(&self.config.rules[fire.rule].name);
    }

    fn admit(&mut self, fire: Fire, now_ms: i64, out: &mut Evaluation) {
        if self.fired.contains(&fire.fire_key) {
            return;
        }
        let rule = &self.config.rules[fire.rule];
        let cooldown_ms = rule.filters.cooldown_sec * 1000.0;
        if let Some(&last) = self.last_fire.get(&fire.rule)
            && ((now_ms - last) as f64) < cooldown_ms
        {
            let left = ((cooldown_ms - (now_ms - last) as f64) / 1000.0).ceil();
            out.skips.push(Skip { rule: fire.rule, code: SkipCode::Cooldown, detail: format!("{left} s left") });
            return;
        }
        if !self.book.reserve_snipe(&rule.name, rule.filters.max_snipes) {
            let detail = format!("maxSnipes {} reached", rule.filters.max_snipes);
            out.skips.push(Skip { rule: fire.rule, code: SkipCode::MaxSnipes, detail });
            return;
        }
        self.fired.insert(fire.fire_key.clone());
        self.fired_order.push_back(fire.fire_key.clone());
        if self.fired.len() > MAX_FIRED_KEYS
            && let Some(oldest) = self.fired_order.pop_front()
        {
            self.fired.remove(&oldest);
        }
        self.last_fire.insert(fire.rule, now_ms);
        out.fires.push(fire);
    }
}
