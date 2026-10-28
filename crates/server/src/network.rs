//! The suite's network proxy: started on demand and left running, like the headless emulator, so the app reaches its
//! services between runs too.

use crate::suite::config::{Platform, SuiteConfig};
use anyhow::{Context, Result, bail};
use soundcheck_driver::proxy::{self, Network, Route};
use std::path::Path;
use std::time::Duration;

/// The routes `platform`'s app goes through, or none when it has no proxy.
pub fn routes(config: &SuiteConfig, platform: Platform) -> Option<Vec<Route>> {
    let routes: Vec<Route> = config
        .network
        .as_ref()?
        .proxy
        .routes(platform)
        .iter()
        .map(|r| Route {
            name: r.name.clone(),
            listen: r.listen,
            upstream: r.upstream.clone(),
        })
        .collect();
    (!routes.is_empty()).then_some(routes)
}

/// Android's forwards for `platform`'s routes, as (device port, port on this machine).
pub fn forwards(config: &SuiteConfig, platform: Platform) -> Vec<(u16, u16)> {
    config.network.as_ref().map_or_else(Vec::new, |n| {
        n.proxy
            .routes(platform)
            .iter()
            .map(|r| (r.device_port(), r.listen))
            .collect()
    })
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

/// The running proxy for `platform`'s routes, starting it detached when it isn't up, or replacing it when it's stale.
pub async fn ensure(suite_root: &Path, platform: Platform, routes: &[Route]) -> Result<Network> {
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
            .args([
                "proxy",
                "serve",
                "--platform",
                &platform.to_string(),
                "--suite",
            ])
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
        "the network proxy didn't start: run `soundcheck proxy serve --platform {platform} --suite {}` to see why",
        suite_root.display()
    )
}
