//! The suite's network proxy: started on demand and left running, like the headless emulator, so the app reaches its
//! services between runs too.

use crate::suite::config::SuiteConfig;
use anyhow::{Context, Result, bail};
use soundcheck_driver::proxy::{self, Network, Route};
use std::path::Path;
use std::time::Duration;

pub fn routes(config: &SuiteConfig) -> Option<Vec<Route>> {
    let network = config.network.as_ref()?;
    Some(
        network
            .proxy
            .iter()
            .map(|r| Route {
                name: r.name.clone(),
                listen: r.listen,
                upstream: r.upstream,
            })
            .collect(),
    )
}

/// A proxy started before this soundcheck was built, or too old to say when it started.
fn stale(started: &str, exe: &Path) -> bool {
    let built = std::fs::metadata(exe)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    started
        .parse::<u64>()
        .map_or(true, |started| started < built)
}

/// The running proxy for these routes, starting it detached when it isn't up, or replacing it when it's stale.
pub async fn ensure(suite_root: &Path, routes: &[Route]) -> Result<Network> {
    let socket = proxy::socket_path(routes);
    let exe = std::env::current_exe().context("finding soundcheck to start the proxy")?;
    if let Ok(started) = proxy::command(&socket, "started").await {
        if !stale(&started, &exe) {
            return Ok(Network { socket });
        }

        // It outlives installs, so one started before this build may lack its commands.
        let _ = proxy::command(&socket, "stop").await;
        for _ in 0..50 {
            if proxy::command(&socket, "status").await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so it outlives the run or drive command that started it.
        std::process::Command::new(exe)
            .args(["proxy", "serve", "--suite"])
            .arg(
                suite_root
                    .canonicalize()
                    .unwrap_or_else(|_| suite_root.into()),
            )
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .context("starting the network proxy")?;
    }
    for _ in 0..50 {
        if proxy::command(&socket, "status").await.is_ok() {
            return Ok(Network { socket });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "the network proxy didn't start: run `soundcheck proxy serve --suite {}` to see why",
        suite_root.display()
    )
}
