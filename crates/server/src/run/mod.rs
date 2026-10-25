//! A run's results engine: row states, review, attempts, stalls and the finish gate, journalled and replayable.

pub mod event;
pub mod journal;
pub mod model;
pub mod saved;
pub mod verdict;

use crate::suite::config::Platform;
use anyhow::{Result, bail};
use event::{Event, ResultCounts, RowRef};
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
    /// When the latest attempt was dispatched.
    pub dispatched_at: Option<u64>,
    /// What the latest attempt posted, kept after filing so the page can show what each outcome rested on.
    pub posted: Option<Posted>,
    /// Whether the latest attempt was a verification.
    pub verifying: bool,
    /// The claim the latest verification was sent to check; only a FAIL that doesn't reproduce is a finding.
    pub verified_claim: Option<Claim>,
    /// The coordinator's call on the latest attempt, with its reason.
    pub review: Option<(Call, String)>,
    /// Evidence the server saved for the latest attempt.
    pub captured: Vec<String>,
    /// Why the device was degraded during the latest attempt; empty when it was healthy.
    pub degraded: Vec<String>,
    /// What the running attempt's tester has judged so far, before it posts; the post takes them up.
    pub provisional: Vec<Judgment>,
    /// The outcomes' wording the latest result was judged against; empty in journals from before it was kept.
    pub judged_against: Vec<String>,
    /// What the coordinator wants the next attempt told, after fixing a harness gap.
    pub rerun_note: Option<String>,
}

impl RowRecord {
    fn new() -> RowRecord {
        RowRecord {
            row: Row::NotRun,
            attempts: 0,
            budget: 0,
            sightings: 0,
            build: None,
            dispatched_at: None,
            posted: None,
            verifying: false,
            verified_claim: None,
            review: None,
            captured: Vec::new(),
            degraded: Vec::new(),
            rerun_note: None,
            judged_against: Vec::new(),
            provisional: Vec::new(),
        }
    }
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
    /// Whether the tester has made its first tool call; until then the longer start limit applies.
    started: bool,
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
                        started: true,
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
        let (attempt, verify) = self.check_dispatch(spec, platform)?;
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
                started: false,
            },
        );
        Ok(token)
    }

    /// Whether a spec could be dispatched now, without dispatching it: the server checks before it seeds, so a
    /// refused dispatch never resets a device. Returns the attempt it would be and any claim to verify.
    pub fn check_dispatch(&self, spec: &str, platform: Platform) -> Result<(u32, Option<String>)> {
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
        Ok((attempt, verify))
    }

    /// A tester's result. A PASS files at once; anything else waits for review.
    pub fn post(&mut self, now: u64, token: &str, posted: Posted) -> Result<Claim> {
        self.post_probed(now, token, posted, false)
    }

    /// Records one outcome a tester has settled, before it posts. Returns how many it has judged so far.
    pub fn judge(&mut self, now: u64, token: &str, judgment: Judgment) -> Result<usize> {
        self.open_for_commands()?;
        let (spec, platform) = self.running(token)?;
        let outcomes = self.planned(&spec)?.outcomes.len();
        if judgment.outcome >= outcomes {
            bail!(
                "outcome {} doesn't exist: the spec has {outcomes}",
                judgment.outcome
            );
        }
        if judgment.observed.trim().is_empty() {
            bail!("outcome {} needs what you observed", judgment.outcome);
        }
        self.commit(
            now,
            Event::Judged {
                token: token.into(),
                judgment,
            },
        )?;
        Ok(self.record(&spec, platform)?.provisional.len())
    }

    /// Records a tester's result. When the spec has probes and a pass says nothing about them, it waits for review
    /// as an Unsure, since probes are where new bugs turn up.
    pub fn post_probed(
        &mut self,
        now: u64,
        token: &str,
        mut posted: Posted,
        has_probes: bool,
    ) -> Result<Claim> {
        self.open_for_commands()?;
        let (spec, platform) = self.running(token)?;
        // Outcomes judged along the way count unless the post judges them again.
        for j in &self.record(&spec, platform)?.provisional {
            if !posted.judgments.iter().any(|p| p.outcome == j.outcome) {
                posted.judgments.push(j.clone());
            }
        }
        posted.judgments.sort_by_key(|j| j.outcome);
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
        let unprobed = has_probes
            && posted.claim() == Claim::Pass
            && posted.probed.as_deref().is_none_or(|p| p.trim().is_empty());
        let claim = if unprobed {
            Claim::Unsure
        } else {
            posted.claim()
        };
        let find_ids = (0..posted.side_finds.len() as u32)
            .map(|i| self.state.next_id + i)
            .collect();
        let outcomes = planned.outcomes.clone();
        self.commit(
            now,
            Event::Posted {
                token: token.into(),
                posted,
                unprobed,
                outcomes,
                find_ids,
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
        let files_failure = matches!(
            (&call, claim),
            (Call::Uphold { .. }, Claim::Fail)
                | (
                    Call::Uphold {
                        cause: Some(Cause::App),
                        ..
                    },
                    Claim::Blocked
                )
                | (
                    Call::Judge {
                        result: Final::Fail,
                        ..
                    },
                    _
                )
        );
        if files_failure && !record.degraded.is_empty() && !record.verifying {
            bail!(
                "the device was degraded during this attempt ({}): verify it on a healthy device before filing a failure",
                record.degraded.join("; ")
            );
        }
        match (&call, claim) {
            (Call::Uphold { title, .. }, Claim::Fail) if blank(title) => {
                bail!(
                    "upholding a FAIL lists it in Problems as a failure: give it a title in plain words"
                )
            }
            (Call::Uphold { cause: None, .. }, Claim::Blocked) => {
                bail!("upholding a BLOCKED needs its cause: app or harness")
            }
            (
                Call::Uphold {
                    cause: Some(Cause::App),
                    title,
                },
                Claim::Blocked,
            ) if blank(title) => {
                bail!(
                    "the app stopped the test, so it failed: give the failure a title in plain words"
                )
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
                    title,
                },
                _,
            ) if blank(title) => {
                bail!(
                    "judging FAIL lists it in Problems as a failure: give it a title in plain words"
                )
            }
            (
                Call::Judge {
                    result: Final::Blocked,
                    ..
                },
                _,
            ) => bail!("an Unsure is judged PASS or FAIL; if it can't be, mark the spec unclear"),
            (Call::Verify { .. } | Call::SendBack | Call::Harness { rerun: Some(_), .. }, _)
                if record.budget >= MAX_ATTEMPTS =>
            {
                bail!("{spec} has used all {MAX_ATTEMPTS} attempts: file what's known")
            }
            (Call::Harness { rerun: Some(note) }, _) if note.trim().is_empty() => {
                bail!("say what the rerun should be told, or leave rerun out to file it BLOCKED")
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

    /// The coordinator's call on a side find: a finding with a title in plain words, or not a problem. `keep` names
    /// the find's own screenshots on its topic.
    pub fn review_find(
        &mut self,
        now: u64,
        id: u32,
        title: Option<String>,
        keep: Vec<String>,
        reason: &str,
    ) -> Result<()> {
        self.open_for_commands()?;
        if reason.trim().is_empty() {
            bail!("every review call needs a one-line reason");
        }
        let Some(pending) = self.state.pending_finds.iter().find(|f| f.id == id) else {
            bail!("side find {id} isn't waiting for review");
        };
        if title.as_deref().is_some_and(|t| t.trim().is_empty()) {
            bail!("a finding's title can't be empty");
        }
        if title.is_none() && !keep.is_empty() {
            bail!("a side find that isn't a problem keeps no screenshots");
        }
        if let Some(name) = keep.iter().find(|k| !pending.find.evidence.contains(k)) {
            bail!(
                "side find {id} has no screenshot {name}; keep only names from its evidence: {}",
                pending.find.evidence.join(", ")
            );
        }
        self.commit(
            now,
            Event::FindReviewed {
                id,
                title,
                reason: reason.into(),
                keep,
            },
        )
    }

    /// Records the device's health after an attempt; a degraded device notes it on the row.
    pub fn device_health(
        &mut self,
        now: u64,
        spec: &str,
        platform: Platform,
        attempt: u32,
        health: soundcheck_driver::Health,
    ) -> Result<()> {
        let record = self.record(spec, platform)?;
        if record.attempts != attempt {
            bail!("{spec} on {platform} has moved on from attempt {attempt}");
        }
        self.commit(
            now,
            Event::DeviceHealth {
                spec: spec.into(),
                platform,
                attempt,
                degraded: health.degraded,
                busy: health.busy,
                facts: health.facts,
            },
        )
    }

    /// Records that the server held a spec while the host was busy.
    pub fn host_busy(
        &mut self,
        now: u64,
        platform: Platform,
        spec: &str,
        reason: &str,
        waited_ms: u64,
        settled: bool,
    ) -> Result<()> {
        self.commit(
            now,
            Event::HostBusy {
                platform,
                spec: spec.into(),
                reason: reason.into(),
                waited_ms,
                settled,
            },
        )
    }

    /// Records that the server restarted a degraded device.
    pub fn device_restarted(
        &mut self,
        now: u64,
        platform: Platform,
        spec: &str,
        reason: &str,
    ) -> Result<()> {
        self.commit(
            now,
            Event::DeviceRestarted {
                platform,
                spec: spec.into(),
                reason: reason.into(),
            },
        )
    }

    /// Records the evidence the server saved for an attempt's non-pass.
    pub fn evidence_captured(
        &mut self,
        now: u64,
        spec: &str,
        platform: Platform,
        attempt: u32,
        files: Vec<String>,
    ) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let record = self.record(spec, platform)?;
        if record.attempts != attempt {
            bail!("{spec} on {platform} has moved on from attempt {attempt}");
        }
        self.commit(
            now,
            Event::EvidenceCaptured {
                spec: spec.into(),
                platform,
                attempt,
                files,
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
        // Allowed on a finished run, which its reruns open again: a harness fix after finishing must reach its rows.
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
            a.started = true;
        }
    }

    pub fn call_finished(&mut self, now: u64, token: &str) {
        if let Some(a) = self.activity.get_mut(token) {
            a.in_call = a.in_call.saturating_sub(1);
            a.last = now;
        }
    }

    /// Tokens whose tester has been silent between calls for at least `silence_ms`, or, before its first call, for
    /// at least `start_ms` since dispatch: a slow network can delay a tester's first call by minutes.
    pub fn stalled(&self, now: u64, silence_ms: u64, start_ms: u64) -> Vec<String> {
        let mut tokens: Vec<String> = self
            .activity
            .iter()
            .filter(|(_, a)| {
                let limit = if a.started { silence_ms } else { start_ms };
                a.in_call == 0 && now.saturating_sub(a.last) >= limit
            })
            .map(|(t, _)| t.clone())
            .collect();
        tokens.sort();
        tokens
    }

    /// Whether a running token's tester has made any tool call yet.
    pub fn started(&self, token: &str) -> bool {
        self.activity.get(token).is_some_and(|a| a.started)
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

    /// On resume: every spec that was running when the server stopped goes back to the queue, without using an
    /// attempt, since the harness stopped it rather than the app.
    pub fn preflight(&mut self, now: u64, checks: Vec<String>) -> Result<()> {
        self.commit(now, Event::Preflight { checks })
    }

    pub fn interrupt_running(&mut self, now: u64) -> Result<usize> {
        let tokens: Vec<String> = self
            .state
            .rows
            .values()
            .filter_map(|r| match &r.row {
                Row::Running { token, .. } => Some(token.clone()),
                _ => None,
            })
            .collect();
        for token in &tokens {
            self.commit(
                now,
                Event::Interrupted {
                    token: token.clone(),
                },
            )?;
            self.activity.remove(token);
        }
        Ok(tokens.len())
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
            Decision::Fix { ticket: Some(t) } | Decision::Defer { ticket: Some(t) }
                if t.trim().is_empty() =>
            {
                bail!("a ticket can't be blank; leave it out when the report is the only record")
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

    /// A screenshot already saved in the run's evidence joins a problem's. Like triage, it works after finishing.
    pub fn add_evidence(&mut self, now: u64, problem: u32, file: &str, reason: &str) -> Result<()> {
        if reason.trim().is_empty() {
            bail!("adding evidence needs a one-line reason");
        }
        let Some(p) = self.state.problem(problem) else {
            bail!("no problem {problem}");
        };
        if p.evidence.iter().any(|e| e == file) {
            bail!("problem {problem} already shows {file}");
        }
        self.commit(
            now,
            Event::EvidenceAdded {
                problem,
                file: file.into(),
                reason: reason.into(),
            },
        )
    }

    /// On resume: platforms the run didn't cover join it, with rows from the plan. A finished run opens again,
    /// since its new rows haven't run.
    pub fn add_platforms(&mut self, now: u64, platforms: Vec<Platform>) -> Result<()> {
        if platforms.is_empty() {
            bail!("no platforms to add");
        }
        for platform in &platforms {
            if !self.state.rows.keys().any(|(_, p)| p == platform) {
                bail!("no spec in the run's scope runs on {platform}");
            }
        }
        self.commit(now, Event::PlatformsAdded { platforms })
    }

    /// Specs the run didn't cover join it, each with a row per platform it runs on; returns the rows added. A
    /// finished run opens again, since its new rows haven't run.
    pub fn add_specs(
        &mut self,
        now: u64,
        specs: Vec<PlannedSpec>,
    ) -> Result<Vec<(String, Platform)>> {
        let specs: Vec<PlannedSpec> = specs
            .into_iter()
            .filter(|s| !self.plan.specs.iter().any(|p| p.id == s.id))
            .collect();
        if specs.is_empty() {
            bail!("every spec named is already in the run");
        }
        if let Some(s) = specs.iter().find(|s| s.platforms.is_empty()) {
            bail!("{} runs on none of the run's platforms", s.id);
        }
        self.commit(
            now,
            Event::SpecsAdded {
                specs: specs.iter().map(|s| s.id.clone()).collect(),
            },
        )?;
        let mut added = Vec::new();
        for spec in specs {
            for &platform in &spec.platforms {
                self.state
                    .rows
                    .insert((spec.id.clone(), platform), RowRecord::new());
                added.push((spec.id.clone(), platform));
            }
            self.plan.specs.push(spec);
        }
        Ok(added)
    }

    /// Queues rows again whatever state they're in, with a fresh attempt budget; returns each with what it was. A
    /// running attempt is abandoned (its token stops working) and a claim waiting for review is set aside unjudged.
    /// Allowed on a finished run, which opens again.
    pub fn requeue(
        &mut self,
        now: u64,
        rows: Vec<RowRef>,
        reason: &str,
        note: Option<String>,
    ) -> Result<Vec<(RowRef, &'static str)>> {
        if reason.trim().is_empty() {
            bail!("say why these rows run again: the report shows it beside their results");
        }
        if rows.is_empty() {
            bail!("name at least one row");
        }
        let mut was = Vec::new();
        let mut abandoned = Vec::new();
        for r in &rows {
            let record = self.record(&r.spec, r.platform)?;
            if let Row::Running { token, .. } = &record.row {
                abandoned.push(token.clone());
            }
            was.push((r.clone(), describe(&record.row)));
        }
        let note = note.filter(|n| !n.trim().is_empty());
        self.commit(
            now,
            Event::Requeued {
                rows,
                reason: reason.into(),
                note,
            },
        )?;
        for token in abandoned {
            self.activity.remove(&token);
        }
        Ok(was)
    }

    /// Takes up a spec's new text, read from disk when it was queued again; later attempts judge `outcomes`.
    pub fn spec_changed(&mut self, now: u64, spec: &str, outcomes: Vec<String>) -> Result<()> {
        let Some(planned) = self.plan.specs.iter_mut().find(|s| s.id == spec) else {
            bail!("no spec {spec} in this run");
        };
        planned.outcomes = outcomes;
        self.commit(now, Event::SpecChanged { spec: spec.into() })
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
        let mut failures: Vec<String> = self
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
        failures.extend(self.state.pending_finds.iter().map(|f| {
            format!(
                "side find {} on {} ({}) needs review",
                f.id, f.spec, f.platform
            )
        }));
        failures
    }

    pub fn finish(&mut self, now: u64) -> Result<ResultCounts> {
        self.open_for_commands()?;
        let failures = self.finish_blockers();
        if !failures.is_empty() {
            bail!("can't finish yet: {}", failures.join("; "));
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

    /// The spec and platform a running token belongs to.
    pub fn running(&self, token: &str) -> Result<(String, Platform)> {
        match self.state.running_token(token) {
            Some(key) => Ok(key),
            None => {
                bail!("token {token} isn't a running spec: it finished, stalled or never started")
            }
        }
    }

    /// The brief a running token's tester was sent, as journalled at dispatch.
    pub fn brief(&self, token: &str) -> Result<String> {
        self.running(token)?;
        let sent = self.entries.iter().rev().find_map(|e| match &e.event {
            Event::Dispatched {
                token: t, prompt, ..
            } if t == token => Some(prompt.clone()),
            _ => None,
        });
        match sent {
            Some(prompt) => Ok(prompt),
            None => bail!("no brief was journalled for {token}"),
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
                state
                    .rows
                    .insert((spec.id.clone(), platform), RowRecord::new());
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

    /// The screenshots of a row's latest attempt, if it's `attempt`: what the tester attached, then the server's.
    fn shots(&self, key: &(String, Platform), attempt: u32) -> Vec<String> {
        let Some(r) = self.rows.get(key).filter(|r| r.attempts == attempt) else {
            return Vec::new();
        };
        let mut shots: Vec<String> = Vec::new();
        let attached = r
            .posted
            .iter()
            .flat_map(|p| &p.judgments)
            .flat_map(|j| &j.evidence);
        for name in attached.chain(r.captured.iter().filter(|f| f.ends_with(".png"))) {
            if !shots.contains(name) {
                shots.push(name.clone());
            }
        }
        shots
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
                    r.dispatched_at = Some(entry.at);
                    r.posted = None;
                    r.verifying = verify.is_some();
                    r.review = None;
                    r.captured.clear();
                    r.degraded.clear();
                    r.rerun_note = None;
                    r.provisional.clear();
                    r.build = build;
                    r.row = Row::Running {
                        attempt: *attempt,
                        token: token.clone(),
                        verify: verify.clone(),
                    };
                }
            }
            Event::Judged { token, judgment } => {
                if let Some(key) = self.running_token(token)
                    && let Some(r) = self.rows.get_mut(&key)
                {
                    r.provisional.retain(|j| j.outcome != judgment.outcome);
                    r.provisional.push(judgment.clone());
                    r.provisional.sort_by_key(|j| j.outcome);
                }
            }
            Event::Posted {
                token,
                posted,
                unprobed,
                outcomes,
                ..
            } => {
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
                if let Some(r) = self.rows.get_mut(&key) {
                    r.posted = Some(posted.clone());
                    r.judged_against.clone_from(outcomes);
                }
                let claim = if *unprobed {
                    Claim::Unsure
                } else {
                    posted.claim()
                };
                if claim == Claim::Pass {
                    if let Some(original) = &verify
                        && self.rows[&key].verified_claim == Some(Claim::Fail)
                    {
                        let r = &self.rows[&key];
                        let title = format!(
                            "Not reproduced ({} of {}): {original}",
                            r.sightings, r.budget
                        );
                        let id = self.next_id();
                        self.problems.push(Problem {
                            id,
                            spec: spec.clone(),
                            platform,
                            attempt,
                            kind: ProblemKind::Finding,
                            source: ProblemSource::NotReproduced,
                            title,
                            detail: "A verification attempt passed.".into(),
                            verified: true,
                            reason: "a verification attempt passed".into(),
                            status: Status::Open,
                            evidence: Vec::new(),
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
                let outcomes = match self.rows.get(&key).map(|r| &r.judged_against) {
                    Some(judged) if !judged.is_empty() => judged.clone(),
                    _ => plan
                        .specs
                        .iter()
                        .find(|s| &s.id == spec)
                        .map(|s| s.outcomes.clone())
                        .unwrap_or_default(),
                };
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
                // Every problem a review makes is a failure; findings come only from side finds and verifications.
                let failure = |title: &Option<String>, answers: &[Answer]| {
                    title.clone().map(|t| (t, summary(answers)))
                };
                let (row, problem): (Row, Option<(String, String)>) = match call {
                    Call::Uphold { title, cause } => match claim {
                        Claim::Fail => (file(Final::Fail), failure(title, &[Answer::Failed])),
                        // BLOCKED means only that the environment or harness stopped the test; if the app did, it failed.
                        _ if *cause == Some(Cause::App) => {
                            (file(Final::Fail), failure(title, &[Answer::CouldntCheck]))
                        }
                        _ => (file(Final::Blocked), None),
                    },
                    Call::Judge { result, title } => (
                        file(*result),
                        if *result == Final::Fail {
                            failure(title, &[Answer::Unsure, Answer::Failed])
                        } else {
                            None
                        },
                    ),
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
                    Call::Harness { rerun: None } => (file(Final::Blocked), None),
                    Call::Harness { rerun: Some(_) } => (Row::Queued { verify: None }, None),
                };
                if let (
                    Row::Filed {
                        result: Final::Blocked,
                        ..
                    },
                    None,
                    Call::Uphold { .. } | Call::Harness { .. },
                ) = (&row, &problem, call)
                {
                    self.notes.push(Note {
                        spec: spec.clone(),
                        platform: *platform,
                        text: format!("harness gap: {reason}"),
                    });
                }
                if let Some((title, detail)) = problem {
                    let id = self.next_id();
                    let evidence = self.shots(&key, *attempt);
                    self.problems.push(Problem {
                        id,
                        spec: spec.clone(),
                        platform: *platform,
                        attempt: *attempt,
                        kind: ProblemKind::Failure,
                        source: ProblemSource::Row,
                        title,
                        detail,
                        verified: verify.is_some(),
                        reason: reason.clone(),
                        status: Status::Open,
                        evidence,
                    });
                }
                let r = self.rows.get_mut(&key).unwrap();
                r.row = row;
                r.review = Some((call.clone(), reason.clone()));
                if let Call::Verify { .. } = call {
                    r.verified_claim = Some(claim);
                }
                if let Call::Harness { rerun } = call {
                    r.rerun_note.clone_from(rerun);
                }
            }
            Event::FindReviewed {
                id,
                title,
                reason,
                keep,
            } => {
                let Some(pos) = self.pending_finds.iter().position(|f| f.id == *id) else {
                    return;
                };
                let pending = self.pending_finds.remove(pos);
                if let Some(title) = title {
                    let evidence = keep.clone();
                    self.problems.push(Problem {
                        id: pending.id,
                        spec: pending.spec,
                        platform: pending.platform,
                        attempt: pending.attempt,
                        kind: ProblemKind::Finding,
                        source: ProblemSource::SideFind,
                        title: title.clone(),
                        detail: format!("{}: {}", pending.find.summary, pending.find.observed),
                        verified: false,
                        reason: reason.clone(),
                        status: Status::Open,
                        evidence,
                    });
                }
            }
            Event::SpecChanged { spec } => {
                for platform in self
                    .rows
                    .keys()
                    .filter(|(s, _)| s == spec)
                    .map(|(_, p)| *p)
                    .collect::<Vec<_>>()
                {
                    self.notes.push(Note {
                        spec: spec.clone(),
                        platform,
                        text: "spec changed since the run started: later attempts use the new text"
                            .into(),
                    });
                }
            }
            Event::Requeued { rows, reason, note } => {
                for row in rows {
                    let Some(r) = self.rows.get_mut(&(row.spec.clone(), row.platform)) else {
                        continue;
                    };
                    let was = describe(&r.row);
                    if !matches!(r.row, Row::NotRun) {
                        r.row = Row::Queued { verify: None };
                    }
                    r.budget = 0;
                    r.sightings = 0;
                    r.rerun_note.clone_from(note);
                    self.notes.push(Note {
                        spec: row.spec.clone(),
                        platform: row.platform,
                        text: format!("queued again ({was}): {reason}"),
                    });
                }
                self.finished = false;
            }
            Event::Interrupted { token } => {
                let Some(key) = self.running_token(token) else {
                    return;
                };
                let r = self.rows.get_mut(&key).unwrap();
                if let Row::Running { verify, .. } = r.row.clone() {
                    r.budget = r.budget.saturating_sub(1);
                    r.row = Row::Queued { verify };
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
            Event::EvidenceAdded { problem, file, .. } => {
                if let Some(p) = self.problems.iter_mut().find(|p| p.id == *problem) {
                    p.evidence.push(file.clone());
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
                if !reruns.is_empty() {
                    self.finished = false;
                }
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
            Event::EvidenceCaptured {
                spec,
                platform,
                attempt,
                files,
            } => {
                if let Some(r) = self.rows.get_mut(&(spec.clone(), *platform))
                    && r.attempts == *attempt
                {
                    r.captured.extend(files.iter().cloned());
                }
            }
            Event::DeviceHealth {
                spec,
                platform,
                attempt,
                degraded,
                ..
            } => {
                if let Some(r) = self.rows.get_mut(&(spec.clone(), *platform))
                    && r.attempts == *attempt
                    && !degraded.is_empty()
                {
                    r.degraded = degraded.clone();
                    self.notes.push(Note {
                        spec: spec.clone(),
                        platform: *platform,
                        text: format!(
                            "device degraded during attempt {attempt}: {}",
                            degraded.join("; ")
                        ),
                    });
                }
            }
            Event::DeviceRestarted {
                spec,
                platform,
                reason,
            } => self.notes.push(Note {
                spec: spec.clone(),
                platform: *platform,
                text: format!("device restarted after this spec: {reason}"),
            }),
            Event::HostBusy {
                spec,
                platform,
                reason,
                waited_ms,
                settled,
            } => self.notes.push(Note {
                spec: spec.clone(),
                platform: *platform,
                text: format!(
                    "held {} s before this spec ({reason}){}",
                    waited_ms / 1000,
                    if *settled { "" } else { "; ran anyway" }
                ),
            }),
            // A record for the timeline; nothing about the rows changes.
            Event::Preflight { .. } => {}
            Event::Paused { .. } => self.paused = true,
            Event::Resumed => self.paused = false,
            Event::Finished { .. } => self.finished = true,
            // The rows themselves come from the plan, which already has the platforms and specs.
            Event::PlatformsAdded { .. } | Event::SpecsAdded { .. } => self.finished = false,
        }
    }
}

fn blank(text: &Option<String>) -> bool {
    text.as_deref().is_none_or(|t| t.trim().is_empty())
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
