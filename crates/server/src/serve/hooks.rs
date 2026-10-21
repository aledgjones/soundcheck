//! A project's hooks: shell commands run from the suite folder. Exit 0 is ok; anything else is a refusal.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::time::Duration;

/// A seed can rebuild a whole account; anything slower than this is stuck.
pub const SEED_TIMEOUT: Duration = Duration::from_secs(300);
pub const GUARD_TIMEOUT: Duration = Duration::from_secs(30);

/// Single-quoted for `sh`, so a scenario or platform name can't become shell syntax.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Fills `{scenario}` and `{platform}` in a hook's command line.
pub fn fill(command: &str, scenario: &str, platform: &str) -> String {
    command
        .replace("{scenario}", &quote(scenario))
        .replace("{platform}", &quote(platform))
}

/// Runs a hook and returns its stdout. A failure carries the end of its stderr, which is where the reason is.
pub async fn run(root: &Path, command: &str, timeout: Duration) -> Result<String> {
    let child = tokio::process::Command::new("sh")
        .args(["-c", command])
        .current_dir(root)
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(timeout, child)
        .await
        .with_context(|| format!("`{command}` took longer than {} s", timeout.as_secs()))?
        .with_context(|| format!("couldn't run `{command}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
        let reason = tail.into_iter().rev().collect::<Vec<_>>().join(" ");
        bail!(
            "`{command}` failed{}",
            if reason.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", reason.trim())
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The context a seed hook may print as `{"context": ...}`, for the tester's brief.
pub fn context(stdout: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let context = value.get("context")?;
    Some(match context {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_quoted() {
        assert_eq!(
            fill("seed {scenario} --on {platform}", "viewer", "ios"),
            "seed 'viewer' --on 'ios'"
        );
        assert_eq!(
            fill("seed {scenario}", "a'b; rm", "x"),
            r"seed 'a'\''b; rm'"
        );
    }

    #[test]
    fn a_seed_can_hand_the_tester_context() {
        assert_eq!(
            context(r#"{"context": "Collection: E2E Seed Library"}"#).as_deref(),
            Some("Collection: E2E Seed Library")
        );
        assert!(context("seeded").is_none());
    }

    #[tokio::test]
    async fn a_failing_hook_says_why() {
        let dir = std::env::temp_dir();
        let err = run(
            &dir,
            "echo noise >&2; echo 'not localhost' >&2; exit 3",
            GUARD_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not localhost"), "{err}");
        assert_eq!(
            run(&dir, "echo ok", GUARD_TIMEOUT).await.unwrap().trim(),
            "ok"
        );
        let slow = run(&dir, "sleep 5", Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(slow.to_string().contains("took longer"), "{slow}");
    }
}
