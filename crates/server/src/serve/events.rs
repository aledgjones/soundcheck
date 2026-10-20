use crate::run::event::Event;
use crate::run::journal::Entry;
use crate::run::model::Claim;

/// One plain line per journal entry, for the coordinator's Monitor: every line wakes it.
pub fn summary(entry: &Entry) -> String {
    let what = match &entry.event {
        Event::Dispatched {
            spec,
            platform,
            attempt,
            verify,
            ..
        } => format!(
            "dispatched {spec} on {platform} (attempt {attempt}{})",
            if verify.is_some() {
                ", verification"
            } else {
                ""
            }
        ),
        Event::Posted { token, posted } => {
            let outcome = match posted.claim() {
                Claim::Pass => "PASS: filed",
                Claim::Fail => "FAIL: needs review",
                Claim::Blocked => "BLOCKED: needs review",
                Claim::Unsure => "Unsure: needs review",
            };
            let finds = match posted.side_finds.len() {
                0 => String::new(),
                n => format!("; {n} side find(s) need review"),
            };
            format!("{token} posted {outcome}{finds}")
        }
        Event::Reviewed {
            spec,
            platform,
            call,
            reason,
            ..
        } => format!(
            "reviewed {spec} on {platform}: {} ({reason})",
            serde_json::to_value(call)
                .ok()
                .and_then(|v| v.get("call").and_then(|c| c.as_str()).map(str::to_string))
                .unwrap_or_default()
        ),
        Event::FindReviewed { id, severity, .. } => match severity {
            Some(s) => format!("side find {id} upheld as a {s:?}").to_lowercase(),
            None => format!("side find {id} isn't a problem"),
        },
        Event::Stalled { token, reason } => format!("{token} stalled: {reason}; queued again"),
        Event::ResetFailed {
            spec,
            platform,
            reason,
        } => format!("{spec} on {platform}: reset failed ({reason}); filed BLOCKED"),
        Event::Triaged {
            problem, reason, ..
        } => format!("problem {problem} triaged ({reason})"),
        Event::BuildInstalled {
            platform,
            build,
            reruns,
        } => format!(
            "build {} installed on {platform}: {} spec(s) to rerun",
            build.label(),
            reruns.len()
        ),
        Event::Paused { reason } => format!("paused: {reason}"),
        Event::Resumed => "resumed".into(),
        Event::Finished { result_counts: c } => format!(
            "finished: {} PASS, {} FAIL, {} BLOCKED, {} not run",
            c.pass, c.fail, c.blocked, c.not_run
        ),
    };
    format!("#{} {what}", entry.seq)
}
