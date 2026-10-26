use super::event::Event;
use super::journal::Entry;
use std::collections::BTreeMap;

/// What one attempt cost, from its `Usage` and `Tokens` entries; either may be missing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AttemptUsage {
    pub token: String,
    pub tokens: Option<u64>,
    pub calls: BTreeMap<String, u32>,
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

/// Every attempt that has any usage recorded, in the order they were dispatched.
pub fn attempts(entries: &[Entry]) -> Vec<AttemptUsage> {
    let mut order: Vec<String> = Vec::new();
    let mut by_token: BTreeMap<String, AttemptUsage> = BTreeMap::new();
    for entry in entries {
        match &entry.event {
            Event::Dispatched { token, .. } => order.push(token.clone()),
            Event::Usage {
                token,
                calls,
                duration_ms,
            } => {
                let a = by_token.entry(token.clone()).or_default();
                a.calls = calls.clone();
                a.duration_ms = Some(*duration_ms);
            }
            Event::Tokens { token, tokens } => {
                by_token.entry(token.clone()).or_default().tokens = Some(*tokens);
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
