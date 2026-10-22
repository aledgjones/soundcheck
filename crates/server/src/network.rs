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

/// The running proxy for these routes, starting it detached when it isn't up.
pub async fn ensure(suite_root: &Path, routes: &[Route]) -> Result<Network> {
    let socket = proxy::socket_path(routes);
    if proxy::command(&socket, "status").await.is_ok() {
        return Ok(Network { socket });
    }
    let exe = std::env::current_exe().context("finding soundcheck to start the proxy")?;
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
