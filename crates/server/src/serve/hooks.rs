//! A project's hooks: shell commands run from the suite folder. Exit 0 is ok; anything else is a refusal.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::time::Duration;

/// A seed can rebuild a whole account; anything slower than this is stuck.
pub const SEED_TIMEOUT: Duration = Duration::from_secs(300);
pub const GUARD_TIMEOUT: Duration = Duration::from_secs(30);
/// Run only once the Mac is calm, so a hook slower than this is stuck, not starved.
pub const BUILD_IDENTITY_TIMEOUT: Duration = Duration::from_secs(60);
/// A release build of a mobile app takes several minutes, more with a cold cache.
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// A hook that ran out of time, told apart from one that failed.
#[derive(Debug)]
pub struct TimedOut {
    pub command: String,
    pub after: Duration,
}

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`{}` took longer than {} s",
            self.command,
            self.after.as_secs()
        )
    }
}

impl std::error::Error for TimedOut {}

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
        .map_err(|_| TimedOut {
            command: command.to_string(),
            after: timeout,
        })?
        .with_context(|| format!("couldn't run `{command}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
        let mut reason = tail.iter().rev().copied().collect::<Vec<_>>().join(" ");
        // A long build's tail is often a stack trace; its first error line is the reason.
        if let Some(first) = stderr
            .lines()
            .find(|l| l.contains("Error:") || l.contains("error:"))
            .filter(|l| !tail.contains(l))
        {
            reason = format!("{} … {reason}", first.trim());
        }
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
        // Compact: the brief is read on every turn of the tester's, and indentation was most of a big seed's size.
        other => serde_json::to_string(other).ok()?,
    })
}

/// How long a web app's dev server gets to start answering.
pub const START_TIMEOUT: Duration = Duration::from_secs(120);

/// A long-running process a hook started, such as the web app's dev server. Dropping it stops the whole process
/// group, because a dev server usually runs under `npm` or `sh`, and killing only the parent leaves it serving.
pub struct Service {
    child: tokio::process::Child,
}

impl Service {
    /// Starts `command` from the suite folder and waits until `url` answers (any HTTP status).
    pub async fn start(
        root: &Path,
        command: &str,
        url: &str,
        timeout: Duration,
    ) -> Result<Service> {
        let child = tokio::process::Command::new("sh")
            .args(["-c", command])
            .current_dir(root)
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("couldn't start `{command}`"))?;
        let mut service = Service { child };
        let client = reqwest::Client::new();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if client.get(url).send().await.is_ok() {
                return Ok(service);
            }
            if let Ok(Some(status)) = service.child.try_wait() {
                bail!("`{command}` stopped ({status}) before {url} answered");
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "{url} didn't answer within {} s of `{command}`",
                    timeout.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(pid) = self.child.id() {
            // The group shares the child's id, as started with process_group(0).
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &format!("-{pid}")])
                .status();
        }
    }
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
        assert_eq!(
            context(r#"{"context": {"scores": [{"title": "Elgar", "parts": 2}]}}"#).as_deref(),
            Some(r#"{"scores":[{"parts":2,"title":"Elgar"}]}"#),
            "all of it, compact"
        );
    }

    #[tokio::test]
    async fn a_hook_that_runs_out_of_time_is_told_apart_from_one_that_fails() {
        let dir = std::env::temp_dir();
        let slow = run(&dir, "sleep 5", Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(slow.chain().any(|e| e.is::<TimedOut>()));
        let failing = run(&dir, "exit 1", GUARD_TIMEOUT).await.unwrap_err();
        assert!(!failing.chain().any(|e| e.is::<TimedOut>()));
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

    #[tokio::test]
    async fn a_web_app_is_started_until_it_answers_then_stopped() {
        let dir = std::env::temp_dir().join(format!("soundcheck-service-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "<h1>app</h1>").unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}/");
        let command = format!("sleep 0.5 && exec python3 -m http.server {port} --bind 127.0.0.1");
        let service = Service::start(&dir, &command, &url, Duration::from_secs(20))
            .await
            .unwrap();
        let body = reqwest::get(&url).await.unwrap().text().await.unwrap();
        assert!(body.contains("<h1>app</h1>"));
        drop(service);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(reqwest::get(&url).await.is_err(), "stopped with the run");

        let err = Service::start(&dir, "exit 4", &url, Duration::from_secs(5))
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("stopped"), "{err}");
    }
}
