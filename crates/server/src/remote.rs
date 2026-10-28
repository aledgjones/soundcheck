//! `--remote`: the live page through an ngrok tunnel, behind Google sign-in and read-only.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;

/// The page's own GETs. The same port serves `/mcp`, `/dispatch` and `/next`, which must never leave the machine.
const PAGE_PATHS: &[&str] = &[
    "/",
    "/fragment",
    "/events",
    "/download",
    "/manifest.webmanifest",
    "/icon-180.png",
    "/icon-512.png",
];
const PAGE_PREFIXES: &[&str] = &["/files/evidence/", "/runs"];

/// A running tunnel; dropping it stops ngrok.
pub struct Tunnel {
    pub url: String,
    _child: Child,
}

/// The traffic policy: sign in with Google, only `allow` gets past it, then only the page's GETs.
pub fn policy(allow: &[String], port: u16) -> Result<String> {
    if allow.is_empty() {
        bail!("suite.yaml's `remote.allow` names nobody");
    }
    if let Some(bad) = allow.iter().find(|e| !e.contains('@') || e.contains('\'')) {
        bail!("`{bad}` in `remote.allow` isn't an email");
    }
    let list = |items: &[&str]| {
        items
            .iter()
            .map(|i| format!("'{i}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let emails: Vec<&str> = allow.iter().map(String::as_str).collect();
    let prefixes = PAGE_PREFIXES
        .iter()
        .map(|p| format!(" || req.url.path.startsWith('{p}')"))
        .collect::<String>();
    Ok(format!(
        "on_http_request:
  - actions:
      - type: oauth
        config:
          provider: google
  - expressions:
      - \"!(actions.ngrok.oauth.identity.email in [{emails}])\"
    actions:
      - type: deny
        config:
          status_code: 403
  - expressions:
      - \"!(req.method in ['GET', 'HEAD'])\"
    actions:
      - type: deny
        config:
          status_code: 403
  - expressions:
      - \"!(req.url.path in [{paths}]{prefixes})\"
    actions:
      - type: deny
        config:
          status_code: 403
  # The server answers only its own Host with no foreign Origin (serve::is_local).
  - actions:
      - type: remove-headers
        config:
          headers: [origin]
      - type: add-headers
        config:
          headers:
            host: \"127.0.0.1:{port}\"
",
        emails = list(&emails),
        paths = list(PAGE_PATHS),
    ))
}

/// Starts ngrok for the server on `port` and waits for its public URL. The policy file goes in `dir`.
pub async fn start(port: u16, allow: &[String], dir: &Path) -> Result<Tunnel> {
    let policy_file = dir.join("remote-policy.yml");
    std::fs::write(&policy_file, policy(allow, port)?)
        .with_context(|| format!("writing {}", policy_file.display()))?;
    let mut child = tokio::process::Command::new("ngrok")
        .args([
            "http",
            &format!("127.0.0.1:{port}"),
            "--traffic-policy-file",
        ])
        .arg(&policy_file)
        .args(["--log", "stdout", "--log-format", "json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("couldn't start ngrok; install it with `brew install ngrok`")?;
    let mut lines = BufReader::new(child.stdout.take().context("ngrok's output")?).lines();
    let found = tokio::time::timeout(Duration::from_secs(30), async {
        let mut last_error = None;
        while let Some(line) = lines.next_line().await? {
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if entry["msg"] == "started tunnel"
                && let Some(url) = entry["url"].as_str()
            {
                return Ok(url.to_string());
            }
            if entry["lvl"] == "eror" || entry["lvl"] == "crit" {
                last_error = entry["err"].as_str().map(str::to_string);
            }
        }
        bail!(
            "ngrok stopped: {}",
            last_error.unwrap_or_else(|| "no reason given; is its authtoken set?".into())
        )
    })
    .await
    .context("ngrok didn't open a tunnel within 30 s")??;
    // ngrok blocks once its log pipe fills, so keep reading it.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    Ok(Tunnel {
        url: found,
        _child: child,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_signs_in_then_lets_only_the_page_through() {
        let policy = policy(&["a@example.com".into()], 7357).unwrap();
        assert!(policy.find("type: oauth") < policy.find("in ['a@example.com']"));
        assert!(policy.contains("'/fragment'"));
        for control in [
            "/mcp",
            "/dispatch",
            "/next",
            "/tokens",
            "/ended",
            "/step-down",
        ] {
            assert!(
                !policy.contains(&format!("'{control}")),
                "{control} gets through"
            );
        }
    }

    #[test]
    fn the_policy_refuses_an_empty_or_odd_allow_list() {
        assert!(policy(&[], 7357).is_err());
        assert!(policy(&["a@b.com' || true || '".into()], 7357).is_err());
        assert!(policy(&["nobody".into()], 7357).is_err());
    }
}
