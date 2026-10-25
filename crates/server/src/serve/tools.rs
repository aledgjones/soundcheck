use super::{Hub, prompt};
use crate::report;
use crate::run::model::{Answer, Build, Call, Claim, Decision, Judgment, Posted, Rerun, SideFind};
use crate::run::{Row, verdict};
use crate::suite::config::Platform;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use soundcheck_driver::any::AnyDevice;
use soundcheck_driver::{Device, Driver, DriverError, Element, Matcher};
use std::sync::Arc;

pub const INSTRUCTIONS: &str = "Soundcheck runs a regression suite. Testers call brief with their spec token first and post_result at the end; the coordinator uses status, dispatch, queue, review, review_find, install_build, triage, pause, resume and finish.";

#[derive(Clone)]
pub struct Tools {
    hub: Arc<Hub>,
    tool_router: ToolRouter<Self>,
}

/// A refusal is a normal tool result the agent reads and acts on, not a protocol error.
fn reply(result: anyhow::Result<serde_json::Value>) -> Result<CallToolResult, ErrorData> {
    Ok(match result {
        Ok(value) => CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&value).unwrap_or_default(),
        )]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("refused: {e:#}"))]),
    })
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PostResultParams {
    /// Your spec token, from your brief.
    pub token: String,
    /// One judgment per outcome by its number in your brief, for those you haven't already recorded with `judge`.
    pub judgments: Vec<Judgment>,
    /// Problems found while probing that the outcomes don't cover.
    #[serde(default)]
    pub side_finds: Vec<SideFind>,
    pub notes: Option<String>,
    /// When your brief has `Probe around it`: what you tried and what happened. A pass without it waits for review.
    pub probed: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct JudgeParams {
    /// Your spec token, from your brief.
    pub token: String,
    /// The outcome you've settled, as one judgment: its number in your brief, your answer, what you saw.
    pub judgment: Judgment,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DispatchParams {
    /// Spec ID, e.g. `annotations/001-draw`.
    pub spec: String,
    pub platform: Platform,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReviewParams {
    pub spec: String,
    pub platform: Platform,
    pub call: Call,
    /// One line: why. It's shown on the row.
    pub reason: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReviewFindParams {
    pub id: u32,
    /// A title in plain words makes it a finding; none means it isn't a problem.
    pub title: Option<String>,
    /// The find's screenshots (from its `evidence`) on its topic, which help the person understand it; leave out
    /// any of something else. Empty when none is.
    pub keep: Vec<String>,
    pub reason: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct AddEvidenceParams {
    pub problem: u32,
    /// The local PNG to add, e.g. one `soundcheck drive <platform> shot` saved.
    pub path: String,
    /// A short name for it, e.g. `slider-102`.
    pub name: String,
    /// How it was taken and whether it confirms or contradicts the claim, in one line.
    pub reason: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TriageParams {
    pub problem: u32,
    pub decision: Decision,
    pub reason: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct InstallBuildParams {
    pub platform: Platform,
    /// The build installed; leave it out to have the project's build-identity hook read it.
    pub build: Option<Build>,
    /// Build and install the current tree with the project's install hook first (takes minutes), when the person
    /// asks you to install their fix rather than having installed it themselves.
    #[serde(default)]
    pub install: bool,
    /// Rows you judge the change may affect, each with a reason. Native changes and open problems rerun anyway.
    #[serde(default)]
    pub reruns: Vec<Rerun>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReasonParams {
    pub reason: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct QueueParams {
    /// Spec IDs or features, as `--scope` takes them, e.g. `annotations/003-eraser` or `viewer`.
    pub specs: Vec<String>,
    /// For specs already in the run, the platforms to run them on again (default: all of theirs).
    pub platforms: Option<Vec<Platform>>,
    /// Why specs already in the run run again; required for them.
    pub reason: Option<String>,
    /// Something the next attempt's tester should know, e.g. a harness fix to use.
    pub note: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TokenParams {
    /// Your spec token.
    pub token: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TargetParams {
    pub token: String,
    /// An element's label, value or `see` line; exact unless it starts with `prefix:` or `re:`. Last resort: `at:<x>,<y>` taps a point (points on iOS, pixels on Android), only for a control `see` can't name at all.
    pub target: String,
    /// Take the last match, e.g. a dialog's button that shares its label with the row behind it.
    #[serde(default)]
    pub last: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SeeParams {
    pub token: String,
    /// Only elements matching this (exact, `prefix:` or `re:`).
    pub filter: Option<String>,
    /// Include elements a screen reader skips.
    #[serde(default)]
    pub all: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CheckParams {
    pub token: String,
    /// Each must be on screen; prefix one with `!` for "must not be".
    pub assertions: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WaitParams {
    pub token: String,
    pub target: String,
    /// Milliseconds, at most 120000.
    pub timeout_ms: u64,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SleepParams {
    pub token: String,
    /// Milliseconds, at most 30000.
    pub ms: u64,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ShotParams {
    pub token: String,
    /// A short name for the screenshot, e.g. `undo-after`; the reply gives the saved file name to attach as evidence.
    pub name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct FillParams {
    pub token: String,
    pub target: String,
    pub text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TextParams {
    pub token: String,
    pub text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ClearParams {
    pub token: String,
    /// The most characters the field might hold (default 60).
    pub chars: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GestureParams {
    pub token: String,
    pub from: [f64; 2],
    pub to: [f64; 2],
    /// The swipe's duration in milliseconds.
    pub ms: u64,
}

/// A place one touch passes through.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct TouchStop {
    /// An element, as `tap` takes it, or a point, `at:<x>,<y>`.
    pub at: String,
    /// Milliseconds the finger rests here; on the first stop, how long it presses before moving.
    #[serde(default)]
    pub pause_ms: u64,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TouchParams {
    pub token: String,
    /// The stops in order, 1 to 20: down at the first, moving between them at a finger's pace, up at the last.
    pub path: Vec<TouchStop>,
    /// Stay down at the last stop, so you can look mid-gesture, until `release` (or other input, or 30 s).
    #[serde(default)]
    pub keep: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReleaseParams {
    pub token: String,
    /// Move back to the first stop before lifting, so nothing is dropped or moved.
    #[serde(default)]
    pub back: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct HoldParams {
    pub token: String,
    pub target: String,
    pub ms: u64,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GotoParams {
    pub token: String,
    /// A URL, or `back` or `forward`.
    pub url: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ViewportParams {
    pub token: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SecondsParams {
    pub token: String,
    pub seconds: u64,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct FaultParams {
    pub token: String,
    /// The request path to fail, starting with /; `*` matches any one segment. The query string doesn't count.
    pub path: Option<String>,
    /// The HTTP status the service answers with, 400–599.
    pub status: Option<u16>,
    /// true clears every fault, with no path or status.
    #[serde(default)]
    pub clear: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct OfflineParams {
    pub token: String,
    /// true takes the app offline; false brings it back online.
    pub on: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PushParams {
    pub token: String,
    /// A file in the suite's fixtures.
    pub file: String,
    /// The name it gets on the device, if different (names often carry meaning, e.g. a part named from its file).
    #[serde(rename = "as")]
    pub as_name: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CheckDataParams {
    pub token: String,
    /// Which check, as the suite names it, e.g. `parts`.
    pub name: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PreviewCompareParams {
    pub token: String,
    /// A name for this placement; its screenshots are saved as evidence under it.
    pub name: String,
    /// Where the touch starts (a symbol or a dot), in screen coordinates.
    pub from: Option<[f64; 2]>,
    /// Where a line or stroke ends; keep it level with `from` so the magnifier stays out.
    pub to: Option<[f64; 2]>,
    /// Instead of a held touch: the control that saves what's on screen (a text box's `Done`).
    pub commit: Option<String>,
    /// Only pixels near this colour count, e.g. `#FF3B30` for red text on a tinted box.
    pub colour: Option<String>,
    /// The area to compare, `[x, y, width, height]` in screen coordinates (needed with `commit`).
    pub region: Option<[f64; 4]>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BatchParams {
    pub token: String,
    /// Steps in order; the batch stops at the first that fails.
    pub steps: Vec<Step>,
}

/// One batch step: the same tools, with the same arguments, minus the token.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum Step {
    Tap {
        target: String,
        #[serde(default)]
        last: bool,
    },
    Fill {
        target: String,
        text: String,
    },
    Type {
        text: String,
    },
    Clear {
        chars: Option<usize>,
    },
    Expect {
        target: String,
    },
    Gone {
        target: String,
    },
    Wait {
        target: String,
        timeout_ms: u64,
    },
    Check {
        assertions: Vec<String>,
    },
    Hold {
        target: String,
        ms: u64,
    },
    Swipe {
        from: [f64; 2],
        to: [f64; 2],
        ms: u64,
    },
    Touch {
        path: Vec<TouchStop>,
        #[serde(default)]
        keep: bool,
    },
    Release {
        #[serde(default)]
        back: bool,
    },
    Hover {
        target: String,
    },
    Goto {
        url: String,
    },
    Press {
        key: String,
    },
    /// Wait without doing anything, so a sequence's timing is the device's, not the agent's.
    Pause {
        ms: u64,
    },
}

/// The least time between one input ending and the next starting in a batch: a person sees the screen respond before
/// acting again. Faster sequences found only races no finger reaches, such as a tap 2 ms into a drawer's close.
pub const INPUT_GAP: std::time::Duration = std::time::Duration::from_millis(500);

impl Step {
    /// Whether it touches or types, rather than only reading the screen or waiting.
    pub fn is_input(&self) -> bool {
        !matches!(
            self,
            Step::Expect { .. }
                | Step::Gone { .. }
                | Step::Wait { .. }
                | Step::Check { .. }
                | Step::Pause { .. }
        )
    }
}

pub async fn step(d: &Driver<AnyDevice>, step: &Step) -> Result<String, DriverError> {
    Ok(match step {
        Step::Tap { target, last } => {
            format!("tapped {}", line(&d.tap(&matcher(target)?, *last).await?))
        }
        Step::Fill { target, text } => {
            d.fill(&matcher(target)?, text).await?;
            format!("filled {target}")
        }
        Step::Type { text } => {
            d.type_checked(text).await?;
            "typed".into()
        }
        Step::Clear { chars } => {
            d.device.clear(chars.unwrap_or(60)).await?;
            "cleared".into()
        }
        Step::Expect { target } => format!("found {}", line(&d.expect(&matcher(target)?).await?)),
        Step::Gone { target } => {
            d.gone(&matcher(target)?).await?;
            format!("gone {target}")
        }
        Step::Wait { target, timeout_ms } => {
            format!(
                "found {}",
                line(&d.wait(&matcher(target)?, *timeout_ms).await?)
            )
        }
        Step::Check { assertions } => {
            let lines = d.check(assertions).await?;
            if let Some(bad) = lines.iter().find(|l| !l.ok) {
                return Err(DriverError::Invalid(format!(
                    "check failed: {}",
                    bad.assertion
                )));
            }
            "checked".into()
        }
        Step::Hold { target, ms } => {
            format!("held {}", line(&d.hold(&matcher(target)?, *ms).await?))
        }
        Step::Swipe { from, to, ms } => {
            d.swipe((from[0], from[1]), (to[0], to[1]), *ms).await?;
            "swiped".into()
        }
        Step::Touch { path, keep } => {
            let stops = path
                .iter()
                .map(|s| Ok((matcher(&s.at)?, s.pause_ms)))
                .collect::<Result<Vec<_>, DriverError>>()?;
            let through: Vec<String> = d.touch(&stops, *keep).await?.iter().map(line).collect();
            let end = if *keep {
                "held down until `release`"
            } else {
                "lifted"
            };
            format!("touched {}; {end}", through.join(" → "))
        }
        Step::Release { back } => {
            d.release(*back).await?;
            if *back {
                "moved back and released"
            } else {
                "released"
            }
            .into()
        }
        Step::Hover { target } => {
            let e = d.expect(&matcher(target)?).await?;
            let (x, y) = e.tap_point();
            d.device.hover_at(x, y).await?;
            format!("hovered {}", line(&e))
        }
        Step::Press { key } => {
            d.device.press(key).await?;
            format!("pressed {key}")
        }
        Step::Pause { ms } => {
            pause(*ms).await?;
            format!("paused {ms} ms")
        }
        Step::Goto { url } => {
            d.device.goto(url).await?;
            format!("opened {url}")
        }
    })
}

/// A plain timed pause, capped so a seconds value passed as milliseconds can't hang a spec.
async fn pause(ms: u64) -> Result<(), DriverError> {
    if ms > MAX_PAUSE_MS {
        return Err(DriverError::Invalid(format!(
            "takes milliseconds: {ms} is over {MAX_PAUSE_MS}"
        )));
    }
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    Ok(())
}

const MAX_PAUSE_MS: u64 = 30_000;

fn matcher(target: &str) -> Result<Matcher, DriverError> {
    Matcher::parse(target).map_err(|e| DriverError::Invalid(format!("{target}: {e}")))
}

pub fn line(e: &Element) -> String {
    let (x, y) = e.tap_point();
    format!("{x:.0},{y:.0}\t{}", e.line())
}

#[derive(Serialize)]
struct RowStatus<'a> {
    spec: &'a str,
    platform: Platform,
    state: &'static str,
    attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<crate::run::model::Final>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claim: Option<crate::run::model::Claim>,
    /// Why the device was degraded during the latest attempt.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    degraded: &'a [String],
    /// For a claim waiting for review: everything the call rests on.
    #[serde(skip_serializing_if = "Option::is_none")]
    review: Option<serde_json::Value>,
}

#[tool_router(vis = "pub")]
impl Tools {
    pub fn new(hub: Arc<Hub>) -> Self {
        Tools {
            hub,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Tester: record one outcome as soon as you've settled it, so the run shows your progress. Judging it again replaces it. `post_result` takes up what you've judged, so you needn't repeat it there."
    )]
    async fn judge(
        &self,
        Parameters(mut p): Parameters<JudgeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (spec, platform, attempt) = match self.attempt_of(&p.token) {
            Ok(row) => row,
            Err(e) => return reply(Err(e)),
        };
        let dir = self.hub.run_dir.join(report::EVIDENCE_DIR);
        let base = report::evidence_base(&spec, attempt, platform);
        let mut missing = Vec::new();
        for name in &mut p.judgment.evidence {
            match report::resolve_evidence(&dir, &base, name) {
                Some(file) => *name = file,
                None => missing.push(name.clone()),
            }
        }
        if !missing.is_empty() {
            return reply(Err(anyhow::anyhow!(
                "no saved evidence named {}: attach the names `shot` returned",
                missing.join(", ")
            )));
        }
        reply(self.hub.with_run(|run, now| {
            let judged = run.judge(now, &p.token, p.judgment)?;
            Ok(json!({ "judged": judged }))
        }))
    }

    #[tool(
        description = "Tester: post your spec's result once, at the end, judging every outcome you haven't recorded with `judge`. A PASS files at once; anything else goes to the coordinator for review."
    )]
    async fn post_result(
        &self,
        Parameters(p): Parameters<PostResultParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (spec, platform, attempt) = match self.attempt_of(&p.token) {
            Ok(row) => row,
            Err(e) => return reply(Err(e)),
        };
        let mut posted = Posted {
            judgments: p.judgments,
            side_finds: p.side_finds,
            notes: p.notes,
            probed: p.probed,
        };
        let dir = self.hub.run_dir.join(report::EVIDENCE_DIR);
        let base = report::evidence_base(&spec, attempt, platform);
        let mut missing = Vec::new();
        let lists = posted
            .judgments
            .iter_mut()
            .map(|j| &mut j.evidence)
            .chain(posted.side_finds.iter_mut().map(|f| &mut f.evidence));
        for list in lists {
            for name in list.iter_mut() {
                match report::resolve_evidence(&dir, &base, name) {
                    Some(file) => *name = file,
                    None => missing.push(name.clone()),
                }
            }
        }
        if !missing.is_empty() {
            return reply(Err(anyhow::anyhow!(
                "no saved evidence named {}: attach the names `shot` returned, or take the screenshot again",
                missing.join(", ")
            )));
        }
        let has_probes = self
            .hub
            .suite()
            .specs
            .iter()
            .any(|s| s.id == spec && s.probe.is_some());
        let claim = match self
            .hub
            .with_run(|run, now| run.post_probed(now, &p.token, posted, has_probes))
        {
            Ok(claim) => claim,
            Err(e) => return reply(Err(e)),
        };
        // Save the evidence now: the next spec's seed wipes the device, and review comes later.
        if claim != Claim::Pass {
            let files = self.hub.capture(&p.token, &spec, platform, attempt).await;
            let _ = self
                .hub
                .with_run(|run, now| run.evidence_captured(now, &spec, platform, attempt, files));
        } else {
            self.hub.forget_screen(&p.token);
        }
        // In the background: sampling takes seconds, and the next spec's reset waits for the device anyway.
        let hub = self.hub.clone();
        tokio::spawn(async move { hub.check_health(&spec, platform, attempt).await });
        reply(Ok(
            json!({ "claim": claim, "next": "you're done: reply in one line (token, claim, side finds) and stop" }),
        ))
    }

    #[tool(
        description = "Coordinator: the run at a glance: verdict and its reasons, every row's state, problems, side finds waiting, and what stops the run finishing."
    )]
    async fn status(&self) -> Result<CallToolResult, ErrorData> {
        let evidence_dir = self.hub.run_dir.join(report::EVIDENCE_DIR);
        reply(self.hub.with_run(|run, _| {
            let state = run.state();
            let rows: Vec<RowStatus> = state
                .rows
                .iter()
                .map(|((spec, platform), r)| {
                    let (label, result, claim) = match &r.row {
                        Row::NotRun => ("not run", None, None),
                        Row::Queued { .. } => ("queued", None, None),
                        Row::Running { .. } => ("running", None, None),
                        Row::NeedsReview { claim, .. } => ("needs review", None, Some(*claim)),
                        Row::Filed { result, .. } => ("filed", Some(*result), None),
                    };
                    let review = match &r.row {
                        Row::NeedsReview {
                            attempt,
                            posted,
                            verify,
                            ..
                        } => {
                            let outcomes = if r.judged_against.is_empty() {
                                run.plan()
                                    .specs
                                    .iter()
                                    .find(|s| s.id == *spec)
                                    .map(|s| s.outcomes.as_slice())
                                    .unwrap_or_default()
                            } else {
                                r.judged_against.as_slice()
                            };
                            let judgments: Vec<_> = posted
                                .judgments
                                .iter()
                                .map(|j| {
                                    json!({
                                        "outcome": j.outcome,
                                        "text": outcomes.get(j.outcome),
                                        "answer": j.answer,
                                        "observed": j.observed,
                                        "evidence": j.evidence,
                                    })
                                })
                                .collect();
                            let untried = claim == Some(Claim::Unsure)
                                && !posted.judgments.iter().any(|j| j.answer == Answer::Unsure);
                            Some(json!({
                                "attempt": attempt,
                                "verifying": verify,
                                "judgments": judgments,
                                "notes": posted.notes,
                                "probed": posted.probed,
                                "probes_untried": untried,
                            }))
                        }
                        _ => None,
                    };
                    RowStatus {
                        spec,
                        platform: *platform,
                        state: label,
                        attempts: r.attempts,
                        result,
                        claim,
                        degraded: &r.degraded,
                        review,
                    }
                })
                .collect();
            let finds: Vec<_> = state
                .pending_finds
                .iter()
                .map(|f| {
                    json!({
                        "id": f.id,
                        "summary": f.find.summary,
                        "spec": f.spec,
                        "platform": f.platform,
                        "observed": f.find.observed,
                        "evidence": f.find.evidence,
                    })
                })
                .collect();
            Ok(json!({
                "verdict": verdict::verdict(state),
                "paused": state.paused,
                "finished": state.finished,
                "rows": rows,
                "problems": state.problems,
                "side_finds_waiting": finds,
                "evidence_dir": evidence_dir,
                "notes": state.notes.iter().map(|n| format!("{} on {}: {}", n.spec, n.platform, n.text)).collect::<Vec<_>>(),
                "unclear_specs": state.unclear_specs,
                "finish_blockers": run.finish_blockers(),
            }))
        }))
    }

    #[tool(
        description = "Coordinator: start a spec's next attempt on a platform. Returns its token, model and a short prompt: pass the prompt unchanged to a background soundcheck-tester agent, which fetches its brief with `brief`."
    )]
    async fn dispatch(
        &self,
        Parameters(p): Parameters<DispatchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let hub = self.hub.clone();
        let suite = hub.suite();
        let Some(spec) = suite.specs.iter().find(|s| s.id == p.spec) else {
            return reply(Err(anyhow::anyhow!("no spec {} in the suite", p.spec)));
        };
        // Check first, so a refused dispatch never resets a device.
        if let Err(e) = hub.with_run(|run, _| run.check_dispatch(&p.spec, p.platform)) {
            return reply(Err(e));
        }
        if let Some(reason) = hub.hold(p.platform).await {
            return reply(Err(anyhow::anyhow!(
                "held: {reason}. Run `soundcheck ready {} {} --wait` in the background, and dispatch again when it exits",
                hub.run_dir.display(),
                p.platform
            )));
        }
        hub.end_wait(&p.spec, p.platform);
        let seeded = match hub.prepare(spec, p.platform).await {
            Ok(context) => context,
            Err(e) => {
                let reason = format!("{e:#}");
                let _ =
                    hub.with_run(|run, now| run.reset_failed(now, &p.spec, p.platform, &reason));
                return reply(Err(anyhow::anyhow!(
                    "the reset failed three times, so {} is filed BLOCKED on {}: {reason}",
                    p.spec,
                    p.platform
                )));
            }
        };
        let context = suite
            .features
            .iter()
            .find(|f| f.name == spec.feature)
            .and_then(|f| f.context.as_deref());
        reply(hub.with_run(|run, now| {
            let rerun = run
                .state()
                .rows
                .get(&(p.spec.clone(), p.platform))
                .and_then(|r| r.rerun_note.clone());
            let token = run.dispatch(now, &p.spec, p.platform, |token, verify| {
                prompt::render(
                    spec,
                    context,
                    p.platform,
                    token,
                    verify,
                    rerun.as_deref(),
                    seeded.as_deref(),
                )
            })?;
            Ok(json!({
                "token": token,
                "model": spec.front.model.clone().or_else(|| {
                    suite.config.as_ref().map(|c| c.models.default.clone())
                }),
                "prompt": prompt::handoff(&token),
            }))
        }))
    }

    #[tool(
        description = "Tester: your brief, the spec you're testing and how to post its result. Call it first, with the token you were given."
    )]
    async fn brief(
        &self,
        Parameters(p): Parameters<TokenParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let brief = self.hub.with_run(|run, now| {
            let brief = run.brief(&p.token)?;
            run.call_started(now, &p.token);
            run.call_finished(now, &p.token);
            Ok(brief)
        });
        Ok(match brief {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("refused: {e:#}"))]),
        })
    }

    #[tool(
        description = "Coordinator: your call on a result waiting for review: uphold, verify (a fresh attempt told the claim), send-back, judge (an Unsure only), spec-unclear, or harness (the harness stopped it: filed BLOCKED, or with rerun a fresh attempt told the note). Every call needs a one-line reason."
    )]
    async fn review(
        &self,
        Parameters(p): Parameters<ReviewParams>,
    ) -> Result<CallToolResult, ErrorData> {
        reply(self.hub.with_run(|run, now| {
            run.review(now, &p.spec, p.platform, p.call, &p.reason)?;
            Ok(json!({ "reviewed": p.spec, "platform": p.platform }))
        }))
    }

    #[tool(
        description = "Coordinator: your call on a side find: give it a title in plain words to make it a finding, or none if it isn't one. Read its screenshots first and keep only those on its topic, whether they confirm it or not."
    )]
    async fn review_find(
        &self,
        Parameters(p): Parameters<ReviewFindParams>,
    ) -> Result<CallToolResult, ErrorData> {
        reply(self.hub.with_run(|run, now| {
            run.review_find(now, p.id, p.title, p.keep, &p.reason)?;
            Ok(json!({ "reviewed_find": p.id }))
        }))
    }

    #[tool(
        description = "Coordinator: add a screenshot you took after review (reproducing a problem with `soundcheck drive`) to that problem's evidence, so the page shows it. A shot of where the problem should be, whether it confirms the claim or contradicts it; the reason says which."
    )]
    async fn add_evidence(
        &self,
        Parameters(p): Parameters<AddEvidenceParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let dir = self.hub.run_dir.join(report::EVIDENCE_DIR);
        reply(self.hub.with_run(|run, now| {
            let Some(problem) = run.state().problem(p.problem) else {
                anyhow::bail!("no problem {}", p.problem);
            };
            let base = report::evidence_base(&problem.spec, problem.attempt, problem.platform);
            let file = report::shot_file(&base, &p.name);
            if !report::safe_name(&file) {
                anyhow::bail!("{}: use letters, digits, - and _ only", p.name);
            }
            let source = std::path::Path::new(&p.path);
            if !p.path.ends_with(".png") || !source.is_file() {
                anyhow::bail!("{}: not a PNG file", p.path);
            }
            if dir.join(&file).exists() {
                anyhow::bail!("{file} is already saved: pick another name");
            }
            std::fs::create_dir_all(&dir)?;
            std::fs::copy(source, dir.join(&file))?;
            run.add_evidence(now, p.problem, &file, &p.reason)?;
            Ok(json!({ "problem": p.problem, "added": file }))
        }))
    }

    #[tool(
        description = "Coordinator: record a triage decision the person made: dismiss, fix (its row must have passed again), defer, or duplicate of another problem. Fix and defer take the ticket when the suite names a tracker; without one, the reason is the record."
    )]
    async fn triage(
        &self,
        Parameters(p): Parameters<TriageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let tracked = self
            .hub
            .suite()
            .config
            .as_ref()
            .is_some_and(|c| c.tickets.is_some());
        if tracked
            && matches!(
                p.decision,
                Decision::Fix { ticket: None } | Decision::Defer { ticket: None }
            )
        {
            return reply(Err(anyhow::anyhow!(
                "this suite tracks findings in a tracker (`tickets` in suite.yaml), so fixed and deferred problems need its ticket"
            )));
        }
        reply(self.hub.with_run(|run, now| {
            run.triage(now, p.problem, p.decision, &p.reason)?;
            Ok(json!({ "triaged": p.problem, "verdict": run.verdict() }))
        }))
    }

    #[tool(
        description = "Coordinator: a fix build is installed on a platform. Reruns every row on a native change, rows with open problems always, and the rows you list (agreed with the person); the rest keep their results. Only between specs."
    )]
    async fn install_build(
        &self,
        Parameters(p): Parameters<InstallBuildParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if p.install
            && let Err(e) = self.hub.install(p.platform).await
        {
            return reply(Err(e));
        }
        let build = match p.build {
            Some(build) => build,
            None => match self.hub.read_build(p.platform).await {
                Ok(build) => build,
                Err(e) => return reply(Err(e)),
            },
        };
        reply(self.hub.with_run(|run, now| {
            let reruns = run.install_build(now, p.platform, build.clone(), p.reruns)?;
            Ok(json!({ "build": build, "reruns": reruns }))
        }))
    }

    #[tool(
        description = "Coordinator: queue specs to run, whatever state they're in. Spec IDs or features: a spec not in the run joins it, read from the suite as it is now, with a row on every run platform it runs on. A spec in the run runs again on `platforms` (default: all of its): a filed result or a claim waiting for review is set aside, a running attempt abandoned, and it gets a fresh attempt budget; that needs a `reason`, shown in the report. `note` goes into the rerun's brief. A finished run opens again. Returns the rows added and requeued."
    )]
    async fn queue(
        &self,
        Parameters(p): Parameters<QueueParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let queued = self.hub.queue(
            &p.specs,
            p.platforms.as_deref(),
            p.reason.as_deref(),
            p.note,
        );
        reply(queued.map(|q| {
            let added: Vec<_> = q
                .added
                .into_iter()
                .map(|(spec, platform)| json!({ "spec": spec, "platform": platform }))
                .collect();
            let requeued: Vec<_> = q
                .requeued
                .into_iter()
                .map(|(r, was)| json!({ "spec": r.spec, "platform": r.platform, "was": was }))
                .collect();
            json!({ "added": added, "requeued": requeued })
        }))
    }

    #[tool(description = "Coordinator: pause the run; no spec is dispatched until resume.")]
    async fn pause(
        &self,
        Parameters(p): Parameters<ReasonParams>,
    ) -> Result<CallToolResult, ErrorData> {
        reply(self.hub.with_run(|run, now| {
            run.pause(now, &p.reason)?;
            Ok(json!({ "paused": true }))
        }))
    }

    #[tool(description = "Coordinator: resume a paused run.")]
    async fn resume(&self) -> Result<CallToolResult, ErrorData> {
        reply(self.hub.with_run(|run, now| {
            run.resume(now)?;
            Ok(json!({ "paused": false }))
        }))
    }

    #[tool(
        description = "Coordinator: finish the run once nothing is running, waiting for review or queued. Writes the final report. Triage still works afterwards; export again after it."
    )]
    async fn finish(&self) -> Result<CallToolResult, ErrorData> {
        let hub = self.hub.clone();
        reply(self.hub.with_run(|run, now| {
            let counts = run.finish(now)?;
            let report = hub.export(run, now)?;
            Ok(json!({ "results": counts, "verdict": run.verdict(), "report": report }))
        }))
    }

    #[tool(
        description = "Coordinator: write a self-contained copy of the report (screenshots embedded) into the run's folder, stamped in progress or final. Do it after triage too."
    )]
    async fn export(&self) -> Result<CallToolResult, ErrorData> {
        let hub = self.hub.clone();
        reply(self.hub.with_run(|run, now| {
            let report = hub.export(run, now)?;
            Ok(json!({ "report": report }))
        }))
    }

    #[tool(
        description = "Tester: what's on screen, as a screen reader gets it: one line per element, `x,y<TAB>label = value (states)`. `filter` narrows it; `all` includes what a screen reader skips."
    )]
    async fn see(&self, Parameters(p): Parameters<SeeParams>) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            let filter = p.filter.as_deref().map(matcher).transpose()?;
            let lines: Vec<String> = d
                .see(filter.as_ref(), p.all)
                .await?
                .iter()
                .map(line)
                .collect();
            if filter.is_none() {
                self.hub.remember_screen(&p.token, lines.clone());
            }
            Ok(lines)
        })
        .await
    }

    #[tool(description = "Tester: fails unless the element is on screen now.")]
    async fn expect(
        &self,
        Parameters(p): Parameters<TargetParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            Ok(line(&d.expect(&matcher(&p.target)?).await?))
        })
        .await
    }

    #[tool(description = "Tester: fails if the element is on screen now.")]
    async fn gone(
        &self,
        Parameters(p): Parameters<TargetParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            d.gone(&matcher(&p.target)?).await.map(|()| "gone")
        })
        .await
    }

    #[tool(
        description = "Tester: several assertions on one read of the screen: `A` must be on screen, `!A` must not."
    )]
    async fn check(
        &self,
        Parameters(p): Parameters<CheckParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            Ok(d.check(&p.assertions)
                .await?
                .into_iter()
                .map(|l| json!({ "assertion": l.assertion, "ok": l.ok }))
                .collect::<Vec<_>>())
        })
        .await
    }

    #[tool(description = "Tester: wait up to timeout_ms (milliseconds) for an element to appear.")]
    async fn wait(
        &self,
        Parameters(p): Parameters<WaitParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            Ok(line(&d.wait(&matcher(&p.target)?, p.timeout_ms).await?))
        })
        .await
    }

    #[tool(
        description = "Tester: do nothing for `ms` milliseconds (at most 30000), e.g. to let an animation or a sync settle before you look. To wait for something to appear, use wait."
    )]
    async fn sleep(
        &self,
        Parameters(p): Parameters<SleepParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let checked = self.hub.with_run(|run, now| {
            run.running(&p.token)?;
            run.call_started(now, &p.token);
            Ok(())
        });
        if let Err(e) = checked {
            return reply(Err(e));
        }
        let result = pause(p.ms).await;
        let _ = self.hub.with_run(|run, now| {
            run.call_finished(now, &p.token);
            Ok(())
        });
        reply(
            result
                .map(|()| json!(format!("slept {} ms", p.ms)))
                .map_err(anyhow::Error::from),
        )
    }

    #[tool(
        description = "Tester: save a screenshot as evidence; returns its file name to attach to a judgment and the path to Read it at. Only for visual checks."
    )]
    async fn shot(
        &self,
        Parameters(p): Parameters<ShotParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let dir = self.hub.run_dir.join(report::EVIDENCE_DIR);
        let (spec, platform, attempt) = match self.attempt_of(&p.token) {
            Ok(row) => row,
            Err(e) => return reply(Err(e)),
        };
        // One evidence folder serves every spec, platform and attempt, so a shot's name carries its row.
        let name = report::shot_file(&report::evidence_base(&spec, attempt, platform), &p.name);
        self.device(&p.token, async |d| {
            if !report::safe_name(&name) {
                return Err(DriverError::Invalid(format!(
                    "{}: use letters, digits, - and _ only",
                    p.name
                )));
            }
            std::fs::create_dir_all(&dir).map_err(|e| DriverError::Device(e.to_string()))?;
            let path = dir.join(&name);
            d.device.screenshot(&path).await?;
            Ok(format!(
                "saved as {name}: attach this name; Read it at {}",
                path.display()
            ))
        })
        .await
    }

    #[tool(description = "Tester: crashes and app errors since the last launch.")]
    async fn errors(
        &self,
        Parameters(p): Parameters<TokenParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| d.device.errors().await)
            .await
    }

    #[tool(description = "Tester: tap an element (waits up to 5 s for it).")]
    async fn tap(
        &self,
        Parameters(p): Parameters<TargetParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            Ok(line(&d.tap(&matcher(&p.target)?, p.last).await?))
        })
        .await
    }

    #[tool(
        description = "Tester: tap a field, empty it, type the text and check it landed (retries twice)."
    )]
    async fn fill(
        &self,
        Parameters(p): Parameters<FillParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            d.fill(&matcher(&p.target)?, &p.text)
                .await
                .map(|()| "filled")
        })
        .await
    }

    #[tool(
        name = "type",
        description = "Tester: type into the focused field (appends)."
    )]
    async fn r#type(
        &self,
        Parameters(p): Parameters<TextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            d.type_checked(&p.text).await.map(|()| "typed")
        })
        .await
    }

    #[tool(description = "Tester: empty the focused field.")]
    async fn clear(
        &self,
        Parameters(p): Parameters<ClearParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            d.device
                .clear(p.chars.unwrap_or(60))
                .await
                .map(|()| "cleared")
        })
        .await
    }

    #[tool(description = "Tester: swipe from one point to another over `ms` milliseconds.")]
    async fn swipe(
        &self,
        Parameters(p): Parameters<GestureParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            d.swipe((p.from[0], p.from[1]), (p.to[0], p.to[1]), p.ms)
                .await
                .map(|()| "swiped")
        })
        .await
    }

    #[tool(
        description = "Tester: one finger along a path of stops (elements or `at:x,y` points), resting `pause_ms` at each, at a finger's pace. A drag is two stops (a reorder handle: rest about 700 ms on the first); also slide off a control and back before lifting, stop a slider partway. With `keep` it stays down at the last stop until `release`, so you can `see` or `shot` mid-gesture."
    )]
    async fn touch(
        &self,
        Parameters(p): Parameters<TouchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let s = Step::Touch {
            path: p.path,
            keep: p.keep,
        };
        self.device(&p.token, async |d| step(d, &s).await).await
    }

    #[tool(
        description = "Tester: lift the touch a kept `touch` left down, where it is, or with `back` after moving back to its first stop."
    )]
    async fn release(
        &self,
        Parameters(p): Parameters<ReleaseParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let s = Step::Release { back: p.back };
        self.device(&p.token, async |d| step(d, &s).await).await
    }

    #[tool(description = "Tester: press and hold an element for `ms` milliseconds.")]
    async fn hold(
        &self,
        Parameters(p): Parameters<HoldParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            Ok(line(&d.hold(&matcher(&p.target)?, p.ms).await?))
        })
        .await
    }

    #[tool(
        description = "Tester: press a hardware key by its web name (ArrowRight, Enter, Escape…), as a keyboard or page-turn pedal would. `Home` goes to the device's home screen, putting the app in the background."
    )]
    async fn press(
        &self,
        Parameters(p): Parameters<TextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            d.device.press(&p.text).await.map(|()| "pressed")
        })
        .await
    }

    #[tool(description = "Tester: whether the on-screen keyboard is shown.")]
    async fn keyboard(
        &self,
        Parameters(p): Parameters<TokenParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| d.device.keyboard_shown().await)
            .await
    }

    #[tool(description = "Tester: restart the app.")]
    async fn launch(
        &self,
        Parameters(p): Parameters<TokenParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            d.device.launch().await.map(|()| "launched")
        })
        .await
    }

    #[tool(description = "Tester (web): hover over an element.")]
    async fn hover(
        &self,
        Parameters(p): Parameters<TargetParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            let e = d.expect(&matcher(&p.target)?).await?;
            let (x, y) = e.tap_point();
            d.device.hover_at(x, y).await?;
            Ok(line(&e))
        })
        .await
    }

    #[tool(description = "Tester (web): open a URL, or go `back` or `forward`.")]
    async fn goto(
        &self,
        Parameters(p): Parameters<GotoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            d.device.goto(&p.url).await.map(|()| "opened")
        })
        .await
    }

    #[tool(description = "Tester (web): resize the browser viewport.")]
    async fn viewport(
        &self,
        Parameters(p): Parameters<ViewportParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            d.device
                .viewport(p.width, p.height)
                .await
                .map(|()| "resized")
        })
        .await
    }

    #[tool(
        description = "Tester: copy a file from the suite's fixtures onto the device where the file picker finds it, optionally under another name (`as`)."
    )]
    async fn push(
        &self,
        Parameters(p): Parameters<PushParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = p.as_name.clone().unwrap_or_else(|| p.file.clone());
        let fixtures = self.hub.fixtures_dir();
        self.device(&p.token, async |d| {
            if !report::safe_name(&p.file) || !report::safe_name(&name) {
                return Err(DriverError::Invalid(
                    "file names only: letters, digits, - _ and .".into(),
                ));
            }
            let source = fixtures.join(&p.file);
            if !source.exists() {
                return Err(DriverError::Invalid(format!(
                    "no {} in the suite's fixtures",
                    p.file
                )));
            }
            d.device.push_file(&source, &name).await?;
            Ok(format!("pushed {name}"))
        })
        .await
    }

    #[tool(
        description = "Tester: have the other side act while you watch, as the suite names it (e.g. an owner revoking your access). Returns what the action printed."
    )]
    async fn act(
        &self,
        Parameters(p): Parameters<CheckDataParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let checked = self.hub.with_run(|run, now| {
            run.running(&p.token)?;
            run.call_started(now, &p.token);
            Ok(())
        });
        if let Err(e) = checked {
            return reply(Err(e));
        }
        let result = self.hub.project_hook("act", &p.name, &p.args).await;
        let _ = self.hub.with_run(|run, now| {
            run.call_finished(now, &p.token);
            Ok(())
        });
        reply(result.map(|out| json!({ "output": out.trim() })))
    }

    #[tool(
        description = "Tester: one of the project's data checks, for outcomes the screen can't show (e.g. what was saved). Returns what the check printed, as evidence."
    )]
    async fn check_data(
        &self,
        Parameters(p): Parameters<CheckDataParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let checked = self.hub.with_run(|run, now| {
            run.running(&p.token)?;
            run.call_started(now, &p.token);
            Ok(())
        });
        if let Err(e) = checked {
            return reply(Err(e));
        }
        let result = self.hub.project_hook("check", &p.name, &p.args).await;
        let _ = self.hub.with_run(|run, now| {
            run.call_finished(now, &p.token);
            Ok(())
        });
        reply(result.map(|out| json!({ "output": out.trim() })))
    }

    #[tool(
        description = "Tester: several steps in one call (tap, fill, type, clear, expect, gone, wait, check, hold, swipe, touch, release, hover, goto, press, pause), stopping at the first that fails; pause steps keep a sequence's timing on the device. Inputs start at least 500 ms after the last one ended, as a person's would. Returns what each step did."
    )]
    async fn batch(
        &self,
        Parameters(p): Parameters<BatchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // A long wait in one call outlives the tester's MCP session, and its reply never arrives (SA-761).
        let paused: u64 = p
            .steps
            .iter()
            .map(|s| match s {
                Step::Pause { ms } => *ms,
                _ => 0,
            })
            .sum();
        if paused > MAX_PAUSE_MS {
            return reply(Err(anyhow::anyhow!(
                "the batch's pauses add up to {paused} ms, over the {MAX_PAUSE_MS} one wait allows: wait out a long time with separate `sleep` calls"
            )));
        }
        self.device(&p.token, async |d| {
            let mut done = Vec::new();
            let mut last_input: Option<tokio::time::Instant> = None;
            for (i, s) in p.steps.iter().enumerate() {
                if s.is_input()
                    && let Some(at) = last_input
                {
                    tokio::time::sleep_until(at + INPUT_GAP).await;
                }
                let result = step(d, s).await;
                if s.is_input() {
                    last_input = Some(tokio::time::Instant::now());
                }
                match result {
                    Ok(what) => done.push(what),
                    Err(e) => {
                        return Err(DriverError::Invalid(format!(
                            "step {} failed ({e}); done before it: {}",
                            i + 1,
                            if done.is_empty() {
                                "nothing".into()
                            } else {
                                done.join("; ")
                            }
                        )));
                    }
                }
            }
            Ok(done)
        })
        .await
    }

    #[tool(
        description = "Tester: is a placement saved where its preview showed it? Holds a touch from `from` (to `to`), screenshots the preview, releases, screenshots the saved result and compares them; or, with `commit`, saves what's on screen by tapping it. Returns PASSED or FAILED with each edge's offset, and the screenshot names to attach as evidence."
    )]
    async fn preview_compare(
        &self,
        Parameters(p): Parameters<PreviewCompareParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let dir = self.hub.run_dir.join(report::EVIDENCE_DIR);
        let (spec, platform, attempt) = match self.attempt_of(&p.token) {
            Ok(row) => row,
            Err(e) => return reply(Err(e)),
        };
        let name = format!(
            "{}-{}",
            report::evidence_base(&spec, attempt, platform),
            p.name
        );
        self.device(&p.token, async |d| {
            if !report::safe_name(&p.name) {
                return Err(DriverError::Invalid(
                    "name: letters, digits, - and _ only".into(),
                ));
            }
            let from = match (p.from, &p.commit) {
                (Some([x, y]), _) => (x, y),
                (None, Some(_)) => (0.0, 0.0),
                (None, None) => {
                    return Err(DriverError::Invalid(
                        "give `from`, or `commit` with a `region`".into(),
                    ));
                }
            };
            let request = soundcheck_driver::ops::CompareRequest {
                from,
                to: p.to.map(|[x, y]| (x, y)),
                commit: p.commit.as_deref().map(matcher).transpose()?,
                colour: p
                    .colour
                    .as_deref()
                    .map(soundcheck_driver::compare::parse_colour)
                    .transpose()?,
                region: p.region.map(|[x, y, w, h]| soundcheck_driver::Rect {
                    x,
                    y,
                    width: w,
                    height: h,
                }),
                tolerance: 2,
            };
            let result = d.preview_compare(&request, &dir, &name).await?;
            Ok(json!({
                "result": result.summary(),
                "passed": result.passed,
                "evidence": [format!("{name}-preview.png"), format!("{name}-saved.png")],
            }))
        })
        .await
    }

    #[tool(
        description = "Tester: double-tap an element, timed on the device at a person's double-tap speed (one tap call at a time is too slow to count as one)."
    )]
    async fn double_tap(
        &self,
        Parameters(p): Parameters<TargetParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            Ok(line(&d.double_tap(&matcher(&p.target)?, p.last).await?))
        })
        .await
    }

    #[tool(
        description = "Tester: cut the device's network for `seconds`. Returns once it's down; it comes back on its own after `seconds`, so `sleep` to wait it out."
    )]
    async fn network_drop(
        &self,
        Parameters(p): Parameters<SecondsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            d.network_drop(p.seconds)
                .await
                .map(|()| format!("network down for {} s; it comes back on its own", p.seconds))
        })
        .await
    }

    #[tool(
        description = "Tester: take the app offline (`on: true`: connections refused, as in airplane mode) or bring it back (`on: false`). It stays offline until you bring it back; the run brings it back after your spec."
    )]
    async fn offline(
        &self,
        Parameters(p): Parameters<OfflineParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            d.offline(p.on)
                .await
                .map(|()| if p.on { "offline" } else { "online" }.to_string())
        })
        .await
    }

    #[tool(
        description = "Tester: have a service answer every request to `path` with `status` (400–599) instead of passing it on, as when it fails or refuses; `*` stands for any one path segment (`/files/*/upload-url`). Faults stay until `clear: true`; the run clears them after your spec. Returns the faults now set."
    )]
    async fn fault(
        &self,
        Parameters(p): Parameters<FaultParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| match (p.clear, &p.path, p.status) {
            (true, None, None) => d.fault_clear().await.map(|()| "no faults".to_string()),
            (false, Some(path), Some(status)) => d.fault(path, status).await,
            _ => Err(soundcheck_driver::DriverError::Invalid(
                "fault takes a path and a status, or clear: true alone".into(),
            )),
        })
        .await
    }
}

impl Tools {
    /// The token's spec, platform and attempt.
    fn attempt_of(&self, token: &str) -> anyhow::Result<(String, Platform, u32)> {
        self.hub.with_run(|run, _| {
            let (spec, platform) = run.running(token)?;
            let attempt = run.row(&spec, platform).map_or(0, |r| r.attempts);
            Ok((spec, platform, attempt))
        })
    }

    /// Runs a device operation for the platform the token's spec is on, counting it as the tester's activity.
    async fn device<T: Serialize>(
        &self,
        token: &str,
        op: impl AsyncFnOnce(&Driver<AnyDevice>) -> Result<T, DriverError>,
    ) -> Result<CallToolResult, ErrorData> {
        let platform = match self.hub.with_run(|run, now| {
            let (_, platform) = run.running(token)?;
            run.call_started(now, token);
            Ok(platform)
        }) {
            Ok(p) => p,
            Err(e) => return reply(Err(e)),
        };
        let result = self.hub.on_device(platform, op).await;
        let _ = self.hub.with_run(|run, now| {
            run.call_finished(now, token);
            Ok(())
        });
        reply(result.and_then(|v| Ok(serde_json::to_value(v)?)))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Tools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS)
            .with_server_info(rmcp::model::Implementation::new(
                "soundcheck",
                crate::VERSION,
            ))
    }
}
