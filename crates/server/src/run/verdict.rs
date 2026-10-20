//! The release verdict, from the run's rows and problems. No special gate tests: every spec counts the same.

use super::model::{Final, ProblemKind, ProblemSource, Status};
use super::{Row, State};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerdictStatus {
    /// An open blocker.
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
    for ((spec, platform), record) in &state.rows {
        match &record.row {
            // A BLOCKED with no problem is a harness gap: it counts as not run until a rerun passes.
            Row::Filed {
                result: Final::Blocked,
                ..
            } if !state.problems.iter().any(|p| {
                &p.spec == spec && p.platform == *platform && p.source == ProblemSource::Row
            }) =>
            {
                harness_blocked += 1
            }
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
    let blockers = open(ProblemKind::Blocker);
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
    say(blockers, "1 blocker", "blockers");
    say(findings, "1 finding to decide", "findings to decide");
    say(
        harness_blocked,
        "1 spec blocked by the harness",
        "specs blocked by the harness",
    );
    say(checking, "1 being checked", "being checked");
    say(not_run, "1 not run yet", "not run yet");

    let status = if blockers > 0 {
        VerdictStatus::Blocked
    } else if reasons.is_empty() {
        VerdictStatus::Ready
    } else {
        VerdictStatus::NotReady
    };
    Verdict { status, reasons }
}
