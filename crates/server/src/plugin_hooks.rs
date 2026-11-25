//! The plugin's SessionStart hook: it asks the run serving on the port and prints the hook's JSON reply, or nothing:
//! with no run up, sessions are left alone. The hooks that made a coordinator dispatch went with SA-1049: the server
//! dispatches.

use crate::serve::Summary;
use anyhow::Result;
use serde_json::json;
use std::time::Duration;

/// The run's summary, or None when no run is serving on the port.
async fn summary(port: u16) -> Result<Option<Summary>> {
    let request = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/summary"))
        .timeout(Duration::from_secs(3));
    match request.send().await {
        Ok(response) => Ok(Some(response.error_for_status()?.json().await?)),
        // A server that's gone, or stopping mid-request (the connection reset), is no run to report on.
        Err(e) if e.is_connect() || e.is_request() => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Tells a new session about the run serving on the port, so a coordinator after `/clear` knows where it stands.
pub async fn session_start(port: u16) -> Result<Option<String>> {
    let Some(s) = summary(port).await? else {
        return Ok(None);
    };
    let mut state = vec![format!("verdict {}", s.verdict)];

    if s.uncommitted {
        state.push("a trial on uncommitted code, so no tickets".into());
    }

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
        "A soundcheck run is up: {} ({}), {}. {}. Its server dispatches and reviews by itself; to coordinate it, use the soundcheck run skill (`soundcheck:run`).",
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
