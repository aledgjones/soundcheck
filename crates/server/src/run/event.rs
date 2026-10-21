use super::model::{Build, Call, Decision, Final, Posted, Rerun};
use crate::suite::config::Platform;
use serde::{Deserialize, Serialize};

/// Everything that happens in a run, in the order it happened. State is rebuilt by replaying these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    Dispatched {
        spec: String,
        platform: Platform,
        attempt: u32,
        token: String,
        /// The claim a verification attempt is asked to reproduce.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verify: Option<String>,
        prompt: String,
    },
    Posted {
        token: String,
        posted: Posted,
    },
    Reviewed {
        spec: String,
        platform: Platform,
        attempt: u32,
        #[serde(flatten)]
        call: Call,
        reason: String,
    },
    /// A side find reviewed: a finding with this title, or not a problem.
    FindReviewed {
        id: u32,
        title: Option<String>,
        reason: String,
    },
    /// What the server saved itself when a non-pass was posted, before the next seed could wipe it.
    EvidenceCaptured {
        spec: String,
        platform: Platform,
        attempt: u32,
        files: Vec<String>,
    },
    /// The device's health, sampled after an attempt posted: part of that attempt's evidence.
    DeviceHealth {
        spec: String,
        platform: Platform,
        attempt: u32,
        /// Why the device counted as degraded; empty when healthy.
        degraded: Vec<String>,
        facts: Vec<String>,
    },
    /// The server restarted a degraded device before the next spec.
    DeviceRestarted {
        platform: Platform,
        /// The spec whose attempt found the device degraded.
        spec: String,
        reason: String,
    },
    /// The tester went quiet; the attempt counts and the spec goes back to the queue.
    Stalled {
        token: String,
        reason: String,
    },
    /// The seed or the starts-on check kept failing: BLOCKED without using an attempt.
    ResetFailed {
        spec: String,
        platform: Platform,
        reason: String,
    },
    /// Triage: allowed after the run has finished, since it usually happens then.
    Triaged {
        problem: u32,
        #[serde(flatten)]
        decision: Decision,
        reason: String,
    },
    /// A new build on a platform, with the reruns it triggered, as computed when it was installed.
    BuildInstalled {
        platform: Platform,
        build: Build,
        reruns: Vec<Rerun>,
    },
    Paused {
        reason: String,
    },
    Resumed,
    Finished {
        result_counts: ResultCounts,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultCounts {
    pub pass: u32,
    pub fail: u32,
    pub blocked: u32,
    pub not_run: u32,
}

impl ResultCounts {
    pub fn add(&mut self, result: Option<Final>) {
        match result {
            Some(Final::Pass) => self.pass += 1,
            Some(Final::Fail) => self.fail += 1,
            Some(Final::Blocked) => self.blocked += 1,
            None => self.not_run += 1,
        }
    }
}
