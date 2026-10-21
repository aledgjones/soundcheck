use super::{Hub, prompt};
use crate::report;
use crate::run::model::{Build, Call, Claim, Decision, Judgment, Posted, Rerun, SideFind};
use crate::run::{Row, verdict};
use crate::suite::config::Platform;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use soundcheck_driver::web::Web;
use soundcheck_driver::{Device, Driver, DriverError, Element, Matcher};
use std::sync::Arc;

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
    /// One judgment per outcome, by its number in your brief.
    pub judgments: Vec<Judgment>,
    /// Problems found while probing that the outcomes don't cover.
    #[serde(default)]
    pub side_finds: Vec<SideFind>,
    pub notes: Option<String>,
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
    /// Rows you judge the change may affect, each with a reason. Native changes and open problems rerun anyway.
    #[serde(default)]
    pub reruns: Vec<Rerun>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReasonParams {
    pub reason: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TokenParams {
    /// Your spec token.
    pub token: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TargetParams {
    pub token: String,
    /// An element's label, value or `see` line; exact unless it starts with `prefix:` or `re:`.
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
pub struct ShotParams {
    pub token: String,
    /// A file name for the screenshot, e.g. `undo-after`; attach it to a judgment's evidence.
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
    /// Milliseconds: the swipe's duration, or how long a drag holds before moving.
    pub ms: u64,
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
pub struct BurstParams {
    pub token: String,
    pub target: String,
    pub count: u32,
    pub interval_ms: u64,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SecondsParams {
    pub token: String,
    pub seconds: u64,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct StressParams {
    pub token: String,
    /// Percent of the device's CPU.
    pub load: u32,
    pub seconds: u64,
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
    Hover {
        target: String,
    },
    Goto {
        url: String,
    },
}

async fn step(d: &Driver<Web>, step: &Step) -> Result<String, DriverError> {
    Ok(match step {
        Step::Tap { target, last } => {
            format!("tapped {}", line(&d.tap(&matcher(target)?, *last).await?))
        }
        Step::Fill { target, text } => {
            d.fill(&matcher(target)?, text).await?;
            format!("filled {target}")
        }
        Step::Type { text } => {
            d.device.type_text(text).await?;
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
        Step::Hover { target } => {
            let e = d.expect(&matcher(target)?).await?;
            let (x, y) = e.tap_point();
            d.device.hover_at(x, y).await?;
            format!("hovered {}", line(&e))
        }
        Step::Goto { url } => {
            d.device.goto(url).await?;
            format!("opened {url}")
        }
    })
}

fn matcher(target: &str) -> Result<Matcher, DriverError> {
    Matcher::parse(target).map_err(|e| DriverError::Invalid(format!("{target}: {e}")))
}

fn line(e: &Element) -> String {
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
}

#[tool_router]
impl Tools {
    pub fn new(hub: Arc<Hub>) -> Self {
        Tools {
            hub,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Tester: post your spec's result once, at the end, judging every outcome. A PASS files at once; anything else goes to the coordinator for review."
    )]
    async fn post_result(
        &self,
        Parameters(p): Parameters<PostResultParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let posted = Posted {
            judgments: p.judgments,
            side_finds: p.side_finds,
            notes: p.notes,
        };
        let posted = self.hub.with_run(|run, now| {
            let (spec, platform) = run.running(&p.token)?;
            let attempt = run.row(&spec, platform).map_or(0, |r| r.attempts);
            let claim = run.post(now, &p.token, posted)?;
            Ok((claim, spec, platform, attempt))
        });
        let (claim, spec, platform, attempt) = match posted {
            Ok(posted) => posted,
            Err(e) => return reply(Err(e)),
        };
        // Save the evidence now: the next spec's seed wipes the device, and review comes later.
        if claim != Claim::Pass {
            let files = self.hub.capture(&spec, platform, attempt).await;
            let _ = self
                .hub
                .with_run(|run, now| run.evidence_captured(now, &spec, platform, attempt, files));
        }
        reply(Ok(
            json!({ "claim": claim, "next": "you're done: stop here" }),
        ))
    }

    #[tool(
        description = "Coordinator: the run at a glance: verdict and its reasons, every row's state, problems, side finds waiting, and what stops the run finishing."
    )]
    async fn status(&self) -> Result<CallToolResult, ErrorData> {
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
                    RowStatus {
                        spec,
                        platform: *platform,
                        state: label,
                        attempts: r.attempts,
                        result,
                        claim,
                    }
                })
                .collect();
            let finds: Vec<_> = state
                .pending_finds
                .iter()
                .map(|f| json!({ "id": f.id, "spec": f.spec, "platform": f.platform, "find": f.find }))
                .collect();
            Ok(json!({
                "verdict": verdict::verdict(state),
                "paused": state.paused,
                "finished": state.finished,
                "rows": rows,
                "problems": state.problems,
                "side_finds_waiting": finds,
                "notes": state.notes.iter().map(|n| format!("{} on {}: {}", n.spec, n.platform, n.text)).collect::<Vec<_>>(),
                "unclear_specs": state.unclear_specs,
                "finish_blockers": run.finish_blockers(),
            }))
        }))
    }

    #[tool(
        description = "Coordinator: start a spec's next attempt on a platform. Returns the tester's brief: pass it unchanged to a background soundcheck-tester agent."
    )]
    async fn dispatch(
        &self,
        Parameters(p): Parameters<DispatchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let hub = self.hub.clone();
        let Some(spec) = hub.suite.specs.iter().find(|s| s.id == p.spec) else {
            return reply(Err(anyhow::anyhow!("no spec {} in the suite", p.spec)));
        };
        // Check first, so a refused dispatch never resets a device.
        if let Err(e) = hub.with_run(|run, _| run.check_dispatch(&p.spec, p.platform)) {
            return reply(Err(e));
        }
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
        let context = hub
            .suite
            .features
            .iter()
            .find(|f| f.name == spec.feature)
            .and_then(|f| f.context.as_deref());
        reply(hub.with_run(|run, now| {
            let mut brief = String::new();
            let token = run.dispatch(now, &p.spec, p.platform, |token, verify| {
                brief = prompt::render(spec, context, p.platform, token, verify, seeded.as_deref());
                brief.clone()
            })?;
            Ok(json!({ "token": token, "model": spec.front.model, "brief": brief }))
        }))
    }

    #[tool(
        description = "Coordinator: your call on a result waiting for review: uphold, verify (a fresh attempt told the claim), send-back, judge (an Unsure only) or spec-unclear. Every call needs a one-line reason."
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
        description = "Coordinator: your call on a side find: give it a title in plain words to make it a finding, or none if it isn't one."
    )]
    async fn review_find(
        &self,
        Parameters(p): Parameters<ReviewFindParams>,
    ) -> Result<CallToolResult, ErrorData> {
        reply(self.hub.with_run(|run, now| {
            run.review_find(now, p.id, p.title, &p.reason)?;
            Ok(json!({ "reviewed_find": p.id }))
        }))
    }

    #[tool(
        description = "Coordinator: record a triage decision the person made: dismiss, fix (ticket; its row must have passed again), defer (ticket) or duplicate of another problem."
    )]
    async fn triage(
        &self,
        Parameters(p): Parameters<TriageParams>,
    ) -> Result<CallToolResult, ErrorData> {
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
            Ok(d.see(filter.as_ref(), p.all)
                .await?
                .iter()
                .map(line)
                .collect::<Vec<_>>())
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
        description = "Tester: save a screenshot as evidence; returns its file name to attach to a judgment. Only for visual checks."
    )]
    async fn shot(
        &self,
        Parameters(p): Parameters<ShotParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let dir = self.hub.run_dir.join(report::EVIDENCE_DIR);
        let name = format!("{}.png", p.name.trim_end_matches(".png"));
        self.device(&p.token, async |d| {
            if !report::safe_name(&name) {
                return Err(DriverError::Invalid(format!(
                    "{name}: use letters, digits, - and _ only"
                )));
            }
            std::fs::create_dir_all(&dir).map_err(|e| DriverError::Device(e.to_string()))?;
            d.device.screenshot(&dir.join(&name)).await?;
            Ok(name)
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
            d.device.type_text(&p.text).await.map(|()| "typed")
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
        description = "Tester: press, hold for `ms` milliseconds, then drag to the second point (reorder handles)."
    )]
    async fn drag(
        &self,
        Parameters(p): Parameters<GestureParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            d.drag((p.from[0], p.from[1]), (p.to[0], p.to[1]), p.ms)
                .await
                .map(|()| "dragged")
        })
        .await
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
        description = "Tester: several steps in one call (tap, fill, type, clear, expect, gone, wait, check, hold, swipe, hover, goto), stopping at the first that fails. Returns what each step did."
    )]
    async fn batch(
        &self,
        Parameters(p): Parameters<BatchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            let mut done = Vec::new();
            for (i, s) in p.steps.iter().enumerate() {
                match step(d, s).await {
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
        description = "Tester: tap an element `count` times, `interval_ms` apart, timed on the device: for races."
    )]
    async fn tap_burst(
        &self,
        Parameters(p): Parameters<BurstParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            Ok(line(
                &d.tap_burst(&matcher(&p.target)?, p.count, p.interval_ms)
                    .await?,
            ))
        })
        .await
    }

    #[tool(description = "Tester: cut the device's network for `seconds`, then restore it.")]
    async fn network_drop(
        &self,
        Parameters(p): Parameters<SecondsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            d.network_drop(p.seconds).await.map(|()| "restored")
        })
        .await
    }

    #[tool(description = "Tester: load the device's CPU for `seconds`.")]
    async fn stress(
        &self,
        Parameters(p): Parameters<StressParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.device(&p.token, async |d| {
            d.stress(p.load, p.seconds).await.map(|()| "done")
        })
        .await
    }
}

impl Tools {
    /// Runs a device operation for the platform the token's spec is on, counting it as the tester's activity.
    async fn device<T: Serialize>(
        &self,
        token: &str,
        op: impl AsyncFnOnce(&Driver<Web>) -> Result<T, DriverError>,
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
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Soundcheck runs a regression suite. Testers use post_result with their spec token; the coordinator uses status, dispatch, review, review_find, install_build, triage, pause, resume and finish.",
        )
    }
}
