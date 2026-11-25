//! The server as the run's orchestrator (SA-1049, SA-1050): it starts the next row on every free platform and
//! launches its tester, and launches a reviewer for every row with a claim or side finds to settle. A clean pass is
//! filed and the next row started without waking anyone. The server never makes a judgment call: anything that
//! needs a decision goes to a reviewer, or, when no reviewer settles it, to the coordinator.

use super::agents::{self, Ended, Launch};
use super::{Dispatched, ENDED_WITHOUT_POSTING, Hub, READY_POLL_MS};
use crate::report;
use crate::run::model::{Answer, Claim};
use crate::run::{Row, Run};
use crate::suite::Suite;
use crate::suite::config::Platform;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Reviewers running at once: each is a Claude process beside the devices on the same Mac.
const MAX_REVIEWERS: usize = 2;
/// How many reviewers a claim gets before it's left for the coordinator.
const REVIEW_TRIES: u32 = 2;
/// How a pause the server made itself starts, which is what tells `soundcheck attention` it's news to the coordinator.
pub const PAUSED_BY_SERVER: &str = "soundcheck paused the run";
/// How a dispatch's refusal starts when the build installed on its platform can't be identified.
pub const BUILD_UNKNOWN: &str = "the installed build can't be identified";
/// How long a reviewer may take before it's stopped.
const REVIEW_LIMIT: Duration = Duration::from_secs(10 * 60);

/// Starts orchestrating the run: called once its checks have passed. Platforms taking turns share one lane, which
/// keeps the run's order; platforms in parallel get a lane each, so one's reset never holds up another's.
pub fn start(hub: &Arc<Hub>) {
    let parallel = hub
        .with_run(|run, _| Ok(run.plan().parallel))
        .unwrap_or(false);
    let lanes: Vec<Option<Platform>> = if parallel {
        hub.with_run(|run, _| Ok(run.platforms()))
            .unwrap_or_default()
            .into_iter()
            .map(Some)
            .collect()
    } else {
        vec![None]
    };
    for platform in lanes {
        tokio::spawn(lane(hub.clone(), platform));
    }

    tokio::spawn(reviews(hub.clone()));
}

/// Waits for something to have changed: a journal entry, a nudge, or the poll that outlasts a hold. False once the
/// server is stopping.
async fn changed(
    hub: &Hub,
    events: &mut tokio::sync::broadcast::Receiver<crate::run::journal::Entry>,
    on_events: bool,
) -> bool {
    tokio::select! {
        _ = hub.stop.cancelled() => return false,
        _ = hub.wake.notified() => {}
        _ = events.recv(), if on_events => {}
        _ = tokio::time::sleep(Duration::from_millis(READY_POLL_MS)) => {}
    }

    while events.try_recv().is_ok() {}

    true
}

/// One dispatch lane: starts the next row whenever one can start, and launches its tester.
async fn lane(hub: Arc<Hub>, platform: Option<Platform>) {
    let mut events = hub.events.subscribe();
    loop {
        if hub.stop.is_cancelled() {
            return;
        }

        // A hold (a busy Mac, a restart, an install) is asked about again at the poll, not on every journal entry:
        // asking samples the host, which takes seconds.
        let refused = match hub.dispatch(None, platform).await {
            Ok(dispatched) => {
                tokio::spawn(tester(hub.clone(), dispatched));
                continue;
            }
            Err(e) => format!("{e:#}"),
        };

        // A build nobody can identify refuses every dispatch until a person acts (a commit landed mid-run, say).
        // A coordinator used to read that refusal; now the run pauses with it, which is what tells one.
        if refused.starts_with(BUILD_UNKNOWN) {
            let _ = hub.with_run(|run, now| {
                if run.state().paused || run.state().finished {
                    return Ok(());
                }

                run.pause(
                    now,
                    &format!("{PAUSED_BY_SERVER}: {refused}; resume once that's fixed"),
                )
            });
        }

        let held = refused.starts_with("held:");

        if !changed(&hub, &mut events, !held).await {
            return;
        }
    }
}

/// Runs an attempt's tester to its end, then settles what it left: its tokens, and its attempt if it never posted.
async fn tester(hub: Arc<Hub>, dispatched: Dispatched) {
    let token = dispatched.token.clone();
    let settled = || {
        hub.with_run(|run, _| Ok(run.running(&token).is_err()))
            .unwrap_or(true)
    };
    let ended = hub
        .launch(
            Launch {
                agent: &dispatched.agent,
                model: dispatched.model.as_deref(),
                prompt: &dispatched.prompt,
                name: &token,
                cwd: &hub.run_dir,
            },
            settled,
        )
        .await;
    hub.tester_done(&token, ended);
}

impl Hub {
    /// A tester's process has ended. Its tokens are journalled. An attempt still running never posted: one whose
    /// tester never made a call goes back to the queue without using an attempt, since the harness failed it, and
    /// any other counts as a stall. A tester that never made a call, or ended in error without posting (Claude not
    /// found, a usage limit, no connection to this server), pauses the run, so the rows after it aren't thrown at
    /// the same wall. One this server stopped as it shut down is left as it is: opening the run again puts its spec
    /// back first without using an attempt.
    pub fn tester_done(&self, token: &str, ended: Ended) {
        let stopping = self.stop.is_cancelled();
        let _ = self.with_run(|run, now| {
            // What it used is kept even as the server stops: it was spent either way.
            if let Some((tokens, spent)) = ended.usage {
                run.tokens(now, token, tokens, Some(spent))?;

                // The last tester's tokens land after its result has finished the run and exported it.
                if run.state().finished {
                    let _ = self.export(run, now);
                }
            }

            if stopping || run.state().finished || run.running(token).is_err() {
                return Ok(());
            }

            let never_called = !run.started(token);
            match &ended.error {
                _ if never_called => run.interrupt(now, token)?,
                Some(error) => {
                    run.stall(now, token, &format!("{ENDED_WITHOUT_POSTING}: {error}"))?
                }
                None => run.stall(now, token, ENDED_WITHOUT_POSTING)?,
            }

            let why = match &ended.error {
                Some(error) => Some(error.clone()),
                None if never_called => Some("it ended without making a single call".to_string()),
                None => None,
            };

            if let Some(why) = why
                && !run.state().paused
            {
                run.pause(
                    now,
                    &format!("{PAUSED_BY_SERVER}: a tester couldn't run ({why}); resume once that's fixed"),
                )?;
            }

            Ok(())
        });
        self.wake.notify_waiters();
    }
}

/// A row with something for a reviewer: its claim, its side finds, or both.
struct ReviewJob {
    spec: String,
    platform: Platform,
    /// Names the row's review across tries: the row and its latest attempt, whether it's the claim, the finds or
    /// both that wait, so finds a reviewer left behind draw no reviewers of their own.
    key: String,
    claim: bool,
    finds: Vec<u32>,
}

#[derive(Default)]
struct Reviews {
    /// Rows with a reviewer running. By row, not by claim: a reviewer settles its row's claim and side finds one
    /// after the other, and the row's remaining find mustn't draw a second reviewer meanwhile.
    active: Mutex<HashSet<(String, Platform)>>,
    tries: Mutex<HashMap<String, u32>>,
}

/// Launches a reviewer for each row with a claim or side finds waiting, a few at a time.
async fn reviews(hub: Arc<Hub>) {
    let mut events = hub.events.subscribe();
    let state = Arc::new(Reviews::default());
    loop {
        if hub.stop.is_cancelled() {
            return;
        }

        let jobs = hub
            .with_run(|run, _| Ok(review_jobs(run)))
            .unwrap_or_default();
        for job in jobs {
            {
                let mut active = state.active.lock().unwrap_or_else(|e| e.into_inner());
                let tried = state
                    .tries
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&job.key)
                    .copied()
                    .unwrap_or(0);

                if active.len() >= MAX_REVIEWERS {
                    break;
                }

                if tried >= REVIEW_TRIES || !active.insert((job.spec.clone(), job.platform)) {
                    continue;
                }
            }

            tokio::spawn(reviewer(hub.clone(), state.clone(), job));
        }

        if !changed(&hub, &mut events, true).await {
            return;
        }
    }
}

/// Whether the row still has what the job was launched for.
fn unsettled(run: &Run, job: &ReviewJob) -> bool {
    let claim = job.claim
        && run.row(&job.spec, job.platform).is_some_and(|r| {
            matches!(r.row, Row::NeedsReview { .. })
                && job.key == review_key(&job.spec, job.platform, r.attempts)
        });
    claim
        || run
            .state()
            .pending_finds
            .iter()
            .any(|f| job.finds.contains(&f.id))
}

fn review_key(spec: &str, platform: Platform, attempt: u32) -> String {
    format!("{spec}#{attempt}@{platform}")
}

async fn reviewer(hub: Arc<Hub>, state: Arc<Reviews>, job: ReviewJob) {
    let started = std::time::Instant::now();
    let settled = || {
        started.elapsed() >= REVIEW_LIMIT
            || hub
                .with_run(|run, _| Ok(!unsettled(run, &job)))
                .unwrap_or(true)
    };
    let suite = hub.suite();
    let model = suite.config.as_ref().map(|c| c.models.reviewer.clone());
    let prompt = hub
        .with_run(|run, _| Ok(review_brief(run, &suite, &hub, &job.spec, job.platform)))
        .unwrap_or_default();
    let ended = hub
        .launch(
            Launch {
                agent: agents::REVIEWER,
                model: model.as_deref(),
                prompt: &prompt,
                name: &format!("review-{}-{}", job.key, started_stamp()),
                cwd: &hub.project_dir(),
            },
            settled,
        )
        .await;

    // A reviewer this server stopped as it shut down didn't fail: the next server launches one afresh.
    if hub.stop.is_cancelled() {
        return;
    }

    let tried = {
        let mut tries = state.tries.lock().unwrap_or_else(|e| e.into_inner());
        let tried = tries.entry(job.key.clone()).or_insert(0);
        *tried += 1;
        *tried
    };
    let _ = hub.with_run(|run, now| {
        // Only the last try's failure is the coordinator's business; an earlier one is tried again.
        let left = (unsettled(run, &job) && tried >= REVIEW_TRIES).then(|| {
            ended
                .error
                .clone()
                .unwrap_or_else(|| format!("{tried} reviewers ended without settling it"))
        });
        let ran = started.elapsed().as_millis() as u64;
        run.reviewer(now, &job.spec, job.platform, ended.usage, ran, left)
    });
    state
        .active
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(job.spec.clone(), job.platform));
    hub.wake.notify_waiters();
}

/// Tells two reviewers of one claim apart in the run folder.
fn started_stamp() -> u64 {
    super::now_ms()
}

fn review_jobs(run: &Run) -> Vec<ReviewJob> {
    let state = run.state();
    let mut rows: Vec<(String, Platform)> = state
        .rows
        .iter()
        .filter(|(_, r)| matches!(r.row, Row::NeedsReview { .. }))
        .map(|((spec, platform), _)| (spec.clone(), *platform))
        .collect();
    for find in &state.pending_finds {
        let row = (find.spec.clone(), find.platform);

        if !rows.contains(&row) {
            rows.push(row);
        }
    }

    rows.into_iter()
        .filter_map(|(spec, platform)| {
            let record = run.row(&spec, platform)?;
            let finds: Vec<u32> = state
                .pending_finds
                .iter()
                .filter(|f| f.spec == spec && f.platform == platform)
                .map(|f| f.id)
                .collect();
            let claim = matches!(record.row, Row::NeedsReview { .. });
            let key = review_key(&spec, platform, record.attempts);
            Some(ReviewJob {
                spec,
                platform,
                key,
                claim,
                finds,
            })
        })
        .collect()
}

/// Everything a reviewer needs for one row, so it settles it without asking for more: what the run is testing, the
/// spec, the claim with each judgment and its evidence, and the row's side finds.
pub fn review_brief(
    run: &Run,
    suite: &Suite,
    hub: &Hub,
    spec_id: &str,
    platform: Platform,
) -> String {
    let evidence = hub.run_dir.join(report::EVIDENCE_DIR);
    let file = |name: &String| evidence.join(name).display().to_string();
    let files = |names: &[String]| names.iter().map(file).collect::<Vec<_>>().join(", ");
    let mut out = String::new();
    let _ = writeln!(out, "# Review {spec_id} on {platform}\n");

    if let Some(about) = hub
        .about
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        let _ = writeln!(out, "## What this run is testing\n\n{about}\n");
    }

    let spec = suite
        .specs
        .iter()
        .find(|s| s.id == spec_id)
        .map(|s| s.for_platform(platform));
    let record = run.row(spec_id, platform);
    let judged_against = record
        .map(|r| r.judged_against.as_slice())
        .unwrap_or_default();
    let outcomes: Vec<String> = if judged_against.is_empty() {
        spec.as_ref()
            .map(|s| s.outcomes.clone())
            .unwrap_or_default()
    } else {
        judged_against.to_vec()
    };

    if let Some(spec) = &spec {
        let _ = writeln!(
            out,
            "## The spec: {}\n\n{}\n",
            spec.front.title,
            spec.goal.trim()
        );

        if let Some(probe) = &spec.probe {
            let _ = writeln!(out, "Probe around it: {}\n", probe.trim());
        }

        let context = suite
            .features
            .iter()
            .find(|f| f.name == spec.feature)
            .and_then(|f| f.context.as_deref())
            .map(str::trim)
            .filter(|c| !c.is_empty());

        if let Some(context) = context {
            let _ = writeln!(out, "About this feature: {context}\n");
        }
    }

    if let Some(record) = record
        && let Row::NeedsReview {
            attempt,
            claim,
            posted,
            verify,
            ..
        } = &record.row
    {
        let untried =
            *claim == Claim::Unsure && !posted.judgments.iter().any(|j| j.answer == Answer::Unsure);
        let claimed = match claim {
            Claim::Fail => "FAIL",
            Claim::Blocked => "BLOCKED",
            Claim::Unsure if untried => "PASS, with its probes untried",
            Claim::Unsure => "Unsure",
            Claim::Pass => "PASS",
        };
        let _ = writeln!(
            out,
            "## The claim: {claimed}\n\nAttempt {attempt}; the row has used {} of its {} attempts on this build.",
            record.budget,
            crate::run::MAX_ATTEMPTS
        );

        if let Some(verify) = verify {
            let _ = writeln!(
                out,
                "This attempt was a verification, sent to reproduce: {verify}"
            );
        }

        if !record.degraded.is_empty() {
            let _ = writeln!(
                out,
                "The device was degraded during this attempt ({}), so a failure seen on it can't be upheld until it reproduces.",
                record.degraded.join("; ")
            );
        }

        let _ = writeln!(out, "\nThe tester's judgments:\n");
        for j in &posted.judgments {
            let answer = serde_json::to_value(j.answer)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "{}. {}\n   - {answer}: {}",
                j.outcome,
                outcomes.get(j.outcome).map_or("", String::as_str),
                j.observed.trim()
            );

            if !j.evidence.is_empty() {
                let _ = writeln!(out, "   - screenshots: {}", files(&j.evidence));
            }
        }

        if let Some(notes) = &posted.notes {
            let _ = writeln!(out, "\nThe tester's notes: {}", notes.trim());
        }

        if let Some(probed) = &posted.probed {
            let _ = writeln!(out, "\nWhat it probed: {}", probed.trim());
        }

        if !record.captured.is_empty() {
            let _ = writeln!(
                out,
                "\nThe server's own capture when the result was posted (screenshot, element tree, errors): {}",
                files(&record.captured)
            );
        }

        let _ = writeln!(
            out,
            "\nRecord your call with `review`, `spec` `{spec_id}` and `platform` `{platform}`.\n"
        );
    }

    let finds: Vec<_> = run
        .state()
        .pending_finds
        .iter()
        .filter(|f| f.spec == spec_id && f.platform == platform)
        .collect();

    if !finds.is_empty() {
        let _ = writeln!(out, "## Side finds\n");
        for f in finds {
            let _ = writeln!(
                out,
                "- id {}: {}\n  - observed: {}",
                f.id,
                f.find.summary.trim(),
                f.find.observed.trim()
            );

            if !f.find.evidence.is_empty() {
                let _ = writeln!(out, "  - screenshots: {}", files(&f.find.evidence));
            }
        }
        let _ = writeln!(
            out,
            "\nRecord a call on each with `review_find` and its `id`."
        );
    }

    out
}
