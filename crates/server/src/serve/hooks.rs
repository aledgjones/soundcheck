//! A project's hooks: shell commands run from the suite folder. Exit 0 is ok; anything else is a refusal.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// A check or act hook reads or changes the stack; anything slower than this is stuck.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(300);
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

/// Tells a run's hooks it's a trial on uncommitted code (`run --uncommitted`), so a project's hooks may accept a
/// build no commit identifies.
pub const UNCOMMITTED: &str = "SOUNDCHECK_UNCOMMITTED";

/// This soundcheck's own executable, so a hook's `soundcheck drive` is the run's version and never one on the PATH.
pub const BIN: &str = "SOUNDCHECK_BIN";

/// What a run tells every hook it runs, through the environment. Every hook command takes one, so none is missed;
/// a hook outside a run (`stack down`) gets the default.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RunEnv {
    pub uncommitted: bool,
}

impl RunEnv {
    /// The variables to set. Each command also removes [`UNCOMMITTED`] first, so a committed run's hooks never inherit
    /// it from the shell that started soundcheck.
    pub fn vars(self) -> BTreeMap<String, String> {
        let mut vars = BTreeMap::new();

        if let Some(exe) = std::env::current_exe()
            .ok()
            .and_then(|e| e.into_os_string().into_string().ok())
        {
            vars.insert(BIN.to_string(), exe);
        }

        if self.uncommitted {
            vars.insert(UNCOMMITTED.to_string(), "1".to_string());
        }

        vars
    }
}

/// Runs a hook and returns its stdout. A failure carries the end of its stderr, which is where the reason is.
pub async fn run(root: &Path, command: &str, timeout: Duration, run: RunEnv) -> Result<String> {
    run_with(root, command, timeout, run, &BTreeMap::new()).await
}

/// [`run`] with `env` added to the command's environment.
pub async fn run_with(
    root: &Path,
    command: &str,
    timeout: Duration,
    run: RunEnv,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let child = tokio::process::Command::new("sh")
        .args(["-c", command])
        .current_dir(root)
        .env_remove(UNCOMMITTED)
        .envs(run.vars())
        .envs(env)
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

/// What a prepare step printed as `{"values": {...}, "context": ...}`: values become environment variables for the
/// steps after it, and context goes into the tester's brief. Output that isn't such an object hands over nothing.
#[derive(Debug, Default, PartialEq)]
pub struct Printed {
    pub values: BTreeMap<String, String>,
    pub context: Option<String>,
}

pub fn printed(stdout: &str) -> Result<Printed, String> {
    let Ok(serde_json::Value::Object(out)) = serde_json::from_str(stdout.trim()) else {
        return Ok(Printed::default());
    };

    let context = out.get("context").and_then(|context| match context {
        serde_json::Value::String(s) => Some(s.clone()),
        // Compact: the brief is read on every turn of the tester's, and indentation was most of a big seed's size.
        other => serde_json::to_string(other).ok(),
    });
    let mut values = BTreeMap::new();
    let given = out.get("values").and_then(serde_json::Value::as_object);
    for (name, value) in given.into_iter().flatten() {
        let mut chars = name.chars();
        let valid = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with("SOUNDCHECK_");
        if !valid {
            return Err(format!(
                "value {name:?} isn't a variable name (letters, digits and _, not SOUNDCHECK_…)"
            ));
        }

        let text = match value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => return Err(format!("value {name} is a {value}; give it a string")),
        };
        values.insert(name.clone(), text);
    }

    Ok(Printed { values, context })
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
        run: RunEnv,
    ) -> Result<Service> {
        let client = reqwest::Client::new();
        // Something else at `url` would answer for the service, which then can't take its port: the run would test
        // whatever that is.
        if client.get(url).send().await.is_ok() {
            bail!(
                "something already answers at {url}, so `{command}` can't serve there; stop it, or move the platform's `url` and `serve` to a free port"
            );
        }

        let child = tokio::process::Command::new("sh")
            .args(["-c", command])
            .current_dir(root)
            .env_remove(UNCOMMITTED)
            .envs(run.vars())
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("couldn't start `{command}`"))?;
        let mut service = Service { child };
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

    /// Stops the service and waits until `url` no longer answers, so a new one can take its port.
    pub async fn stop(mut self, url: &str, timeout: Duration) -> Result<()> {
        if let Some(pid) = self.child.id() {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &format!("-{pid}")])
                .status();
        }
        let client = reqwest::Client::new();
        let deadline = tokio::time::Instant::now() + timeout;
        let _ = tokio::time::timeout(timeout, self.child.wait()).await;
        while client.get(url).send().await.is_ok() {
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "{url} still answers {} s after its service was stopped",
                    timeout.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Ok(())
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
    fn a_step_hands_on_values_and_context() {
        let out =
            printed(r#"{"values": {"SCORE": "s-1", "PARTS": 2}, "context": "Elgar"}"#).unwrap();
        assert_eq!(out.values["SCORE"], "s-1");
        assert_eq!(out.values["PARTS"], "2");
        assert_eq!(out.context.as_deref(), Some("Elgar"));
        assert_eq!(printed("seeded\n").unwrap(), Printed::default());
        assert_eq!(
            printed(r#"{"context": {"scores": [{"title": "Elgar", "parts": 2}]}}"#)
                .unwrap()
                .context
                .as_deref(),
            Some(r#"{"scores":[{"parts":2,"title":"Elgar"}]}"#),
            "all of it, compact"
        );
        assert!(printed(r#"{"values": {"my-score": "x"}}"#).is_err());
        assert!(printed(r#"{"values": {"SOUNDCHECK_PLATFORM": "x"}}"#).is_err());
        assert!(printed(r#"{"values": {"SCORE": {"id": 1}}}"#).is_err());
    }

    #[tokio::test]
    async fn a_step_reads_its_environment() {
        let env = BTreeMap::from([("SCORE".to_string(), "s 1".to_string())]);
        let out = run_with(
            &std::env::temp_dir(),
            "echo \"$SCORE\"",
            Duration::from_secs(5),
            RunEnv::default(),
            &env,
        )
        .await
        .unwrap();
        assert_eq!(out, "s 1\n");
    }

    #[tokio::test]
    async fn only_an_uncommitted_runs_hooks_are_told_so() {
        let dir = std::env::temp_dir();
        let said = async |run| {
            super::run(
                &dir,
                "echo \"${SOUNDCHECK_UNCOMMITTED:-unset}\"",
                GUARD_TIMEOUT,
                run,
            )
            .await
            .unwrap()
        };
        assert_eq!(said(RunEnv { uncommitted: true }).await, "1\n");
        assert_eq!(said(RunEnv::default()).await, "unset\n");
    }

    #[tokio::test]
    async fn every_hook_is_told_this_soundcheck() {
        let said = run(
            &std::env::temp_dir(),
            "echo \"$SOUNDCHECK_BIN\"",
            GUARD_TIMEOUT,
            RunEnv::default(),
        )
        .await
        .unwrap();
        let exe = std::env::current_exe().unwrap();
        assert_eq!(said.trim(), exe.to_str().unwrap());
    }

    #[tokio::test]
    async fn a_hook_that_runs_out_of_time_is_told_apart_from_one_that_fails() {
        let dir = std::env::temp_dir();
        let slow = run(
            &dir,
            "sleep 5",
            Duration::from_millis(100),
            RunEnv::default(),
        )
        .await
        .unwrap_err();
        assert!(slow.chain().any(|e| e.is::<TimedOut>()));
        let failing = run(&dir, "exit 1", GUARD_TIMEOUT, RunEnv::default())
            .await
            .unwrap_err();
        assert!(!failing.chain().any(|e| e.is::<TimedOut>()));
    }

    #[tokio::test]
    async fn a_failing_hook_says_why() {
        let dir = std::env::temp_dir();
        let err = run(
            &dir,
            "echo noise >&2; echo 'not localhost' >&2; exit 3",
            GUARD_TIMEOUT,
            RunEnv::default(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not localhost"), "{err}");
        assert_eq!(
            run(&dir, "echo ok", GUARD_TIMEOUT, RunEnv::default())
                .await
                .unwrap()
                .trim(),
            "ok"
        );
        let slow = run(
            &dir,
            "sleep 5",
            Duration::from_millis(100),
            RunEnv::default(),
        )
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
        let service = Service::start(
            &dir,
            &command,
            &url,
            Duration::from_secs(20),
            RunEnv::default(),
        )
        .await
        .unwrap();
        let body = reqwest::get(&url).await.unwrap().text().await.unwrap();
        assert!(body.contains("<h1>app</h1>"));
        drop(service);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(reqwest::get(&url).await.is_err(), "stopped with the run");

        let err = Service::start(
            &dir,
            "exit 4",
            &url,
            Duration::from_secs(5),
            RunEnv::default(),
        )
        .await
        .err()
        .unwrap();
        assert!(err.to_string().contains("stopped"), "{err}");
    }

    #[tokio::test]
    async fn a_stopped_web_app_frees_its_port_for_the_next() {
        let dir = std::env::temp_dir().join(format!("soundcheck-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}/");
        let command = format!("exec python3 -m http.server {port} --bind 127.0.0.1");
        let first = Service::start(
            &dir,
            &command,
            &url,
            Duration::from_secs(20),
            RunEnv::default(),
        )
        .await
        .unwrap();
        first.stop(&url, Duration::from_secs(10)).await.unwrap();
        assert!(reqwest::get(&url).await.is_err(), "gone once stop returns");

        std::fs::write(dir.join("index.html"), "<h1>rebuilt</h1>").unwrap();
        let second = Service::start(
            &dir,
            &command,
            &url,
            Duration::from_secs(20),
            RunEnv::default(),
        )
        .await
        .unwrap();
        let body = reqwest::get(&url).await.unwrap().text().await.unwrap();
        assert!(body.contains("<h1>rebuilt</h1>"));
        drop(second);
    }

    #[tokio::test]
    async fn a_web_app_whose_url_already_answers_is_refused() {
        let dir = std::env::temp_dir().join(format!("soundcheck-taken-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}/");
        let command = format!("exec python3 -m http.server {port} --bind 127.0.0.1");
        let other = Service::start(
            &dir,
            &command,
            &url,
            Duration::from_secs(20),
            RunEnv::default(),
        )
        .await
        .unwrap();

        let ran = dir.join("ran");
        let err = Service::start(
            &dir,
            &format!("touch {}", ran.display()),
            &url,
            Duration::from_secs(5),
            RunEnv::default(),
        )
        .await
        .err()
        .unwrap();
        assert!(err.to_string().contains("already answers"), "{err}");
        assert!(!ran.exists(), "the hook never runs");
        drop(other);
    }
}
