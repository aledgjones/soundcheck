use crate::run::event::Event;
use crate::run::journal::Entry;
use crate::run::model::{Answer, Claim};
use crate::suite::config::Platform;

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
        Event::Judged { token, judgment } => format!(
            "{token} judged outcome {}: {}",
            judgment.outcome,
            match judgment.answer {
                Answer::Held => "held",
                Answer::Failed => "failed",
                Answer::Unsure => "unsure",
                Answer::CouldntCheck => "couldn't check",
            }
        ),
        Event::Posted {
            token,
            posted,
            unprobed,
            find_ids,
            ..
        } => {
            let outcome = match posted.claim() {
                Claim::Pass if *unprobed => "PASS without trying its probes: needs review",
                Claim::Pass => "PASS: filed",
                Claim::Fail => "FAIL: needs review",
                Claim::Blocked => "BLOCKED: needs review",
                Claim::Unsure => "Unsure: needs review",
            };
            let finds = match posted.side_finds.len() {
                0 => String::new(),
                // Journals from before side finds carried their ids.
                n if find_ids.len() != n => format!("; {n} side find(s) need review"),
                n => format!(
                    "; {n} side find(s) need review: {}",
                    find_ids
                        .iter()
                        .zip(&posted.side_finds)
                        .map(|(id, f)| format!("id {id}: {}", f.summary))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
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
        Event::FindReviewed { id, title, .. } => match title {
            Some(title) => format!("side find {id} is a finding: {title}"),
            None => format!("side find {id} isn't a problem"),
        },
        Event::DeviceHealth {
            spec,
            platform,
            degraded,
            busy,
            facts,
            ..
        } => {
            if degraded.is_empty() && !busy.is_empty() {
                format!(
                    "{platform} healthy after {spec}, but the Mac is busy ({}): the next spec waits for it",
                    busy.join(", ")
                )
            } else if degraded.is_empty() {
                format!("{platform} healthy after {spec}: {}", facts.join(", "))
            } else {
                format!(
                    "{platform} DEGRADED after {spec}: {} (restarting before the next spec)",
                    degraded.join(", ")
                )
            }
        }
        Event::DeviceRestarted {
            platform, reason, ..
        } => format!("{platform} restarted: {reason}"),
        Event::HostBusy {
            platform,
            spec,
            reason,
            waited_ms,
            settled,
        } => format!(
            "{platform} waited {} s for the Mac before {spec} ({reason}){}",
            waited_ms / 1000,
            if *settled {
                ""
            } else {
                "; the Mac didn't settle, so specs go ahead anyway"
            }
        ),
        Event::EvidenceCaptured {
            spec,
            platform,
            files,
            ..
        } => format!(
            "evidence saved for {spec} on {platform}: {}",
            files.join(", ")
        ),
        Event::Preflight { checks } => format!("ready to run: {}", checks.join("; ")),
        Event::Interrupted { token } => format!(
            "{token} interrupted by a server restart; queued again without using an attempt"
        ),
        Event::Stalled { token, reason } => format!("{token} stalled: {reason}; queued again"),
        Event::ResetFailed {
            spec,
            platform,
            reason,
        } => format!("{spec} on {platform}: reset failed ({reason}); filed BLOCKED"),
        Event::Triaged {
            problem, reason, ..
        } => format!("problem {problem} triaged ({reason})"),
        Event::EvidenceAdded {
            problem,
            file,
            reason,
        } => format!("problem {problem} shows {file} ({reason})"),
        Event::BuildInstalled {
            platform,
            build,
            reruns,
        } => format!(
            "build {} installed on {platform}: {} spec(s) to rerun",
            build.label(),
            reruns.len()
        ),
        Event::PlatformsAdded { platforms } => format!(
            "added {}: their specs join the run, which is open again",
            platforms
                .iter()
                .map(Platform::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Event::SpecsAdded { specs } => format!(
            "added {}: their rows join the run, not run yet",
            specs.join(", ")
        ),
        Event::Requeued { rows, reason, .. } => format!(
            "queued again: {} ({reason})",
            rows.iter()
                .map(|r| format!("{} on {}", r.spec, r.platform))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Event::SpecChanged { spec } => {
            format!("{spec} changed since the run started: reread from disk")
        }
        Event::Paused { reason } => format!("paused: {reason}"),
        Event::Resumed => "resumed".into(),
        Event::Finished { result_counts: c } => format!(
            "finished: {} PASS, {} FAIL, {} BLOCKED, {} not run",
            c.pass, c.fail, c.blocked, c.not_run
        ),
    };
    format!("#{} {what}", entry.seq)
}
