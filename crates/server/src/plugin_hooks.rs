//! The plugin's Stop, SessionStart and PreToolUse hooks. Each asks the run serving on the port and prints the hook's
//! JSON reply, or nothing: with no run up, sessions are left alone.

use crate::serve::{Dispatched, Summary};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

const TESTERS: [&str; 2] = [
    "soundcheck:soundcheck-tester",
    "soundcheck:soundcheck-tester-mechanics",
];

/// Covers a dispatch's reset, which seeds and relaunches the app, three times at worst.
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(590);

#[derive(Deserialize)]
struct StopInput {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    stop_hook_active: bool,
}

#[derive(Deserialize)]
struct ToolInput {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    tool_input: Value,
}

#[derive(Deserialize)]
struct Next {
    spec: String,
    platform: String,
}

/// The run's summary, or None when no run is serving on the port.
async fn summary(port: u16, session: Option<&str>) -> Result<Option<Summary>> {
    let mut url = format!("http://127.0.0.1:{port}/summary");

    // Session IDs are UUIDs, so anything else needn't be escaped: it's left out.
    if let Some(session) =
        session.filter(|s| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
    {
        url.push_str(&format!("?session={session}"));
    }

    let request = reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(3));
    match request.send().await {
        Ok(response) => Ok(Some(response.error_for_status()?.json().await?)),
        Err(e) if e.is_connect() => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Blocks the coordinator from stopping while a row could be dispatched and nothing waits for review: nothing running,
/// or with platforms in parallel, a platform left idle. Only a session that has started testers is held, and only once per stop.
pub async fn stop(stdin: &str, port: u16) -> Result<Option<String>> {
    let input: StopInput = serde_json::from_str(stdin).context("hook input isn't JSON")?;

    if input.stop_hook_active {
        return Ok(None);
    }

    let Some(s) = summary(port, Some(&input.session_id)).await? else {
        return Ok(None);
    };
    // A running tester doesn't make the session busy: with platforms in parallel, another may be free, and `/next`
    // refuses when none is.
    let busy = !s.review.is_empty() || s.side_finds > 0;

    if !s.coordinator || s.finished || s.paused || busy {
        return Ok(None);
    }

    let response = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/next"))
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    if !response.status().is_success() {
        return Ok(None);
    }

    let next: Next = response.json().await?;
    Ok(Some(
        json!({
            "decision": "block",
            "reason": format!(
                "dispatch {} on {}: it could start now and nothing has started it, so start its tester now",
                next.spec, next.platform
            ),
        })
        .to_string(),
    ))
}

/// Tells a new session about the run serving on the port, so a coordinator after `/clear` knows where it stands.
pub async fn session_start(port: u16) -> Result<Option<String>> {
    let Some(s) = summary(port, None).await? else {
        return Ok(None);
    };
    let mut state = vec![format!("verdict {}", s.verdict)];

    if s.paused {
        state.push("paused".into());
    }

    if s.finished {
        state.push("finished".into());
    }

    let mut open = vec![format!("{} of {} rows left", s.left, s.rows)];

    if !s.running.is_empty() {
        open.push(format!("running: {}", s.running.join(", ")));
    }

    if !s.review.is_empty() {
        open.push(format!("waiting for review: {}", s.review.join(", ")));
    }

    if s.side_finds > 0 {
        open.push(format!("{} side finds waiting for review", s.side_finds));
    }

    let context = format!(
        "A soundcheck run is up: {} ({}), {}. {}. To coordinate it, use the soundcheck run skill (`soundcheck:run`).",
        s.folder.display(),
        s.label,
        state.join(", "),
        open.join("; "),
    );
    Ok(Some(
        json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": context,
            },
        })
        .to_string(),
    ))
}

/// Turns the coordinator's tester Agent call into a dispatch: the server starts the next row (or the one the prompt
/// names) and the call's input is rewritten to that attempt's prompt, agent type, model and token. A refusal denies
/// the call with the server's reason.
pub async fn pre_tool_use(stdin: &str, port: u16) -> Result<Option<String>> {
    let input: ToolInput = serde_json::from_str(stdin).context("hook input isn't JSON")?;
    let agent = input.tool_input["subagent_type"]
        .as_str()
        .unwrap_or_default();

    if !matches!(input.tool_name.as_str(), "Agent" | "Task") || !TESTERS.contains(&agent) {
        return Ok(None);
    }

    let prompt = input.tool_input["prompt"].as_str().unwrap_or_default();
    let sent = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/dispatch"))
        .timeout(DISPATCH_TIMEOUT)
        .json(&json!({ "prompt": prompt, "session": input.session_id }))
        .send()
        .await;
    let response = match sent {
        Ok(response) => response,
        Err(e) if e.is_connect() => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let decision = if response.status().is_success() {
        let d: Dispatched = response.json().await?;
        let mut updated = input.tool_input.clone();
        updated["subagent_type"] = json!(format!("soundcheck:{}", d.agent));
        updated["prompt"] = json!(d.prompt);
        updated["description"] = json!(d.token);
        updated["run_in_background"] = json!(true);
        match d.model {
            Some(model) => updated["model"] = json!(model),
            None => {
                if let Some(fields) = updated.as_object_mut() {
                    fields.remove("model");
                }
            }
        }

        json!({
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "permissionDecisionReason": format!("soundcheck dispatched {}", d.token),
            "updatedInput": updated,
        })
    } else {
        let reason = response.text().await.unwrap_or_default();
        json!({
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": format!("soundcheck refused the dispatch: {}", reason.trim()),
        })
    };
    Ok(Some(json!({ "hookSpecificOutput": decision }).to_string()))
}
