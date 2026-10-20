use super::{Hub, prompt};
use crate::run::model::{Build, Call, Decision, Judgment, Posted, Rerun, Severity, SideFind};
use crate::run::{Row, verdict};
use crate::suite::config::Platform;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::json;
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
    pub proposed_severity: Option<Severity>,
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
    /// A severity makes it a problem; none means it isn't one.
    pub severity: Option<Severity>,
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
    pub build: Build,
    /// Rows you judge the change may affect, each with a reason. Native changes and open problems rerun anyway.
    #[serde(default)]
    pub reruns: Vec<Rerun>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReasonParams {
    pub reason: String,
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
            proposed_severity: p.proposed_severity,
            notes: p.notes,
        };
        reply(self.hub.with_run(|run, now| {
            let claim = run.post(now, &p.token, posted)?;
            Ok(json!({ "claim": claim, "next": "you're done: stop here" }))
        }))
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
        reply(self.hub.with_run(|run, now| {
            let Some(spec) = hub.suite.specs.iter().find(|s| s.id == p.spec) else {
                anyhow::bail!("no spec {} in the suite", p.spec);
            };
            let context = hub
                .suite
                .features
                .iter()
                .find(|f| f.name == spec.feature)
                .and_then(|f| f.context.as_deref());
            let mut brief = String::new();
            let token = run.dispatch(now, &p.spec, p.platform, |token, verify| {
                brief = prompt::render(spec, context, p.platform, token, verify);
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
        description = "Coordinator: your call on a side find: give a severity to make it a problem, or none if it isn't one."
    )]
    async fn review_find(
        &self,
        Parameters(p): Parameters<ReviewFindParams>,
    ) -> Result<CallToolResult, ErrorData> {
        reply(self.hub.with_run(|run, now| {
            run.review_find(now, p.id, p.severity, &p.reason)?;
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
        reply(self.hub.with_run(|run, now| {
            let reruns = run.install_build(now, p.platform, p.build, p.reruns)?;
            Ok(json!({ "reruns": reruns }))
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
        description = "Coordinator: finish the run once nothing is running, waiting for review or queued. Triage still works afterwards."
    )]
    async fn finish(&self) -> Result<CallToolResult, ErrorData> {
        reply(self.hub.with_run(|run, now| {
            let counts = run.finish(now)?;
            Ok(json!({ "results": counts, "verdict": run.verdict() }))
        }))
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
