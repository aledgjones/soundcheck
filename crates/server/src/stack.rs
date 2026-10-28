//! Each platform's scratch backend (suite.yaml `hooks.stack-up` and `stack-down`). A run starts a blank one per
//! platform and removes it when its server stops. A record per stack names the server that owns it, so a stack whose
//! server died (killed, crashed, the Mac restarted) is removed by the next run, `serve` or `soundcheck stack down`.

use crate::serve::hooks;
use crate::suite::{self, config::Platform, config::SuiteConfig};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const UP_TIMEOUT: Duration = Duration::from_secs(300);
pub const DOWN_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a server stepping down for a new run gets to remove its stacks, since it frees the port first.
const OWNER_EXIT_WAIT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub platform: Platform,
    pub pid: u32,
    /// The owner's start time as `ps` gives it, so a reused pid isn't taken for the owner.
    pub started: String,
    /// The suite folder and the command that removes the stack, as they were when it started.
    pub suite: PathBuf,
    pub down: String,
}

impl Record {
    pub fn owner_alive(&self) -> bool {
        started_at(self.pid).as_deref() == Some(self.started.as_str())
    }
}

fn started_at(pid: u32) -> Option<String> {
    // `lstart` follows the locale and time zone, which differ between shells; the record must read the same in all.
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .output()
        .ok()?;
    let started = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !started.is_empty()).then_some(started)
}

pub fn records_dir(root: &Path, config: &SuiteConfig) -> PathBuf {
    suite::reports_dir(root, config).join(".stacks")
}

pub fn records(dir: &Path) -> Vec<(PathBuf, Record)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, Record)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let record = serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
            Some((p, record))
        })
        .collect();
    found.sort_by_key(|(p, _)| p.clone());
    found
}

/// What [`reap`] does with a stack whose server is still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Live {
    /// A new run starting these platforms: give their owner [`OWNER_EXIT_WAIT`] to stop (it's stepping down for
    /// this run), and leave other platforms' stacks to the server running them.
    Wait(Vec<Platform>),
    /// Refuse: removing it by hand would pull the backend from under a run.
    Refuse,
    /// Leave it: `serve` only cleans up after servers that are gone.
    Skip,
}

/// Removes every stack whose server is gone and says which.
pub async fn reap(dir: &Path, live: Live) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for (path, record) in records(dir) {
        let leave = match &live {
            Live::Skip => true,
            Live::Wait(starting) => !starting.contains(&record.platform),
            Live::Refuse => false,
        };
        if leave && record.owner_alive() {
            continue;
        }
        let wait = if matches!(live, Live::Wait(_)) {
            OWNER_EXIT_WAIT
        } else {
            Duration::ZERO
        };
        let deadline = tokio::time::Instant::now() + wait;
        while record.owner_alive() {
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "the {} stack belongs to soundcheck (pid {}), which is still running; stop that server first",
                    record.platform,
                    record.pid
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        // An owner that stepped down removes its own record on the way out.
        if !path.exists() {
            continue;
        }
        hooks::run(&record.suite, &record.down, DOWN_TIMEOUT)
            .await
            .with_context(|| {
                format!(
                    "removing the {} stack a stopped server left",
                    record.platform
                )
            })?;
        let _ = std::fs::remove_file(&path);
        removed.push(format!(
            "{}: removed the stack a stopped server left",
            record.platform
        ));
    }
    Ok(removed)
}

/// A platform's stack for as long as this server runs. Dropping it removes the stack, on every way out the server
/// has: the run stopped, a step-down, a failed preflight.
pub struct Stack {
    record: Record,
    path: PathBuf,
}

/// Starts `platform`'s blank stack, or nothing when the suite has no stack hooks.
pub async fn up(root: &Path, config: &SuiteConfig, platform: Platform) -> Result<Option<Stack>> {
    let (Some(up), Some(down)) = (&config.hooks.stack_up, &config.hooks.stack_down) else {
        return Ok(None);
    };
    start(root, &records_dir(root, config), platform, up, down)
        .await
        .map(Some)
}

async fn start(root: &Path, dir: &Path, platform: Platform, up: &str, down: &str) -> Result<Stack> {
    std::fs::create_dir_all(dir)?;
    let pid = std::process::id();
    let record = Record {
        platform,
        pid,
        started: started_at(pid).context("reading this server's start time")?,
        suite: std::fs::canonicalize(root)?,
        down: hooks::fill(down, "", &platform.to_string()),
    };
    let path = dir.join(format!("{platform}.json"));
    // Written first, so a server killed while the stack starts still leaves it to the next one to remove.
    std::fs::write(&path, serde_json::to_string_pretty(&record)?)?;
    let stack = Stack { record, path };
    hooks::run(
        root,
        &hooks::fill(up, "", &platform.to_string()),
        UP_TIMEOUT,
    )
    .await
    .with_context(|| format!("{platform}: the stack didn't start"))?;
    Ok(stack)
}

/// Runs `down` to the end, or gives up after [`DOWN_TIMEOUT`] (a hung Docker), so the server still exits.
fn remove(record: &Record) -> bool {
    let Ok(mut child) = std::process::Command::new("sh")
        .args(["-c", &record.down])
        .current_dir(&record.suite)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = std::time::Instant::now() + DOWN_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            _ => {
                let _ = child.kill();
                return false;
            }
        }
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        // Blocking, because the server is on its way out and the stack must be gone before it is.
        if !remove(&self.record) {
            // The record stays, so the next run or `soundcheck stack down` tries again. Written, not printed: after
            // a hangup the terminal is gone, and a failed print would panic here.
            let _ = writeln!(
                std::io::stderr(),
                "soundcheck: couldn't remove the {} stack; `soundcheck stack down` tries again",
                self.record.platform
            );
            return;
        }
        // A new run may already have written its own record for this platform.
        let ours = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str::<Record>(&s).ok())
            .is_some_and(|r| r.pid == self.record.pid && r.started == self.record.started);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("soundcheck-stack-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record(dir: &Path, started: String) -> PathBuf {
        let path = dir.join("records/ios.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let record = Record {
            platform: Platform::Ios,
            pid: std::process::id(),
            started,
            suite: dir.to_path_buf(),
            down: "touch down-ran".into(),
        };
        std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
        path
    }

    #[tokio::test]
    async fn a_stack_is_removed_when_dropped_and_its_record_with_it() {
        let dir = scratch("drop");
        let records = dir.join("records");
        let stack = start(&dir, &records, Platform::Ios, "true", "touch down-ran")
            .await
            .unwrap();
        assert!(records.join("ios.json").exists());
        drop(stack);
        assert!(dir.join("down-ran").exists());
        assert!(!records.join("ios.json").exists());
    }

    #[tokio::test]
    async fn a_stack_that_fails_to_start_is_removed_and_says_why() {
        let dir = scratch("fail");
        let records = dir.join("records");
        let error = start(
            &dir,
            &records,
            Platform::Android,
            "echo port taken >&2; false",
            "touch down-ran",
        )
        .await
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("android: the stack didn't start"));
        assert!(format!("{error:#}").contains("port taken"));
        assert!(dir.join("down-ran").exists());
        assert!(!records.join("android.json").exists());
    }

    #[tokio::test]
    async fn a_new_servers_record_survives_the_old_one_stepping_down() {
        let dir = scratch("handover");
        let records = dir.join("records");
        let old = start(&dir, &records, Platform::Ios, "true", "true")
            .await
            .unwrap();
        let newer = record(&dir, "a later server".into());
        drop(old);
        assert!(newer.exists());
    }

    #[tokio::test]
    async fn a_stack_whose_server_is_gone_is_removed() {
        let dir = scratch("reap");
        let path = record(&dir, "not when this process started".into());
        let removed = reap(&dir.join("records"), Live::Refuse).await.unwrap();
        assert_eq!(removed, ["ios: removed the stack a stopped server left"]);
        assert!(dir.join("down-ran").exists());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_stack_whose_server_runs_is_left_or_refused() {
        let dir = scratch("live");
        let path = record(&dir, started_at(std::process::id()).unwrap());
        let records = dir.join("records");
        assert!(reap(&records, Live::Skip).await.unwrap().is_empty());
        let error = reap(&records, Live::Refuse).await.err().unwrap();
        assert!(error.to_string().contains("which is still running"));
        assert!(path.exists());
        assert!(!dir.join("down-ran").exists());
    }

    #[tokio::test]
    async fn a_new_run_leaves_a_running_servers_other_platforms() {
        let dir = scratch("other");
        let path = record(&dir, started_at(std::process::id()).unwrap());
        let starting = Live::Wait(vec![Platform::Android]);
        assert!(
            reap(&dir.join("records"), starting)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(path.exists());
    }
}
