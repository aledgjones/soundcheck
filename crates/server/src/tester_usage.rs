//! The plugin's `SubagentStop` hook: reads a stopped tester's transcript, posts its tokens to the run and says it ended.

use crate::run::event::Spent;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// What the hook posts to `/tokens`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TesterTokens {
    pub token: String,
    pub tokens: u64,
    pub spent: Spent,
}

#[derive(Deserialize)]
struct HookInput {
    #[serde(default)]
    agent_type: String,
    agent_transcript_path: Option<PathBuf>,
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
}

/// The spec token from the tester's first prompt (which `dispatch` wrote), and its usage: the last turn's context
/// and the sum over turns. None for a transcript that isn't a soundcheck tester's.
pub fn read_transcript(jsonl: &str) -> Option<TesterTokens> {
    let mut token = None;
    // One line per content block, each repeating its message's usage, so count each message once.
    let mut order: Vec<String> = Vec::new();
    let mut usage: HashMap<String, Usage> = HashMap::new();
    for line in jsonl.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match entry["type"].as_str() {
            Some("user") if token.is_none() => {
                let content = &entry["message"]["content"];
                let text = content
                    .as_str()
                    .or_else(|| content[0]["text"].as_str())
                    .unwrap_or_default();
                token = spec_token(text);
            }
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
            _ => {}
        }
    }

    let token = token?;
    let turns: Vec<Usage> = order.iter().map(|id| usage[id]).collect();
    let spent = turns.iter().fold(Spent::default(), |s, u| Spent {
        input: s.input + u.input_tokens,
        cache_write: s.cache_write + u.cache_creation_input_tokens,
        cache_read: s.cache_read + u.cache_read_input_tokens,
        output: s.output + u.output_tokens,
    });
    Some(TesterTokens {
        token,
        tokens: turns.last().map_or(0, Usage::total),
        spent,
    })
}

fn spec_token(prompt: &str) -> Option<String> {
    let rest = prompt.strip_prefix("Run soundcheck spec `")?;
    let (token, _) = rest.split_once('`')?;
    Some(token.to_string())
}

/// Reads the hook's input from stdin, posts the tester's tokens to the run serving on `port`, then tells it the tester
/// ended, which queues the spec again if it never posted. Agents other than soundcheck testers are ignored. Its
/// message goes to stderr, which Claude Code keeps out of the conversation.
pub async fn subagent_stop(stdin: &str, port: u16) -> Result<Option<String>> {
    let input: HookInput = serde_json::from_str(stdin).context("hook input isn't JSON")?;
    if !input.agent_type.contains("soundcheck-tester") {
        return Ok(None);
    }

    let path = input
        .agent_transcript_path
        .context("hook input has no agent_transcript_path")?;
    let jsonl =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let Some(tokens) = read_transcript(&jsonl) else {
        anyhow::bail!("{} has no spec token in its first prompt", path.display());
    };

    let counted = post(port, "tokens", &tokens).await;
    // Whether the attempt posted is the server's to judge, so this goes whether or not the count was taken.
    let ended = post(port, "ended", &serde_json::json!({ "token": tokens.token })).await;
    let said: Vec<String> = [counted, ended]
        .into_iter()
        .map(|r| r.unwrap_or_else(|e| format!("{e:#}")))
        .filter(|line| !line.is_empty())
        .collect();
    Ok(Some(said.join("; ")))
}

async fn post(port: u16, route: &str, body: &impl Serialize) -> Result<String> {
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/{route}"))
        .timeout(std::time::Duration::from_secs(3))
        .json(body)
        .send()
        .await
        .with_context(|| format!("no run serving on port {port}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("the run refused /{route}: {}", text.trim());
    }

    Ok(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(id: &str, input: u64, write: u64, read: u64, output: u64) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {"id": id, "usage": {
                "input_tokens": input,
                "cache_creation_input_tokens": write,
                "cache_read_input_tokens": read,
                "output_tokens": output,
            }},
        })
        .to_string()
    }

    #[test]
    fn sums_each_message_once_and_keeps_the_last_turn() {
        let prompt = serde_json::json!({
            "type": "user",
            "message": {"content": "Run soundcheck spec `help/001-read-help#1@ios`: call `brief` with this token first"},
        });
        let later = serde_json::json!({"type": "user", "message": {"content": "Run soundcheck spec `other#1@ios`"}});
        let lines = [
            prompt.to_string(),
            assistant("m1", 2, 13_000, 0, 100),
            assistant("m1", 2, 13_000, 0, 100),
            later.to_string(),
            "not json".into(),
            assistant("m2", 1, 500, 13_000, 50),
        ];
        let t = read_transcript(&lines.join("\n")).unwrap();
        assert_eq!(t.token, "help/001-read-help#1@ios");
        assert_eq!(t.tokens, 1 + 500 + 13_000 + 50);
        assert_eq!(
            t.spent,
            Spent {
                input: 3,
                cache_write: 13_500,
                cache_read: 13_000,
                output: 150,
            }
        );
    }

    #[test]
    fn other_agents_have_no_token() {
        let prompt = serde_json::json!({"type": "user", "message": {"content": [{"type": "text", "text": "Reply ok."}]}});
        assert_eq!(read_transcript(&prompt.to_string()), None);
    }
}
