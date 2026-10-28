use super::model::{Build, Call, Decision, Final, Judgment, Posted, Rerun};
use crate::suite::config::Platform;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
        /// How long the seed, relaunch and starts-on check took; the entry is written as they finish.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reset_ms: Option<u64>,
    },
    /// A tester settled one outcome before posting, so the page can show progress. Provisional: a later judgment of
    /// the same outcome replaces it, and the post files them all.
    Judged {
        token: String,
        judgment: Judgment,
    },
    Posted {
        token: String,
        posted: Posted,
        /// A pass on a spec with probes that says nothing about them: it waits for review like an Unsure.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        unprobed: bool,
        /// The outcomes' wording as the tester judged them, so an edited spec never rewords an old result.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        outcomes: Vec<String>,
        /// The ids its side finds get, in order, for `review_find`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        find_ids: Vec<u32>,
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
        /// The find's own screenshots on its topic; the rest are dropped.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        keep: Vec<String>,
    },
    /// A screenshot the coordinator took after review, added to a problem's evidence.
    EvidenceAdded {
        problem: u32,
        file: String,
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
        /// Why the Mac counted as busy, which holds the next dispatch however healthy the device is.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        busy: Vec<String>,
        facts: Vec<String>,
    },
    /// The server restarted a degraded device before the next spec.
    DeviceRestarted {
        platform: Platform,
        /// The spec whose attempt found the device degraded.
        spec: String,
        reason: String,
    },
    /// The server held a spec while the machine running the devices was busy; a restart doesn't fix that.
    HostBusy {
        platform: Platform,
        /// The spec that waited.
        spec: String,
        reason: String,
        waited_ms: u64,
        /// Whether it settled, or the wait ran out and the spec went ahead anyway.
        settled: bool,
    },
    /// The checks that passed before the run started (or resumed): suite, guard, builds, devices, app ready.
    Preflight {
        checks: Vec<String>,
    },
    /// The server stopped while the spec ran (resumed later): the attempt doesn't count, and the spec goes first.
    Interrupted {
        token: String,
    },
    /// A tester started readying a platform (suite.yaml `prepare`) before any spec runs there.
    PrepareDispatched {
        platform: Platform,
        attempt: u32,
        token: String,
        prompt: String,
    },
    /// The platform is ready: the tester finished and its `ready` labels showed.
    Prepared {
        platform: Platform,
        token: String,
    },
    /// An attempt at readying the platform didn't work; on the `last`, its waiting rows are filed BLOCKED.
    PrepareFailed {
        platform: Platform,
        token: String,
        reason: String,
        last: bool,
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
    /// The coordinator queued rows again, whatever state they were in: a running attempt is abandoned and a claim
    /// waiting for review set aside unjudged. Each gets a fresh attempt budget, and a finished run opens again.
    Requeued {
        rows: Vec<RowRef>,
        reason: String,
        /// For the next attempt's brief.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// A spec's file changed since the run began and was read again when it was queued: later attempts use the new
    /// text, and the report says so beside its results.
    SpecChanged {
        spec: String,
    },
    /// Platforms added on resume: their rows for the run's scope join it, and a finished run opens again.
    PlatformsAdded {
        platforms: Vec<Platform>,
    },
    /// Specs added to the scope, on resume or mid-run: their rows join it, and a finished run opens again.
    SpecsAdded {
        specs: Vec<String>,
    },
    Paused {
        reason: String,
    },
    Resumed,
    /// What the run is for changed mid-run (a check that grew into the release regression): its new label.
    Relabelled {
        label: String,
        reason: String,
    },
    Finished {
        result_counts: ResultCounts,
    },
    /// What an attempt's tester did, counted by the server from its calls and written as the attempt ended.
    Usage {
        token: String,
        calls: BTreeMap<String, u32>,
        /// From its first call to its last.
        duration_ms: u64,
    },
    /// A tester's tokens, from its transcript when it stops (the plugin's `SubagentStop` hook).
    Tokens {
        token: String,
        /// Its context on its last turn: the figure Claude Code's completion notice gives.
        tokens: u64,
        /// What its turns added up to; missing for attempts recorded before soundcheck kept it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        spent: Option<Spent>,
    },
}

/// A tester's usage summed over its turns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spent {
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
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

/// One row: a spec on a platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowRef {
    pub spec: String,
    pub platform: Platform,
}
