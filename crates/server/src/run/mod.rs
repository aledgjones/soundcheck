//! A run's results engine: row states, review, attempts, stalls and the finish gate, journalled and replayable.

pub mod event;
pub mod journal;
pub mod model;
pub mod verdict;

use crate::suite::config::Platform;
use anyhow::{Result, bail};
use event::{Event, ResultCounts};
use journal::{Entry, Journal};
use model::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

/// The original attempt plus send-backs and verifications.
pub const MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone)]
pub struct PlannedSpec {
    pub id: String,
    pub platforms: Vec<Platform>,
    pub outcomes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub specs: Vec<PlannedSpec>,
    /// Off by default: platforms take turns, one spec running at a time.
    pub parallel: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    NotRun,
    /// Another attempt is waiting: a send-back, a verification, or a retry after a stall.
    Queued {
        verify: Option<String>,
    },
    Running {
        attempt: u32,
        token: String,
        verify: Option<String>,
    },
    NeedsReview {
        attempt: u32,
        claim: Claim,
        posted: Posted,
        verify: Option<String>,
        since: u64,
    },
    Filed {
        result: Final,
        attempt: u32,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct RowRecord {
    pub row: Row,
    /// The last attempt's number. It never goes back, so a later pass can prove a fix.
    pub attempts: u32,
    /// Attempts used on the current build; the limit applies per build.
    pub budget: u32,
    /// Attempts on the current build that claimed a non-pass, for "not reproduced (1 of 2)".
    pub sightings: u32,
    /// The build the latest attempt ran on. Internal tracking: the report's notes show it, rows don't.
    pub build: Option<Build>,
}

/// A side find waiting for the coordinator.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingFind {
    pub id: u32,
    pub spec: String,
    pub platform: Platform,
    pub attempt: u32,
    pub find: SideFind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub spec: String,
    pub platform: Platform,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct State {
    pub rows: BTreeMap<(String, Platform), RowRecord>,
    pub problems: Vec<Problem>,
    pub pending_finds: Vec<PendingFind>,
    /// Harness gaps and reset failures: not app problems.
    pub notes: Vec<Note>,
    /// Specs flagged for rewording after an outcome stayed unclear.
    pub unclear_specs: BTreeSet<String>,
    /// The build installed on each platform now.
    pub builds: BTreeMap<Platform, Build>,
    pub paused: bool,
    pub finished: bool,
    next_id: u32,
}

#[derive(Debug, Clone, Copy)]
struct Activity {
    last: u64,
    in_call: u32,
}

pub struct Run {
    plan: Plan,
    journal: Journal,
    state: State,
    /// Tester liveness, kept in memory: a resumed run starts every running spec's clock again.
    activity: HashMap<String, Activity>,
    /// Every entry so far, for the event stream's replay.
    entries: Vec<Entry>,
}

impl Run {
    /// Opens the run's journal, replaying whatever is already in it.
    pub fn open(plan: Plan, journal_path: &Path, now: u64) -> Result<Run> {
        let (journal, entries) = Journal::open(journal_path)?;
        let mut run = Run {
            state: State::new(&plan),
            plan,
            journal,
            activity: HashMap::new(),
            entries: Vec::new(),
        };
        for entry in &entries {
            run.state.apply(&run.plan, entry);
        }
        run.entries = entries;
        for record in run.state.rows.values() {
            if let Row::Running { token, .. } = &record.row {
                run.activity.insert(
                    token.clone(),
                    Activity {
                        last: now,
                        in_call: 0,
                    },
                );
            }
        }
        Ok(run)
    }

    pub fn last_seq(&self) -> u64 {
        self.entries.last().map_or(0, |e| e.seq)
    }

    /// Entries after `seq`, oldest first.
    pub fn entries_after(&self, seq: u64) -> &[Entry] {
        let start = self.entries.partition_point(|e| e.seq <= seq);
        &self.entries[start..]
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn verdict(&self) -> verdict::Verdict {
        verdict::verdict(&self.state)
    }

    pub fn row(&self, spec: &str, platform: Platform) -> Option<&RowRecord> {
        self.state.rows.get(&(spec.to_string(), platform))
    }

    /// Starts the next attempt of a spec on a platform; returns its token. `brief` renders the tester's prompt from
    /// the token and any claim to verify, and is journalled as sent.
    pub fn dispatch(
        &mut self,
        now: u64,
        spec: &str,
        platform: Platform,
        brief: impl FnOnce(&str, Option<&str>) -> String,
    ) -> Result<String> {
        self.open_for_commands()?;
        if self.state.paused {
            bail!("the run is paused; resume it first");
        }
        let record = self.record(spec, platform)?;
        let verify = match &record.row {
            Row::NotRun => None,
            Row::Queued { verify } => verify.clone(),
            other => bail!(
                "{spec} on {platform} can't be dispatched: it's {}",
                describe(other)
            ),
        };
        if record.budget >= MAX_ATTEMPTS {
            bail!("{spec} on {platform} has used all {MAX_ATTEMPTS} attempts on this build");
        }
        let attempt = record.attempts + 1;
        if let Some(((busy, on), _)) = self.state.rows.iter().find(|((_, p), r)| {
            matches!(r.row, Row::Running { .. }) && (*p == platform || !self.plan.parallel)
        }) {
            bail!("{busy} is still running on {on}; one spec at a time");
        }
        let token = format!("{spec}#{attempt}@{platform}");
        self.commit(
            now,
            Event::Dispatched {
                spec: spec.into(),
                platform,
                attempt,
                token: token.clone(),
                prompt: brief(&token, verify.as_deref()),
                verify,
            },
        )?;
        self.activity.insert(
            token.clone(),
            Activity {
                last: now,
                in_call: 0,
            },
        );
        Ok(token)
    }

    /// A tester's result. A PASS files at once; anything else waits for review.
    pub fn post(&mut self, now: u64, token: &str, posted: Posted) -> Result<Claim> {
        self.open_for_commands()?;
        let (spec, _) = self.running(token)?;
        let planned = self.planned(&spec)?;
        let mut seen = BTreeSet::new();
        for j in &posted.judgments {
            if j.outcome >= planned.outcomes.len() {
                bail!(
                    "outcome {} doesn't exist: the spec has {}",
                    j.outcome,
                    planned.outcomes.len()
                );
            }
            if !seen.insert(j.outcome) {
                bail!("outcome {} is judged twice", j.outcome);
            }
            if j.observed.trim().is_empty() {
                bail!("outcome {} needs what you observed", j.outcome);
            }
        }
        if let Some(missing) = (0..planned.outcomes.len()).find(|i| !seen.contains(i)) {
            bail!(
                "outcome {missing} ({:?}) isn't judged; judge every outcome",
                planned.outcomes[missing]
            );
        }
        if let Some(find) = posted
            .side_finds
            .iter()
            .find(|f| f.summary.trim().is_empty() || f.observed.trim().is_empty())
        {
            bail!(
                "side find {:?} needs a summary and what you observed",
                find.summary
            );
        }
        let claim = posted.claim();
        self.commit(
            now,
            Event::Posted {
                token: token.into(),
                posted,
            },
        )?;
        self.activity.remove(token);
        Ok(claim)
    }

    /// The coordinator's call on a result waiting for review.
    pub fn review(
        &mut self,
        now: u64,
        spec: &str,
        platform: Platform,
        call: Call,
        reason: &str,
    ) -> Result<()> {
        self.open_for_commands()?;
        if reason.trim().is_empty() {
            bail!("every review call needs a one-line reason");
        }
        let record = self.record(spec, platform)?;
        let Row::NeedsReview { attempt, claim, .. } = &record.row else {
            bail!(
                "{spec} on {platform} isn't waiting for review: it's {}",
                describe(&record.row)
            );
        };
        let (attempt, claim) = (*attempt, *claim);
        match (&call, claim) {
            (Call::Uphold { severity: None }, Claim::Fail) => {
                bail!("upholding a FAIL needs a severity")
            }
            (Call::Uphold { .. }, Claim::Unsure) => {
                bail!("an Unsure can't be upheld: judge it, verify it or mark the spec unclear")
            }
            (Call::Judge { .. } | Call::SpecUnclear, c) if c != Claim::Unsure => {
                bail!(
                    "only an Unsure is judged or marked unclear; uphold, verify or send back this {c:?}"
                )
            }
            (
                Call::Judge {
                    result: Final::Fail,
                    severity: None,
                },
                _,
            ) => bail!("judging FAIL needs a severity"),
            (Call::Verify { .. } | Call::SendBack, _) if record.budget >= MAX_ATTEMPTS => {
                bail!("{spec} has used all {MAX_ATTEMPTS} attempts: file what's known")
            }
            (Call::Verify { claim }, _) if claim.trim().is_empty() => {
                bail!("say what the verification should reproduce")
            }
            _ => {}
        }
        self.commit(
            now,
            Event::Reviewed {
                spec: spec.into(),
                platform,
                attempt,
                call,
                reason: reason.into(),
            },
        )?;
        Ok(())
    }

    /// The coordinator's call on a side find: a problem with a severity, or no problem.
    pub fn review_find(
        &mut self,
        now: u64,
        id: u32,
        severity: Option<Severity>,
        reason: &str,
    ) -> Result<()> {
        self.open_for_commands()?;
        if reason.trim().is_empty() {
            bail!("every review call needs a one-line reason");
        }
        if !self.state.pending_finds.iter().any(|f| f.id == id) {
            bail!("side find {id} isn't waiting for review");
        }
        self.commit(
            now,
            Event::FindReviewed {
                id,
                severity,
                reason: reason.into(),
            },
        )
    }

    pub fn reset_failed(
        &mut self,
        now: u64,
        spec: &str,
        platform: Platform,
        reason: &str,
    ) -> Result<()> {
        self.open_for_commands()?;
        let record = self.record(spec, platform)?;
        if !matches!(record.row, Row::NotRun | Row::Queued { .. }) {
            bail!("{spec} on {platform} isn't waiting to run");
        }
        self.commit(
            now,
            Event::ResetFailed {
                spec: spec.into(),
                platform,
                reason: reason.into(),
            },
        )
    }

    /// A new build is on a platform. Reruns what it may have changed: every row on a native change, rows with open
    /// problems always (so a fix is proven on the build that has it), plus the rows the coordinator lists with a reason.
    /// Everything else keeps its result, stamped with the build it ran on.
    pub fn install_build(
        &mut self,
        now: u64,
        platform: Platform,
        build: Build,
        listed: Vec<Rerun>,
    ) -> Result<Vec<Rerun>> {
        self.open_for_commands()?;
        if build.commit.trim().is_empty() {
            bail!("a build needs its commit");
        }
        let busy: Vec<&String> = self
            .state
            .rows
            .iter()
            .filter(|((_, p), r)| {
                *p == platform && matches!(r.row, Row::Running { .. } | Row::NeedsReview { .. })
            })
            .map(|((spec, _), _)| spec)
            .collect();
        if !busy.is_empty() {
            bail!(
                "install between specs: on {platform}, {} still running or waiting for review",
                busy.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let native = self
            .state
            .builds
            .get(&platform)
            .is_some_and(|old| old.fingerprint != build.fingerprint);
        let mut reruns: Vec<Rerun> = Vec::new();
        let mut add = |spec: &str, reason: String| {
            if !reruns.iter().any(|r| r.spec == spec) {
                reruns.push(Rerun {
                    spec: spec.into(),
                    reason,
                });
            }
        };
        for ((spec, p), r) in &self.state.rows {
            if *p != platform || !matches!(r.row, Row::Filed { .. }) {
                continue;
            }
            if native {
                add(spec, "native change".into());
            } else if let Some(problem) = self.state.problems.iter().find(|pr| {
                &pr.spec == spec
                    && pr.platform == platform
                    && self.state.status_of(pr) == &Status::Open
            }) {
                add(spec, format!("open problem {}", problem.id));
            }
        }
        for rerun in listed {
            if rerun.reason.trim().is_empty() {
                bail!("say why {} should rerun", rerun.spec);
            }
            match self.state.rows.get(&(rerun.spec.clone(), platform)) {
                None => bail!("{} doesn't run on {platform}", rerun.spec),
                Some(r) if matches!(r.row, Row::Filed { .. }) => add(&rerun.spec, rerun.reason),
                // Not run yet, or already queued: it runs on the new build anyway.
                Some(_) => {}
            }
        }
        self.commit(
            now,
            Event::BuildInstalled {
                platform,
                build,
                reruns: reruns.clone(),
            },
        )?;
        Ok(reruns)
    }

    /// A tester's tool call began; time inside a call never counts towards a stall.
    pub fn call_started(&mut self, now: u64, token: &str) {
        if let Some(a) = self.activity.get_mut(token) {
            a.in_call += 1;
            a.last = now;
        }
    }

    pub fn call_finished(&mut self, now: u64, token: &str) {
        if let Some(a) = self.activity.get_mut(token) {
            a.in_call = a.in_call.saturating_sub(1);
            a.last = now;
        }
    }

    /// Tokens whose tester has been silent between calls for at least `silence_ms`.
    pub fn stalled(&self, now: u64, silence_ms: u64) -> Vec<String> {
        let mut tokens: Vec<String> = self
            .activity
            .iter()
            .filter(|(_, a)| a.in_call == 0 && now.saturating_sub(a.last) >= silence_ms)
            .map(|(t, _)| t.clone())
            .collect();
        tokens.sort();
        tokens
    }

    pub fn stall(&mut self, now: u64, token: &str, reason: &str) -> Result<()> {
        self.open_for_commands()?;
        self.running(token)?;
        self.commit(
            now,
            Event::Stalled {
                token: token.into(),
                reason: reason.into(),
            },
        )?;
        self.activity.remove(token);
        Ok(())
    }

    /// Records a triage decision. Fixed needs its row to have passed on a later attempt than the one that found it.
    pub fn triage(
        &mut self,
        now: u64,
        problem: u32,
        decision: Decision,
        reason: &str,
    ) -> Result<()> {
        if reason.trim().is_empty() {
            bail!("every triage decision needs a reason");
        }
        let Some(p) = self.state.problem(problem) else {
            bail!("no problem {problem}");
        };
        match &decision {
            Decision::Fix { ticket } | Decision::Defer { ticket } if ticket.trim().is_empty() => {
                bail!("fixed and deferred problems need a ticket")
            }
            Decision::Fix { .. } => {
                let passed_later = matches!(
                    self.row(&p.spec, p.platform).map(|r| &r.row),
                    Some(Row::Filed { result: Final::Pass, attempt, .. }) if *attempt > p.attempt
                );
                if !passed_later {
                    bail!(
                        "problem {problem} can't be Fixed until {} passes on {} again with the fix",
                        p.spec,
                        p.platform
                    );
                }
            }
            Decision::Duplicate { of } => {
                if *of == problem {
                    bail!("a problem can't duplicate itself");
                }
                match self.state.problem(*of) {
                    None => bail!("no problem {of}"),
                    Some(o) if matches!(o.status, Status::Duplicate { .. }) => {
                        bail!("problem {of} is itself a duplicate; point at its original")
                    }
                    Some(_) => {}
                }
                if self
                    .state
                    .problems
                    .iter()
                    .any(|o| o.status == Status::Duplicate { of: problem })
                {
                    bail!("other problems are duplicates of {problem}; it can't become one itself");
                }
            }
            _ => {}
        }
        self.commit(
            now,
            Event::Triaged {
                problem,
                decision,
                reason: reason.into(),
            },
        )
    }

    pub fn pause(&mut self, now: u64, reason: &str) -> Result<()> {
        self.open_for_commands()?;
        if self.state.paused {
            bail!("already paused");
        }
        self.commit(
            now,
            Event::Paused {
                reason: reason.into(),
            },
        )
    }

    pub fn resume(&mut self, now: u64) -> Result<()> {
        self.open_for_commands()?;
        if !self.state.paused {
            bail!("not paused");
        }
        self.commit(now, Event::Resumed)
    }

    /// What stops the run finishing: anything running, waiting for review or queued for another attempt.
    pub fn finish_blockers(&self) -> Vec<String> {
        let mut blockers: Vec<String> = self
            .state
            .rows
            .iter()
            .filter_map(|((spec, platform), r)| match &r.row {
                Row::Running { .. } => Some(format!("{spec} is running on {platform}")),
                Row::NeedsReview { .. } => Some(format!("{spec} on {platform} needs review")),
                Row::Queued { .. } => Some(format!(
                    "{spec} on {platform} is queued for another attempt"
                )),
                _ => None,
            })
            .collect();
        blockers.extend(self.state.pending_finds.iter().map(|f| {
            format!(
                "side find {} on {} ({}) needs review",
                f.id, f.spec, f.platform
            )
        }));
        blockers
    }

    pub fn finish(&mut self, now: u64) -> Result<ResultCounts> {
        self.open_for_commands()?;
        let blockers = self.finish_blockers();
        if !blockers.is_empty() {
            bail!("can't finish yet: {}", blockers.join("; "));
        }
        let mut counts = ResultCounts::default();
        for r in self.state.rows.values() {
            counts.add(match r.row {
                Row::Filed { result, .. } => Some(result),
                _ => None,
            });
        }
        self.commit(
            now,
            Event::Finished {
                result_counts: counts.clone(),
            },
        )?;
        Ok(counts)
    }

    fn open_for_commands(&self) -> Result<()> {
        if self.state.finished {
            bail!("the run has finished");
        }
        Ok(())
    }

    fn record(&self, spec: &str, platform: Platform) -> Result<&RowRecord> {
        match self.state.rows.get(&(spec.to_string(), platform)) {
            Some(r) => Ok(r),
            None if self.plan.specs.iter().any(|s| s.id == spec) => {
                bail!("{spec} doesn't run on {platform}")
            }
            None => bail!("no spec {spec} in this run"),
        }
    }

    fn planned(&self, spec: &str) -> Result<&PlannedSpec> {
        match self.plan.specs.iter().find(|s| s.id == spec) {
            Some(s) => Ok(s),
            None => bail!("no spec {spec} in this run"),
        }
    }

    fn running(&self, token: &str) -> Result<(String, Platform)> {
        match self.state.running_token(token) {
            Some(key) => Ok(key),
            None => {
                bail!("token {token} isn't a running spec: it finished, stalled or never started")
            }
        }
    }

    fn commit(&mut self, now: u64, event: Event) -> Result<()> {
        let entry = self.journal.append(now, event)?;
        self.state.apply(&self.plan, &entry);
        self.entries.push(entry);
        Ok(())
    }
}

impl State {
    fn new(plan: &Plan) -> State {
        let mut state = State {
            next_id: 1,
            ..State::default()
        };
        for spec in &plan.specs {
            for &platform in &spec.platforms {
                state.rows.insert(
                    (spec.id.clone(), platform),
                    RowRecord {
                        row: Row::NotRun,
                        attempts: 0,
                        budget: 0,
                        sightings: 0,
                        build: None,
                    },
                );
            }
        }
        state
    }

    pub fn problem(&self, id: u32) -> Option<&Problem> {
        self.problems.iter().find(|p| p.id == id)
    }

    /// A problem's effective status: a duplicate takes its original's.
    pub fn status_of<'a>(&'a self, problem: &'a Problem) -> &'a Status {
        match &problem.status {
            Status::Duplicate { of } => self.problem(*of).map_or(&problem.status, |o| &o.status),
            status => status,
        }
    }

    fn running_token(&self, token: &str) -> Option<(String, Platform)> {
        self.rows.iter().find_map(|(key, r)| match &r.row {
            Row::Running { token: t, .. } if t == token => Some(key.clone()),
            _ => None,
        })
    }

    fn next_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Applies one journalled event. Commands validated it before it was written, so this never fails.
    fn apply(&mut self, plan: &Plan, entry: &Entry) {
        match &entry.event {
            Event::Dispatched {
                spec,
                platform,
                attempt,
                token,
                verify,
                ..
            } => {
                let build = self.builds.get(platform).cloned();
                if let Some(r) = self.rows.get_mut(&(spec.clone(), *platform)) {
                    r.attempts = *attempt;
                    r.budget += 1;
                    r.build = build;
                    r.row = Row::Running {
                        attempt: *attempt,
                        token: token.clone(),
                        verify: verify.clone(),
                    };
                }
            }
            Event::Posted { token, posted } => {
                let Some(key) = self.running_token(token) else {
                    return;
                };
                let (spec, platform) = key.clone();
                let Some(Row::Running {
                    attempt, verify, ..
                }) = self.rows.get(&key).map(|r| r.row.clone())
                else {
                    return;
                };
                for find in &posted.side_finds {
                    let id = self.next_id();
                    self.pending_finds.push(PendingFind {
                        id,
                        spec: spec.clone(),
                        platform,
                        attempt,
                        find: find.clone(),
                    });
                }
                let claim = posted.claim();
                if claim == Claim::Pass {
                    if let Some(original) = &verify {
                        let r = &self.rows[&key];
                        let summary = format!(
                            "Not reproduced ({} of {}): {original}",
                            r.sightings, r.budget
                        );
                        let id = self.next_id();
                        self.problems.push(Problem {
                            id,
                            spec: spec.clone(),
                            platform,
                            attempt,
                            severity: Severity::Finding,
                            source: ProblemSource::NotReproduced,
                            summary,
                            verified: true,
                            reason: "a verification attempt passed".into(),
                            status: Status::Open,
                        });
                    }
                    let reason = if verify.is_some() {
                        "passed on verification"
                    } else {
                        "passed"
                    };
                    self.rows.get_mut(&key).unwrap().row = Row::Filed {
                        result: Final::Pass,
                        attempt,
                        reason: reason.into(),
                    };
                } else {
                    let r = self.rows.get_mut(&key).unwrap();
                    r.sightings += 1;
                    r.row = Row::NeedsReview {
                        attempt,
                        claim,
                        posted: posted.clone(),
                        verify,
                        since: entry.at,
                    };
                }
            }
            Event::Reviewed {
                spec,
                platform,
                attempt,
                call,
                reason,
            } => {
                let key = (spec.clone(), *platform);
                let Some(Row::NeedsReview {
                    claim,
                    posted,
                    verify,
                    ..
                }) = self.rows.get(&key).map(|r| r.row.clone())
                else {
                    return;
                };
                let outcomes = plan
                    .specs
                    .iter()
                    .find(|s| &s.id == spec)
                    .map(|s| s.outcomes.clone())
                    .unwrap_or_default();
                let summary = |answers: &[Answer]| -> String {
                    posted
                        .judgments
                        .iter()
                        .filter(|j| answers.contains(&j.answer))
                        .map(|j| {
                            format!(
                                "{}: {}",
                                outcomes
                                    .get(j.outcome)
                                    .map_or("?", |o| o.trim_end_matches('.')),
                                j.observed
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                };
                let file = |result: Final| Row::Filed {
                    result,
                    attempt: *attempt,
                    reason: reason.clone(),
                };
                let (row, problem): (Row, Option<(Severity, String)>) = match call {
                    Call::Uphold { severity } => match claim {
                        Claim::Fail => (
                            file(Final::Fail),
                            severity.map(|s| (s, summary(&[Answer::Failed]))),
                        ),
                        _ => (
                            file(Final::Blocked),
                            severity.map(|s| (s, summary(&[Answer::CouldntCheck]))),
                        ),
                    },
                    Call::Judge { result, severity } => {
                        let found = summary(&[Answer::Unsure, Answer::Failed]);
                        (
                            file(*result),
                            severity
                                .filter(|_| *result != Final::Pass)
                                .map(|s| (s, found)),
                        )
                    }
                    Call::SpecUnclear => {
                        self.unclear_specs.insert(spec.clone());
                        (
                            Row::Filed {
                                result: Final::Blocked,
                                attempt: *attempt,
                                reason: format!("spec unclear: {reason}"),
                            },
                            None,
                        )
                    }
                    Call::Verify { claim } => (
                        Row::Queued {
                            verify: Some(claim.clone()),
                        },
                        None,
                    ),
                    Call::SendBack => (Row::Queued { verify: None }, None),
                };
                if let (
                    Row::Filed {
                        result: Final::Blocked,
                        ..
                    },
                    None,
                    Call::Uphold { .. },
                ) = (&row, &problem, call)
                {
                    self.notes.push(Note {
                        spec: spec.clone(),
                        platform: *platform,
                        text: format!("harness gap: {reason}"),
                    });
                }
                if let Some((severity, summary)) = problem {
                    let id = self.next_id();
                    self.problems.push(Problem {
                        id,
                        spec: spec.clone(),
                        platform: *platform,
                        attempt: *attempt,
                        severity,
                        source: ProblemSource::Row,
                        summary,
                        verified: verify.is_some(),
                        reason: reason.clone(),
                        status: Status::Open,
                    });
                }
                self.rows.get_mut(&key).unwrap().row = row;
            }
            Event::FindReviewed {
                id,
                severity,
                reason,
            } => {
                let Some(pos) = self.pending_finds.iter().position(|f| f.id == *id) else {
                    return;
                };
                let pending = self.pending_finds.remove(pos);
                if let Some(severity) = severity {
                    self.problems.push(Problem {
                        id: pending.id,
                        spec: pending.spec,
                        platform: pending.platform,
                        attempt: pending.attempt,
                        severity: *severity,
                        source: ProblemSource::SideFind,
                        summary: format!("{}: {}", pending.find.summary, pending.find.observed),
                        verified: false,
                        reason: reason.clone(),
                        status: Status::Open,
                    });
                }
            }
            Event::Stalled { token, reason } => {
                let Some(key) = self.running_token(token) else {
                    return;
                };
                let r = self.rows.get_mut(&key).unwrap();
                let Row::Running {
                    attempt, verify, ..
                } = r.row.clone()
                else {
                    return;
                };
                r.row = if r.budget >= MAX_ATTEMPTS {
                    Row::Filed {
                        result: Final::Blocked,
                        attempt,
                        reason: format!("stalled on the last attempt: {reason}"),
                    }
                } else {
                    Row::Queued { verify }
                };
            }
            Event::ResetFailed {
                spec,
                platform,
                reason,
            } => {
                if let Some(r) = self.rows.get_mut(&(spec.clone(), *platform)) {
                    r.row = Row::Filed {
                        result: Final::Blocked,
                        attempt: r.attempts,
                        reason: format!("reset failed: {reason}"),
                    };
                    self.notes.push(Note {
                        spec: spec.clone(),
                        platform: *platform,
                        text: format!("reset failed: {reason}"),
                    });
                }
            }
            Event::Triaged {
                problem,
                decision,
                reason,
            } => {
                if let Some(p) = self.problems.iter_mut().find(|p| p.id == *problem) {
                    let reason = reason.clone();
                    p.status = match decision {
                        Decision::Dismiss => Status::Dismissed { reason },
                        Decision::Fix { ticket } => Status::Fixed {
                            ticket: ticket.clone(),
                            reason,
                        },
                        Decision::Defer { ticket } => Status::Deferred {
                            ticket: ticket.clone(),
                            reason,
                        },
                        Decision::Duplicate { of } => Status::Duplicate { of: *of },
                    };
                }
            }
            Event::BuildInstalled {
                platform,
                build,
                reruns,
            } => {
                self.builds.insert(*platform, build.clone());
                for rerun in reruns {
                    if let Some(r) = self.rows.get_mut(&(rerun.spec.clone(), *platform)) {
                        r.row = Row::Queued { verify: None };
                        r.budget = 0;
                        r.sightings = 0;
                    }
                }
                self.notes.extend(reruns.iter().map(|rerun| Note {
                    spec: rerun.spec.clone(),
                    platform: *platform,
                    text: format!("rerun on {}: {}", build.label(), rerun.reason),
                }));
            }
            Event::Paused { .. } => self.paused = true,
            Event::Resumed => self.paused = false,
            Event::Finished { .. } => self.finished = true,
        }
    }
}

fn describe(row: &Row) -> &'static str {
    match row {
        Row::NotRun => "not run",
        Row::Queued { .. } => "queued for another attempt",
        Row::Running { .. } => "running",
        Row::NeedsReview { .. } => "waiting for review",
        Row::Filed { .. } => "already filed",
    }
}
