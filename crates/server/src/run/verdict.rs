//! The release verdict, from the run's rows and problems. No special gate tests: every spec counts the same.

use super::model::{Final, ProblemSource, Status};
use super::{Row, State};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerdictStatus {
    /// An open FAIL.
    Blocked,
    /// Something unsettled: an untriaged problem, a spec not run, a result still under review.
    NotReady,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verdict {
    pub status: VerdictStatus,
    /// Each reason the verdict isn't Ready, as one plain line.
    pub reasons: Vec<String>,
}

/// A problem stops being in the way once it's Dismissed, Fixed or Deferred (a duplicate follows its original).
fn settled(status: &Status) -> bool {
    matches!(
        status,
        Status::Dismissed { .. } | Status::Fixed { .. } | Status::Deferred { .. }
    )
}

pub fn verdict(state: &State) -> Verdict {
    let mut open_fails = 0;
    let mut unsettled_blocked = 0;
    let mut not_run = 0;
    let mut in_progress = 0;
    for ((spec, platform), record) in &state.rows {
        match &record.row {
            Row::Filed { result, .. } if *result != Final::Pass => {
                let problems: Vec<_> = state
                    .problems
                    .iter()
                    .filter(|p| {
                        &p.spec == spec && p.platform == *platform && p.source == ProblemSource::Row
                    })
                    .collect();
                // A BLOCKED with no problem is a harness gap: it counts as not run until a rerun passes.
                let ok =
                    !problems.is_empty() && problems.iter().all(|p| settled(state.status_of(p)));
                match (result, ok) {
                    (Final::Fail, false) => open_fails += 1,
                    (Final::Blocked, false) => unsettled_blocked += 1,
                    _ => {}
                }
            }
            Row::Filed { .. } => {}
            Row::NotRun => not_run += 1,
            Row::Queued { .. } | Row::Running { .. } | Row::NeedsReview { .. } => in_progress += 1,
        }
    }
    let open_problems = state
        .problems
        .iter()
        .filter(|p| p.status == Status::Open)
        .count();
    let pending_finds = state.pending_finds.len();

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
    say(open_fails, "1 open FAIL", "open FAILs");
    say(
        open_problems,
        "1 problem not triaged",
        "problems not triaged",
    );
    say(unsettled_blocked, "1 spec BLOCKED", "specs BLOCKED");
    say(
        in_progress,
        "1 spec still running or under review",
        "specs still running or under review",
    );
    say(
        pending_finds,
        "1 side find under review",
        "side finds under review",
    );
    say(not_run, "1 spec not run", "specs not run");

    let status = if open_fails > 0 {
        VerdictStatus::Blocked
    } else if reasons.is_empty() {
        VerdictStatus::Ready
    } else {
        VerdictStatus::NotReady
    };
    Verdict { status, reasons }
}
