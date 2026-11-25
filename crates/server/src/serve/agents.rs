//! The agents the server launches itself (SA-1049, SA-1050): each a headless `claude -p` attached straight to this
//! server's MCP URL, with only its own tools and none of the project's settings, memory or plugins. A tester runs one
//! attempt; a reviewer settles one row's claim and side finds. Each one's output stream is kept in the run folder,
//! and its usage is read from it.

use super::Hub;
use crate::run::event::Spent;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const TESTER: &str = "soundcheck-tester";
pub const MECHANICS_TESTER: &str = "soundcheck-tester-mechanics";
pub const REVIEWER: &str = "soundcheck-reviewer";

/// The run folder's subfolder for the agents' definitions and each one's output stream.
pub const AGENTS_DIR: &str = "agents";

/// The name the server's tools go by in an agent it launches: `mcp__soundcheck__<tool>`.
const MCP_NAME: &str = "soundcheck";

/// Names the program to launch instead of `claude` (the server's own tests).
pub const CLAUDE_ENV: &str = "SOUNDCHECK_CLAUDE";

/// How long an agent has to end by itself once its work is settled, before it's stopped.
const WIND_DOWN: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_secs(2);

const TESTER_PROMPT: &str = include_str!("tester.md");
const REVIEWER_PROMPT: &str = include_str!("reviewer.md");

/// The tester for a spec. Only specs that list driver tools get the one with the mechanics: their schemas are sent on
/// every turn, and most specs never use them.
pub fn tester_for(spec: &crate::suite::spec::Spec) -> &'static str {
    if spec.front.tools.is_empty() {
        TESTER
    } else {
        MECHANICS_TESTER
    }
}

fn mcp(tool: &str) -> String {
    format!("mcp__{MCP_NAME}__{tool}")
}

/// A tester's tools: what posts its result, and the driver's tools, the mechanics only for a spec that lists them
/// (their schemas ride along on every turn).
fn tester_tools(mechanics: bool) -> Vec<String> {
    let mut tools = vec!["Read".to_string(), mcp("judge"), mcp("post_result")];
    tools.extend(
        soundcheck_driver::TOOLS
            .iter()
            .filter(|t| mechanics || t.kind == soundcheck_driver::Kind::Basic)
            .map(|t| mcp(&t.mcp_name())),
    );
    tools
}

fn reviewer_tools() -> Vec<String> {
    vec![
        "Read".to_string(),
        "Grep".to_string(),
        "Glob".to_string(),
        mcp("status"),
        mcp("review"),
        mcp("review_find"),
    ]
}

fn tools_of(agent: &str) -> Vec<String> {
    match agent {
        REVIEWER => reviewer_tools(),
        MECHANICS_TESTER => tester_tools(true),
        _ => tester_tools(false),
    }
}

/// The agents file `claude --agents` reads: each agent's instructions and the only tools it has.
pub fn definitions() -> serde_json::Value {
    let tester = format!("{TESTER_PROMPT}\n{}", super::prompt::CONTRACT);
    json!({
        TESTER: {
            "description": "Runs one soundcheck spec on one device and posts its result.",
            "prompt": tester,
            "tools": tester_tools(false),
        },
        MECHANICS_TESTER: {
            "description": "Runs one soundcheck spec that lists driver tools on one device and posts its result.",
            "prompt": tester,
            "tools": tester_tools(true),
        },
        REVIEWER: {
            "description": "Settles one soundcheck claim, and its row's side finds, from the evidence.",
            "prompt": REVIEWER_PROMPT,
            "tools": reviewer_tools(),
        },
    })
}

/// An agent to launch.
pub struct Launch<'a> {
    pub agent: &'a str,
    pub model: Option<&'a str>,
    pub prompt: &'a str,
    /// Names its output stream in the run folder.
    pub name: &'a str,
    /// Where it starts: what its relative paths and searches are read against.
    pub cwd: &'a Path,
}

/// How a launched agent ended.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Ended {
    /// Why it couldn't run or stopped in error: Claude not found, a usage limit, a model it doesn't have.
    pub error: Option<String>,
    /// Its context on its last turn, and what its turns added up to; None when it never got a reply.
    pub usage: Option<(u64, Spent)>,
}

fn file_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The program that runs an agent: `claude` on the PATH, unless [`CLAUDE_ENV`] names another.
pub fn program() -> String {
    std::env::var(CLAUDE_ENV).unwrap_or_else(|_| "claude".to_string())
}

/// Whether agents can be launched at all, asked before a run starts: Claude's version, or why it won't run.
pub async fn check() -> Result<String> {
    let program = program();
    let output = tokio::process::Command::new(&program)
        .arg("--version")
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(20), output)
        .await
        .map_err(|_| anyhow::anyhow!("`{program} --version` didn't answer in 20 s"))?
        .with_context(|| {
            format!("couldn't run `{program}`: soundcheck launches its testers with Claude Code, so it must be on the PATH")
        })?;

    if !output.status.success() {
        anyhow::bail!(
            "`{program} --version` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

impl Hub {
    fn agents_dir(&self) -> PathBuf {
        self.run_dir.join(AGENTS_DIR)
    }

    /// Launches an agent and waits for it to end. Once `settled` says its work is done (its attempt posted, stalled
    /// or was abandoned; its claim was reviewed) it gets a minute to finish by itself, then it's stopped, as it is
    /// when the server stops.
    pub async fn launch(self: &Arc<Self>, launch: Launch<'_>, settled: impl Fn() -> bool) -> Ended {
        match self.launch_inner(launch, settled).await {
            Ok(ended) => ended,
            Err(e) => Ended {
                error: Some(format!("{e:#}")),
                usage: None,
            },
        }
    }

    async fn launch_inner(
        self: &Arc<Self>,
        launch: Launch<'_>,
        settled: impl Fn() -> bool,
    ) -> Result<Ended> {
        let url = self
            .url
            .get()
            .context("the server has no address yet, so no agent can reach it")?;
        let dir = self.agents_dir();
        std::fs::create_dir_all(&dir)?;
        let agents = dir.join("agents.json");
        // Written for each launch, so an agent started after an upgrade mid-run gets this version's.
        std::fs::write(&agents, serde_json::to_string_pretty(&definitions())?)?;
        let stream = dir.join(format!("{}.jsonl", file_name(launch.name)));
        let errors = dir.join(format!("{}.err", file_name(launch.name)));
        let servers =
            json!({ "mcpServers": { MCP_NAME: { "type": "http", "url": format!("{url}/mcp") } } });
        let program = program();
        let mut command = tokio::process::Command::new(&program);
        command
            .arg("--print")
            .arg("--agents")
            .arg(&agents)
            .args(["--agent", launch.agent])
            .args(["--mcp-config", &servers.to_string()])
            .arg("--strict-mcp-config")
            // Nothing of the person's or the project's: no settings, hooks, plugins, memory or CLAUDE.md.
            .args(["--setting-sources", ""])
            .args(["--allowedTools", &tools_of(launch.agent).join(",")])
            .arg("--disable-slash-commands")
            .arg("--no-session-persistence")
            .args(["--output-format", "stream-json"])
            .arg("--verbose");

        if let Some(model) = launch.model {
            command.args(["--model", model]);
        }

        let mut child = command
            .current_dir(launch.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::fs::File::create(&stream)?)
            .stderr(std::fs::File::create(&errors)?)
            // In a group of its own, so a Ctrl-C at the server's terminal reaches only the server, which then stops
            // its agents itself and doesn't take their ending for a failure.
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!("couldn't run `{program}`: soundcheck launches its agents with Claude Code, so it must be on the PATH")
            })?;

        // The prompt goes in on stdin: a brief is long, and may start with anything.
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            // A Claude that ends before it reads its prompt closes the pipe: what it said on the way out is the
            // error worth reporting, not the write that failed.
            let _ = stdin.write_all(launch.prompt.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }

        let mut settled_at = None;
        let status = loop {
            tokio::select! {
                status = child.wait() => break Some(status?),
                _ = self.stop.cancelled() => {
                    let _ = child.kill().await;
                    break None;
                }
                _ = tokio::time::sleep(POLL) => {
                    if settled_at.is_none() && settled() {
                        settled_at = Some(std::time::Instant::now());
                    }

                    if settled_at.is_some_and(|at| at.elapsed() >= WIND_DOWN) {
                        let _ = child.kill().await;
                        break None;
                    }
                }
            }
        };
        let read = read_stream(&std::fs::read_to_string(&stream).unwrap_or_default());
        // An agent this server stopped didn't fail; one that ended in error did, whatever it managed first.
        let failed = status.is_some_and(|s| !s.success()) || read.error.is_some();
        let error = failed.then(|| {
            read.error.clone().unwrap_or_else(|| {
                let said = std::fs::read_to_string(&errors).unwrap_or_default();
                let said = said.trim();
                let last = said.lines().last().unwrap_or("it said nothing");
                format!("`{program}` ended in error: {last}")
            })
        });

        if error.is_none() {
            let _ = std::fs::remove_file(&errors);
        }

        Ok(Ended {
            error,
            usage: read.usage,
        })
    }
}

#[derive(Deserialize, Default, Clone, Copy)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

impl Usage {
    fn total(&self) -> u64 {
        self.input_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
            + self.output_tokens
    }

    fn spent(&self) -> Spent {
        Spent {
            input: self.input_tokens,
            cache_write: self.cache_creation_input_tokens,
            cache_read: self.cache_read_input_tokens,
            output: self.output_tokens,
        }
    }
}

/// What an agent's output stream says of it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stream {
    pub usage: Option<(u64, Spent)>,
    /// Its closing line reported an error, with what it said.
    pub error: Option<String>,
}

/// Reads `claude --output-format stream-json`: the last turn's context from its messages, and the total from its
/// closing `result` line (the messages' own sum when it was stopped before one).
pub fn read_stream(jsonl: &str) -> Stream {
    // One line per content block, each repeating its message's usage, so count each message once.
    let mut order: Vec<String> = Vec::new();
    let mut usage: HashMap<String, Usage> = HashMap::new();
    let mut total = None;
    let mut error = None;
    for line in jsonl.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match entry["type"].as_str() {
            Some("assistant") => {
                let message = &entry["message"];
                let (Some(id), Ok(u)) = (
                    message["id"].as_str(),
                    serde_json::from_value::<Usage>(message["usage"].clone()),
                ) else {
                    continue;
                };

                if !usage.contains_key(id) {
                    order.push(id.to_string());
                }

                usage.insert(id.to_string(), u);
            }
            Some("result") => {
                total = serde_json::from_value::<Usage>(entry["usage"].clone()).ok();

                if entry["is_error"].as_bool() == Some(true) {
                    let said = entry["result"]
                        .as_str()
                        .or_else(|| entry["subtype"].as_str())
                        .unwrap_or("it ended in error");
                    error = Some(said.trim().to_string());
                }
            }
            _ => {}
        }
    }

    let turns: Vec<Usage> = order.iter().map(|id| usage[id]).collect();
    let summed = turns.iter().fold(Spent::default(), |s, u| Spent {
        input: s.input + u.input_tokens,
        cache_write: s.cache_write + u.cache_creation_input_tokens,
        cache_read: s.cache_read + u.cache_read_input_tokens,
        output: s.output + u.output_tokens,
    });
    let usage = turns.last().map(|last| {
        (
            last.total(),
            total
                .filter(|t| t.total() > 0)
                .map_or(summed, |t| t.spent()),
        )
    });
    Stream { usage, error }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_gives_the_last_context_and_the_total() {
        let stream = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"id":"a","usage":{"input_tokens":2,"cache_creation_input_tokens":2000,"cache_read_input_tokens":0,"output_tokens":10}}}
{"type":"assistant","message":{"id":"a","usage":{"input_tokens":2,"cache_creation_input_tokens":2000,"cache_read_input_tokens":0,"output_tokens":10}}}
{"type":"user"}
{"type":"assistant","message":{"id":"b","usage":{"input_tokens":2,"cache_creation_input_tokens":200,"cache_read_input_tokens":2000,"output_tokens":3}}}
{"type":"result","is_error":false,"result":"done","usage":{"input_tokens":4,"cache_creation_input_tokens":2200,"cache_read_input_tokens":2000,"output_tokens":300}}"#;
        let read = read_stream(stream);
        assert_eq!(read.error, None);
        assert_eq!(
            read.usage,
            Some((
                2205,
                Spent {
                    input: 4,
                    cache_write: 2200,
                    cache_read: 2000,
                    output: 300
                }
            ))
        );
    }

    #[test]
    fn a_stream_stopped_early_sums_its_messages() {
        let stream = r#"{"type":"assistant","message":{"id":"a","usage":{"input_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":0,"output_tokens":10}}}
{"type":"assistant","message":{"id":"b","usage":{"input_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":100,"output_tokens":5}}}"#;
        let (context, spent) = read_stream(stream).usage.unwrap();
        assert_eq!(context, 107);
        assert_eq!(spent.cache_read, 100);
        assert_eq!(spent.output, 15);
    }

    #[test]
    fn a_closing_error_is_kept_with_what_it_said() {
        let stream = r#"{"type":"result","is_error":true,"result":"Claude usage limit reached. Your limit will reset at 3am."}"#;
        let read = read_stream(stream);
        assert_eq!(
            read.error.as_deref(),
            Some("Claude usage limit reached. Your limit will reset at 3am.")
        );
        assert_eq!(read.usage, None);
    }

    #[test]
    fn only_a_mechanics_tester_gets_the_mechanics() {
        let plain = tester_tools(false);
        let mechanics = tester_tools(true);
        for tool in soundcheck_driver::TOOLS {
            let name = mcp(&tool.mcp_name());
            assert!(mechanics.contains(&name), "{name}");
            assert_eq!(
                plain.contains(&name),
                tool.kind == soundcheck_driver::Kind::Basic,
                "{name}"
            );
        }
        assert!(plain.contains(&mcp("post_result")) && plain.contains(&mcp("judge")));
        assert!(
            !plain.iter().any(|t| t.contains("review")),
            "testers never review"
        );
        assert!(
            !reviewer_tools().iter().any(|t| t.contains("post_result")),
            "reviewers never post"
        );
    }

    #[test]
    fn a_stream_is_named_for_its_agent_without_path_characters() {
        assert_eq!(
            file_name("annotations/001-draw#1@ios"),
            "annotations-001-draw-1-ios"
        );
    }
}
