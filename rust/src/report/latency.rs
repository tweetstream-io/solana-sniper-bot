//! Per-signal stage timings and the summary printed on exit. Stages, in order:
//!
//! - `publish->receipt`: post publish time (from the X id) to WebSocket receipt; wall clocks, so it
//!   includes clock skew between X and this machine
//! - `receipt->decision`: parse, dedupe, detect, rule match, reservation; pure CPU
//! - `safety read`: the parallel checks
//! - `build+sign`: the part of `decision->signed` after the checks returned
//! - `decision->signed`: checks, transaction build, signing
//! - `signed->first ack`: first sender that accepted the transaction
//! - `send->confirmed`: first send to confirmation over the RPC WebSocket
//!
//! Every stage except the first uses [`Instant`], which never jumps.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::log::log;

/// The times recorded for one fired rule.
#[derive(Debug, Clone)]
pub struct Trace {
    /// The token.
    pub mint: String,
    /// Rule name.
    pub rule: String,
    /// Connection that delivered the event first.
    pub connection: String,
    /// The event it came from.
    pub event_ref: String,
    /// Post publish time, wall clock ms.
    pub published_at_wall: Option<i64>,
    /// Receipt time, wall clock ms.
    pub received_wall: i64,
    /// Receipt, monotonic.
    pub received: Instant,
    /// Decision made.
    pub decided: Option<Instant>,
    /// How long the checks took.
    pub gate: Option<Duration>,
    /// Checks returned.
    pub gate_done: Option<Instant>,
    /// Transaction signed.
    pub signed: Option<Instant>,
    /// First send.
    pub sent: Option<Instant>,
    /// First sender accepted.
    pub first_ack: Option<Instant>,
    /// Which sender accepted first.
    pub first_ack_by: Option<String>,
    /// Confirmed or failed on chain.
    pub confirmed: Option<Instant>,
}

impl Trace {
    /// Starts a trace at receipt.
    pub fn new(
        mint: &str,
        rule: &str,
        connection: &str,
        event_ref: &str,
        published_at_wall: Option<i64>,
        received_wall: i64,
        received: Instant,
    ) -> Trace {
        Trace {
            mint: mint.to_string(),
            rule: rule.to_string(),
            connection: connection.to_string(),
            event_ref: event_ref.to_string(),
            published_at_wall,
            received_wall,
            received,
            decided: None,
            gate: None,
            gate_done: None,
            signed: None,
            sent: None,
            first_ack: None,
            first_ack_by: None,
            confirmed: None,
        }
    }
}

const STAGES: [&str; 7] =
    ["publish->receipt", "receipt->decision", "safety read", "build+sign", "decision->signed", "signed->first ack", "send->confirmed"];

fn ms(from: Instant, to: Instant) -> f64 {
    let ms = to.saturating_duration_since(from).as_secs_f64() * 1000.0;
    (ms * 1000.0).round() / 1000.0
}

/// Returns the `p`-quantile of a sorted list (nearest rank).
pub fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.max(1) - 1])
}

#[derive(Debug, Default)]
struct Totals {
    stages: BTreeMap<usize, Vec<f64>>,
    feed: BTreeMap<String, Vec<f64>>,
    first_copies: BTreeMap<String, u64>,
    first_acks: BTreeMap<String, u64>,
}

/// Collects stage timings and prints one line per trace and a summary.
#[derive(Debug, Default)]
pub struct LatencyReport {
    totals: Mutex<Totals>,
}

impl LatencyReport {
    fn totals(&self) -> std::sync::MutexGuard<'_, Totals> {
        self.totals.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Records the publish-to-receipt time of a new post on a connection.
    pub fn record_post(&self, connection: &str, publish_to_receipt_ms: f64) {
        let mut totals = self.totals();
        let list = totals.feed.entry(connection.to_string()).or_default();
        list.push(publish_to_receipt_ms);
        if list.len() > 10_000 {
            list.remove(0);
        }
    }

    /// Counts an event that this connection delivered first.
    pub fn record_first_copy(&self, connection: &str) {
        *self.totals().first_copies.entry(connection.to_string()).or_default() += 1;
    }

    /// Records the stages of a finished trace and logs them with the outcome.
    pub fn finish(&self, t: &Trace, outcome: &str) {
        let mut values: [Option<f64>; 7] = [None; 7];
        values[0] = t.published_at_wall.map(|p| (t.received_wall - p) as f64);
        values[1] = t.decided.map(|d| ms(t.received, d));
        values[2] = t.gate.map(|g| (g.as_secs_f64() * 1e6).round() / 1000.0);
        values[3] = t.gate_done.zip(t.signed).map(|(g, s)| ms(g, s));
        values[4] = t.decided.zip(t.signed).map(|(d, s)| ms(d, s));
        values[5] = t.signed.zip(t.first_ack).map(|(s, a)| ms(s, a));
        values[6] = t.sent.zip(t.confirmed).map(|(s, c)| ms(s, c));
        let mut parts = Vec::new();
        {
            let mut totals = self.totals();
            for (i, value) in values.iter().enumerate() {
                let Some(value) = value else { continue };
                totals.stages.entry(i).or_default().push(*value);
                parts.push(format!("{} {value} ms", STAGES[i]));
            }
            if let Some(by) = &t.first_ack_by {
                *totals.first_acks.entry(by.clone()).or_default() += 1;
            }
        }
        let ack = t.first_ack_by.as_ref().map(|by| format!(", first ack {by}")).unwrap_or_default();
        let mint: String = t.mint.chars().take(6).collect();
        log("latency", &format!("{} {mint} via {}: {}{ack} => {outcome}", t.rule, t.connection, parts.join(", ")));
    }

    /// Returns the summary lines: p50 and p95 per stage and per connection.
    pub fn summary(&self) -> Vec<String> {
        let fmt = |list: &[f64]| {
            let mut sorted = list.to_vec();
            sorted.sort_by(f64::total_cmp);
            let p = |q| percentile(&sorted, q).map_or("-".to_string(), |v| v.to_string());
            format!("p50 {} ms, p95 {} ms, n={}", p(0.5), p(0.95), sorted.len())
        };
        let totals = self.totals();
        let mut lines = Vec::new();
        for (connection, list) in &totals.feed {
            lines.push(format!("feed {connection} publish->receipt: {}", fmt(list)));
        }
        for (connection, n) in &totals.first_copies {
            lines.push(format!("first copy of an event came from {connection}: {n} times"));
        }
        for (i, list) in &totals.stages {
            if !list.is_empty() {
                lines.push(format!("{}: {}", STAGES[*i], fmt(list)));
            }
        }
        for (sender, n) in &totals.first_acks {
            lines.push(format!("first sender to accept: {sender} ({n})"));
        }
        if lines.is_empty() {
            lines.push("no events yet".to_string());
        }
        lines
    }
}
