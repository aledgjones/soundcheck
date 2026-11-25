use super::event::{Event, Spent};
use super::journal::Entry;
use std::collections::BTreeMap;

/// What one attempt cost, from its `Usage` and `Tokens` entries; either may be missing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AttemptUsage {
    pub token: String,
    pub tokens: Option<u64>,
    pub spent: Option<Spent>,
    pub calls: BTreeMap<String, u32>,
    pub call_ms: BTreeMap<String, u64>,
    pub duration_ms: Option<u64>,
}

impl AttemptUsage {
    pub fn total_calls(&self) -> u32 {
        self.calls.values().sum()
    }

    /// The token without its attempt number, so attempts of one spec in two runs pair up.
    pub fn row(&self) -> String {
        match (self.token.split_once('#'), self.token.rsplit_once('@')) {
            (Some((spec, _)), Some((_, platform))) => format!("{spec}@{platform}"),
            _ => self.token.clone(),
        }
    }
}

/// What one reviewer cost, from its `Reviewer` entry (SA-1059): reviews are part of what a run costs, so they're
/// listed and compared beside the attempts.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewUsage {
    pub spec: String,
    pub platform: String,
    pub tokens: Option<u64>,
    pub spent: Option<Spent>,
    pub duration_ms: Option<u64>,
    /// It was the row's last reviewer and left it for the coordinator.
    pub unsettled: bool,
}

/// Every reviewer the server launched, in the order they ended.
pub fn reviews(entries: &[Entry]) -> Vec<ReviewUsage> {
    entries
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Reviewer {
                spec,
                platform,
                tokens,
                spent,
                duration_ms,
                unsettled,
            } => Some(ReviewUsage {
                spec: spec.clone(),
                platform: platform.to_string(),
                tokens: *tokens,
                spent: *spent,
                duration_ms: *duration_ms,
                unsettled: unsettled.is_some(),
            }),
            _ => None,
        })
        .collect()
}

/// Everything a `Spent` adds up to: what was read fresh, written to the cache, read from it and written out.
pub fn all(spent: &Spent) -> u64 {
    spent.input + spent.cache_write + spent.cache_read + spent.output
}

/// Every attempt that has any usage recorded, in the order they were dispatched.
pub fn attempts(entries: &[Entry]) -> Vec<AttemptUsage> {
    let mut order: Vec<String> = Vec::new();
    let mut by_token: BTreeMap<String, AttemptUsage> = BTreeMap::new();
    for entry in entries {
        match &entry.event {
            Event::Dispatched { token, .. } => {
                order.push(token.clone());
            }
            Event::Usage {
                token,
                calls,
                call_ms,
                duration_ms,
            } => {
                let a = by_token.entry(token.clone()).or_default();
                a.calls = calls.clone();
                a.call_ms = call_ms.clone();
                a.duration_ms = Some(*duration_ms);
            }
            Event::Tokens {
                token,
                tokens,
                spent,
            } => {
                let a = by_token.entry(token.clone()).or_default();
                a.tokens = Some(*tokens);
                a.spent = *spent;
            }
            _ => {}
        }
    }
    order
        .into_iter()
        .filter_map(|token| {
            by_token.remove(&token).map(|mut a| {
                a.token = token;
                a
            })
        })
        .collect()
}
