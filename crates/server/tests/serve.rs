use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use soundcheck::run::{Plan, PlannedSpec, Run};
use soundcheck::serve::{self, Hub};
use soundcheck::suite;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

async fn server() -> (serve::Server, String) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/good");
    let suite = suite::load(&root);
    let plan = Plan {
        specs: suite
            .specs
            .iter()
            .map(|s| PlannedSpec {
                id: s.id.clone(),
                platforms: s.front.platforms.clone(),
                outcomes: s.outcomes.clone(),
            })
            .collect(),
        parallel: false,
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soundcheck-serve-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let run = Run::open(plan, &dir.join("journal.jsonl"), serve::now_ms()).unwrap();
    let server = serve::start(Hub::new(suite, run, 120_000), 0)
        .await
        .unwrap();
    let url = format!("http://{}", server.addr);
    (server, url)
}

async fn client(url: &str) -> RunningService<RoleClient, ClientConfig> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{url}/mcp")),
    );
    ClientConfig::default().serve(transport).await.unwrap()
}

/// Calls a tool and returns its JSON reply, or the refusal text as a string.
async fn call(
    client: &RunningService<RoleClient, ClientConfig>,
    tool: &'static str,
    args: Value,
) -> Value {
    let mut params = CallToolRequestParams::new(tool);
    if let Value::Object(map) = args {
        params = params.with_arguments(map);
    }
    let result = client.call_tool(params).await.unwrap();
    let text = result.content[0].as_text().unwrap().text.clone();
    if result.is_error == Some(true) {
        Value::String(text)
    } else {
        serde_json::from_str(&text).unwrap()
    }
}

#[tokio::test]
async fn only_this_machines_own_callers_are_answered() {
    let (server, url) = server().await;
    let port = server.addr.port();
    let get = |origin: Option<String>, host: Option<String>| {
        let mut request = reqwest::Client::new().get(format!("{url}/events"));
        if let Some(origin) = origin {
            request = request.header("Origin", origin);
        }
        if let Some(host) = host {
            request = request.header("Host", host);
        }
        request.send()
    };
    let status = |r: reqwest::Response| r.status().as_u16();
    assert_eq!(
        status(get(None, None).await.unwrap()),
        200,
        "agents and the CLI send no Origin"
    );
    assert_eq!(
        status(
            get(Some(format!("http://127.0.0.1:{port}")), None)
                .await
                .unwrap()
        ),
        200,
        "the live page"
    );
    assert_eq!(
        status(
            get(Some("https://evil.example".into()), None)
                .await
                .unwrap()
        ),
        403,
        "another site"
    );
    assert_eq!(
        status(
            get(None, Some(format!("evil.example:{port}")))
                .await
                .unwrap()
        ),
        403,
        "a rebound domain"
    );
    server.shutdown.cancel();
}

#[tokio::test]
async fn a_spec_runs_end_to_end_over_mcp() {
    let (server, url) = server().await;
    let client = client(&url).await;

    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
    for want in [
        "post_result",
        "status",
        "dispatch",
        "review",
        "review_find",
        "install_build",
        "triage",
        "pause",
        "resume",
        "finish",
    ] {
        assert!(
            names.contains(&want.to_string()),
            "missing tool {want}: {names:?}"
        );
    }

    let dispatched = call(
        &client,
        "dispatch",
        json!({ "spec": "annotations/001-draw", "platform": "android" }),
    )
    .await;
    let token = dispatched["token"].as_str().unwrap().to_string();
    let brief = dispatched["brief"].as_str().unwrap();
    assert!(brief.contains(&token));
    assert!(brief.contains("0. The stroke disappears on undo"));
    assert!(
        brief.contains("Open a score, then tap Annotate."),
        "feature context is in the brief"
    );

    let refused = call(
        &client,
        "dispatch",
        json!({ "spec": "annotations/001-draw", "platform": "ios" }),
    )
    .await;
    assert!(
        refused
            .as_str()
            .unwrap()
            .starts_with("refused: annotations/001-draw is still running"),
        "{refused}"
    );

    let posted = call(
        &client,
        "post_result",
        json!({
            "token": token,
            "judgments": [
                { "outcome": 0, "answer": "failed", "observed": "the stroke stayed after undo" },
                { "outcome": 1, "answer": "held", "observed": "Undo greyed out after the undo" }
            ]
        }),
    )
    .await;
    assert_eq!(posted["claim"], "FAIL");

    let status = call(&client, "status", json!({})).await;
    assert_eq!(status["verdict"]["status"], "not-ready");
    assert!(
        status["finish_blockers"][0]
            .as_str()
            .unwrap()
            .contains("needs review")
    );

    let reviewed = call(
        &client,
        "review",
        json!({
            "spec": "annotations/001-draw",
            "platform": "android",
            "call": { "call": "uphold", "severity": "bug" },
            "reason": "screenshot shows the stroke after undo"
        }),
    )
    .await;
    assert_eq!(reviewed["reviewed"], "annotations/001-draw");
    let status = call(&client, "status", json!({})).await;
    assert_eq!(status["verdict"]["status"], "blocked");
    assert_eq!(
        status["problems"][0]["summary"],
        "The stroke disappears on undo and returns in the same place on redo: the stroke stayed after undo"
    );

    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn the_event_stream_replays_then_follows() {
    let (server, url) = server().await;
    let client = client(&url).await;
    call(&client, "pause", json!({ "reason": "first" })).await;
    call(&client, "resume", json!({})).await;

    let mut response = reqwest::Client::new()
        .get(format!("{url}/events?after=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut lines = Vec::new();
    let mut pending = Vec::new();
    let mut read_line = async || -> Value {
        loop {
            if let Some(end) = pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                return serde_json::from_slice(&line).unwrap();
            }
            let chunk = tokio::time::timeout(Duration::from_secs(5), response.chunk())
                .await
                .expect("no event within 5 s")
                .unwrap()
                .unwrap();
            pending.extend_from_slice(&chunk);
        }
    };
    let replayed = read_line().await;
    assert_eq!(
        (replayed["seq"].as_u64(), replayed["event"].as_str()),
        (Some(2), Some("resumed"))
    );

    call(&client, "pause", json!({ "reason": "live" })).await;
    let live = read_line().await;
    assert_eq!(
        (
            live["seq"].as_u64(),
            live["event"].as_str(),
            live["reason"].as_str()
        ),
        (Some(3), Some("paused"), Some("live"))
    );
    lines.push(live);
    assert_eq!(
        serve::events::summary(&serde_json::from_value(lines.remove(0)).unwrap()),
        "#3 paused: live"
    );

    client.cancel().await.unwrap();
    server.shutdown.cancel();
}
