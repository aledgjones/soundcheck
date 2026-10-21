//! The release verdict, from the run's rows and problems. No special gate tests: every spec counts the same.

use super::model::{Final, ProblemKind, Status};
use super::{Row, State};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerdictStatus {
    /// An open failure.
    Blocked,
    /// Something unsettled: a finding to decide, a spec not run, a result still being checked.
    NotReady,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verdict {
    pub status: VerdictStatus,
    /// Each reason the verdict isn't Ready, as one plain line.
    pub reasons: Vec<String>,
}

pub fn verdict(state: &State) -> Verdict {
    let mut harness_blocked = 0;
    let mut checking = state.pending_finds.len();
    let mut not_run = 0;
    for record in state.rows.values() {
        match &record.row {
            // BLOCKED means the environment or harness stopped the test: it counts as untested until a rerun passes.
            Row::Filed {
                result: Final::Blocked,
                ..
            } => harness_blocked += 1,
            Row::Filed { .. } => {}
            Row::NeedsReview { .. } => checking += 1,
            Row::NotRun | Row::Queued { .. } | Row::Running { .. } => not_run += 1,
        }
    }
    // Duplicates carry the status "duplicate", so each problem counts once, through its original.
    let open = |kind: ProblemKind| {
        state
            .problems
            .iter()
            .filter(|p| p.kind == kind && p.status == Status::Open)
            .count()
    };
    let failures = open(ProblemKind::Failure);
    let findings = open(ProblemKind::Finding);

    let mut reasons = Vec::new();
    let mut say = |n: usize, one: &str, many: &str| {
        if n > 0 {
            reasons.push(if n == 1 {
                one.to_string()
            } else {
                format!("{n} {many}")
            });
        }
    };
    say(failures, "1 failure", "failures");
    say(findings, "1 finding to decide", "findings to decide");
    say(harness_blocked, "1 blocked", "blocked");
    say(checking, "1 being checked", "being checked");
    say(not_run, "1 not run yet", "not run yet");

    let status = if failures > 0 {
        VerdictStatus::Blocked
    } else if reasons.is_empty() {
        VerdictStatus::Ready
    } else {
        VerdictStatus::NotReady
    };
    Verdict { status, reasons }
}
