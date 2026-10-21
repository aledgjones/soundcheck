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
    let (server, url, _) = server_in().await;
    (server, url)
}

/// A server whose run folder sits in its own reports folder, beside an earlier run.
async fn server_in() -> (serve::Server, String, std::path::PathBuf) {
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
    let dir = std::env::temp_dir()
        .join(format!("soundcheck-serve-{}-{nanos}", std::process::id()))
        .join("20261021-090000");
    std::fs::create_dir_all(&dir).unwrap();
    let run = Run::open(plan, &dir.join("journal.jsonl"), serve::now_ms()).unwrap();
    let server = serve::start(
        Hub::new(suite, run, "v0.0.0 · test".into(), dir.clone(), 120_000),
        0,
    )
    .await
    .unwrap();
    let url = format!("http://{}", server.addr);
    (server, url, dir)
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
            "call": { "call": "uphold", "title": "Undo leaves the stroke on screen" },
            "reason": "screenshot shows the stroke after undo"
        }),
    )
    .await;
    assert_eq!(reviewed["reviewed"], "annotations/001-draw");
    let status = call(&client, "status", json!({})).await;
    assert_eq!(status["verdict"]["status"], "failing");
    assert_eq!(status["problems"][0]["kind"], "failure");
    assert_eq!(
        status["problems"][0]["title"],
        "Undo leaves the stroke on screen"
    );
    assert_eq!(
        status["problems"][0]["detail"],
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

#[tokio::test]
async fn finishing_writes_the_final_report_and_the_index_lists_runs() {
    let (server, url, dir) = server_in().await;
    // An earlier run beside this one, with its own export.
    let earlier = dir.parent().unwrap().join("20261020-090000");
    std::fs::create_dir_all(&earlier).unwrap();
    std::fs::write(
        earlier.join("summary.json"),
        json!({"label": "v2.16.1 · candidate", "started": 1, "finished": true,
               "verdict": {"status": "ready", "reasons": []},
               "counts": {"total": 2, "pass": 2, "fail": 0, "blocked": 0}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(earlier.join("report.html"), "<h1>earlier</h1>").unwrap();

    let client = client(&url).await;
    call(
        &client,
        "pause",
        json!({ "reason": "a summary needs an event" }),
    )
    .await;
    call(&client, "resume", json!({})).await;
    let finished = call(&client, "finish", json!({})).await;
    let report = std::fs::read_to_string(finished["report"].as_str().unwrap()).unwrap();
    assert!(report.contains("Final report, exported"));
    assert!(
        !report.contains("class=\"nowbar\""),
        "an export has no live bar"
    );
    assert!(dir.join("summary.json").exists());

    let index = reqwest::get(format!("{url}/runs"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let (newer, older) = (
        index.find("v0.0.0 · test").unwrap(),
        index.find("v2.16.1 · candidate").unwrap(),
    );
    assert!(newer < older, "newest first");
    assert!(index.contains("href=\"runs/20261020-090000/report.html\""));
    let past = reqwest::get(format!("{url}/runs/20261020-090000/report.html"))
        .await
        .unwrap();
    assert_eq!(past.text().await.unwrap(), "<h1>earlier</h1>");

    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn evidence_is_served_from_the_run_folder_only() {
    let (server, url, dir) = server_in().await;
    std::fs::create_dir_all(dir.join("evidence")).unwrap();
    std::fs::write(dir.join("evidence/undo.png"), b"png").unwrap();
    let ok = reqwest::get(format!("{url}/files/evidence/undo.png"))
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.headers()["content-type"], "image/png");
    for bad in ["..%2Fjournal.jsonl", ".hidden", "missing.png"] {
        let r = reqwest::get(format!("{url}/files/evidence/{bad}"))
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "{bad}");
    }
    server.shutdown.cancel();
}
