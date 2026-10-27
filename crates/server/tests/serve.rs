use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use soundcheck::run::{Plan, PlannedSpec, Run};
use soundcheck::serve::{self, Hub};
use soundcheck::suite;
use soundcheck::suite::config::Platform;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

async fn server() -> (serve::Server, String) {
    let (server, url, _) = server_in().await;
    (server, url)
}

/// A server whose run folder sits in its own reports folder, beside an earlier run.
async fn server_in() -> (serve::Server, String, std::path::PathBuf) {
    server_on(&[Platform::Android, Platform::Ios, Platform::Web]).await
}

/// [`server_in`] with rows only on `platforms`.
async fn server_on(platforms: &[Platform]) -> (serve::Server, String, std::path::PathBuf) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/good");
    let suite = suite::load(&root);
    let plan = Plan {
        specs: suite
            .specs
            .iter()
            .map(|s| PlannedSpec {
                id: s.id.clone(),
                platforms: s
                    .front
                    .platforms
                    .iter()
                    .filter(|p| platforms.contains(p))
                    .copied()
                    .collect(),
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
        Hub::without_devices(
            suite,
            run,
            "v0.0.0 · test".into(),
            dir.clone(),
            120_000,
            vec![Platform::Android, Platform::Ios],
        ),
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
        // `brief` answers in plain Markdown.
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    }
}

#[tokio::test]
async fn a_long_wait_is_refused_inside_a_batch() {
    let (_server, url) = server().await;
    let client = client(&url).await;
    let pause = json!({"tool": "pause", "ms": 30_000});
    let refused = call(
        &client,
        "batch",
        json!({"token": "any", "steps": [pause, pause]}),
    )
    .await;
    assert!(refused.as_str().unwrap().contains("separate `sleep` calls"));
}

#[tokio::test]
async fn a_suite_with_a_tracker_needs_its_ticket() {
    let (_server, url) = server().await;
    let client = client(&url).await;
    let refused = call(
        &client,
        "triage",
        json!({"problem": 1, "decision": {"decision": "defer"}, "reason": "known"}),
    )
    .await;
    assert!(refused.as_str().unwrap().contains("need its ticket"));
}

#[tokio::test]
async fn a_run_in_use_keeps_its_port() {
    let (server, url) = server().await;
    let step_down = || {
        reqwest::Client::new()
            .post(format!("{url}/step-down"))
            .send()
    };
    let going = step_down().await.unwrap();
    assert_eq!(going.status().as_u16(), 409);
    assert!(going.text().await.unwrap().contains("still going"));

    let client = client(&url).await;
    call(&client, "finish", json!({})).await;
    let just_used = step_down().await.unwrap();
    assert_eq!(just_used.status().as_u16(), 409);
    assert!(
        just_used
            .text()
            .await
            .unwrap()
            .contains("triage may be under way")
    );
    assert!(!server.shutdown.is_cancelled());
}

#[tokio::test]
async fn an_open_event_stream_doesnt_keep_the_server_up() {
    let (server, url) = server().await;
    // As the live page does: follow the stream and never close it.
    let stream = reqwest::get(format!("{url}/events")).await.unwrap();
    assert_eq!(stream.status().as_u16(), 200);
    server.shutdown.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), server.handle)
        .await
        .expect("the server stops with a stream open")
        .unwrap();
    drop(stream);
}

#[tokio::test]
async fn a_reports_server_makes_way_for_a_run() {
    let dir = std::env::temp_dir().join(format!("soundcheck-reports-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let server = serve::start_reports(dir, 0).await.unwrap();
    let answer = reqwest::Client::new()
        .post(format!("http://{}/step-down", server.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status().as_u16(), 200);
    tokio::time::timeout(std::time::Duration::from_secs(5), server.handle)
        .await
        .expect("the reports server stops")
        .unwrap();
}

#[tokio::test]
async fn ready_answers_whether_a_platform_can_take_a_spec() {
    let (_server, url) = server().await;
    let response = reqwest::get(format!("{url}/ready/ios")).await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), "ios ready\n");
    let unknown = reqwest::get(format!("{url}/ready/desktop")).await.unwrap();
    assert!(unknown.status().is_client_error());
}

#[tokio::test]
async fn a_stopped_testers_hook_records_its_tokens() {
    let (server, url, dir) = server_in().await;
    let client = client(&url).await;
    let dispatched = call(
        &client,
        "dispatch",
        json!({ "spec": "annotations/001-draw", "platform": "android" }),
    )
    .await;
    let token = dispatched["token"].as_str().unwrap();
    let transcript = dir.join("agent.jsonl");
    let lines = [
        json!({ "type": "user", "message": { "content": dispatched["prompt"] } }),
        json!({ "type": "assistant", "message": { "id": "m1", "usage": {
            "input_tokens": 2, "cache_creation_input_tokens": 13_000,
            "cache_read_input_tokens": 0, "output_tokens": 100 } } }),
        json!({ "type": "assistant", "message": { "id": "m2", "usage": {
            "input_tokens": 1, "cache_creation_input_tokens": 400,
            "cache_read_input_tokens": 13_000, "output_tokens": 60 } } }),
    ];
    std::fs::write(&transcript, lines.map(|l| l.to_string()).join("\n")).unwrap();
    let hook = |agent_type: &str| {
        json!({ "agent_type": agent_type, "agent_transcript_path": transcript }).to_string()
    };

    let port = server.addr.port();
    let other = soundcheck::tester_usage::subagent_stop(&hook("general-purpose"), port)
        .await
        .unwrap();
    assert_eq!(other, None, "other agents are left alone");
    let said = soundcheck::tester_usage::subagent_stop(&hook("soundcheck:soundcheck-tester"), port)
        .await
        .unwrap();
    assert_eq!(
        said,
        Some(format!(
            "{token} used 13461 tokens; {token}: the tester ended without posting"
        )),
        "a tester that never posted is queued again"
    );
    let status = call(&client, "status", json!({})).await;
    assert_eq!(status["rows"][0]["state"], "queued");

    let journal = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap();
    let recorded: Value = journal
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|e| e["event"] == "tokens")
        .unwrap();
    assert_eq!(recorded["token"], token);
    assert_eq!(recorded["tokens"], 13_461);
    assert_eq!(
        recorded["spent"],
        json!({ "input": 3, "cache_write": 13_400, "cache_read": 13_000, "output": 160 })
    );

    let unknown = reqwest::Client::new()
        .post(format!("{url}/tokens"))
        .json(&json!({ "token": "nope#1@ios", "tokens": 1,
            "spent": { "input": 0, "cache_write": 0, "cache_read": 0, "output": 0 } }))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status().as_u16(), 409);
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
        "brief",
        "post_result",
        "status",
        "dispatch",
        "queue",
        "review",
        "review_find",
        "install_build",
        "triage",
        "add_evidence",
        "pause",
        "resume",
        "finish",
        "relabel",
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
    let prompt = dispatched["prompt"].as_str().unwrap();
    assert!(
        prompt.contains(&token) && prompt.contains("`brief`"),
        "{prompt}"
    );
    assert!(
        dispatched.get("brief").is_none(),
        "the brief skips the coordinator"
    );
    let brief = call(&client, "brief", json!({ "token": token })).await;
    let brief = brief.as_str().unwrap();
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
    let refused = call(&client, "brief", json!({ "token": token })).await;
    assert!(
        refused
            .as_str()
            .unwrap()
            .starts_with(&format!("refused: token {token} isn't a running spec")),
        "a posted spec's brief is refused: {refused}"
    );

    let status = call(&client, "status", json!({})).await;
    assert_eq!(status["verdict"]["status"], "not-ready");
    assert!(
        status["finish_blockers"][0]
            .as_str()
            .unwrap()
            .contains("needs review")
    );
    // The waiting claim carries what a review call rests on.
    let waiting = status["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["state"] == "needs review")
        .unwrap();
    let failed = &waiting["review"]["judgments"][0];
    assert_eq!(failed["answer"], "failed");
    assert_eq!(failed["observed"], "the stroke stayed after undo");
    assert_eq!(
        failed["text"],
        "The stroke disappears on undo and returns in the same place on redo."
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
async fn the_page_offers_its_report_as_a_download() {
    let (server, url, dir) = server_in().await;
    let client = client(&url).await;
    call(&client, "pause", json!({ "reason": "an event to report" })).await;

    let page = reqwest::get(format!("{url}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("href=\"download\""));
    assert!(
        page.contains("class=\"nowbar-time elapsed\""),
        "one timer for the run"
    );
    let download = reqwest::get(format!("{url}/download")).await.unwrap();
    let disposition = download.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_string();
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(
        disposition,
        format!("attachment; filename=\"soundcheck-{name}.html\"")
    );
    let html = download.text().await.unwrap();
    assert!(html.contains("In progress, exported"));
    assert!(
        !html.contains("href=\"download\""),
        "an export has no button"
    );

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

#[tokio::test]
async fn the_build_identity_hook_records_each_platforms_build() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!("soundcheck-builds-{nanos}"));
    let suite_dir = base.join("suite");
    let good = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/good");
    std::fs::create_dir_all(suite_dir.join("annotations")).unwrap();
    for f in [
        "annotations/_feature.md",
        "annotations/001-draw.spec.md",
        "annotations/002-back.spec.md",
    ] {
        std::fs::copy(good.join(f), suite_dir.join(f)).unwrap();
    }
    let config: String = std::fs::read_to_string(good.join("suite.yaml"))
        .unwrap()
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("build-identity:") {
                // iOS has no build installed until the install hook runs: its identity hook fails.
                [
                    r#"  build-identity: "test {platform} = 'ios' && ! test -f installed-ios && exit 1; echo '{\"commit\": \"abc1234def\", \"fingerprint\": \"fp-'{platform}'\", \"version\": \"2.16.2\"}'""#,
                    r#"  install: "touch installed-{platform}""#,
                ]
                .join("\n")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(suite_dir.join("suite.yaml"), config).unwrap();
    let suite = suite::load(&suite_dir);
    assert!(suite.problems.is_empty(), "{:?}", suite.problems);
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
    let run_dir = base.join("reports/run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let run = Run::open(plan, &run_dir.join("journal.jsonl"), serve::now_ms()).unwrap();
    let hub = Hub::without_devices(
        suite,
        run,
        "builds".into(),
        run_dir,
        120_000,
        vec![Platform::Android, Platform::Ios],
    );

    let problems = hub.read_builds(&[Platform::Android, Platform::Ios]).await;
    assert_eq!(problems.len(), 1);
    assert!(problems[0].starts_with("ios:"), "{problems:?}");
    let android = hub
        .with_run(|run, _| Ok(run.state().builds.get(&Platform::Android).cloned()))
        .unwrap()
        .unwrap();
    assert_eq!(android.commit, "abc1234def");
    assert_eq!(android.fingerprint.as_deref(), Some("fp-android"));
    assert_eq!(android.label(), "2.16.2 (abc1234)");

    // Reading the same build again changes nothing.
    let before = hub.with_run(|run, _| Ok(run.last_seq())).unwrap();
    assert!(hub.read_builds(&[Platform::Android]).await.is_empty());
    assert_eq!(hub.with_run(|run, _| Ok(run.last_seq())).unwrap(), before);

    // The install hook builds and installs it; then it can be identified.
    hub.install(Platform::Ios).await.unwrap();
    assert!(hub.read_builds(&[Platform::Ios]).await.is_empty());
}

#[tokio::test]
async fn a_tester_can_run_the_projects_data_checks() {
    let (server, url) = server().await;
    let client = client(&url).await;
    let dispatched = call(
        &client,
        "dispatch",
        json!({ "spec": "annotations/002-back", "platform": "android" }),
    )
    .await;
    let token = dispatched["token"].as_str().unwrap();
    let out = call(
        &client,
        "check_data",
        json!({ "token": token, "name": "parts", "args": ["E2E Seed Alpha Score"] }),
    )
    .await;
    assert_eq!(out["output"], "checked parts E2E Seed Alpha Score");
    let stale = call(
        &client,
        "check_data",
        json!({ "token": "nope", "name": "parts" }),
    )
    .await;
    assert!(
        stale.as_str().unwrap().starts_with("refused: token nope"),
        "{stale}"
    );
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn status_names_each_waiting_side_find_by_id() {
    let (server, url) = server().await;
    let client = client(&url).await;
    let dispatched = call(
        &client,
        "dispatch",
        json!({ "spec": "annotations/002-back", "platform": "android" }),
    )
    .await;
    let token = dispatched["token"].as_str().unwrap();
    call(
        &client,
        "post_result",
        json!({
            "token": token,
            "judgments": [{ "outcome": 0, "answer": "held", "observed": "the collection showed each time" }],
            "side_finds": [{ "summary": "Back flashes white", "observed": "a white frame on the third back" }]
        }),
    )
    .await;
    let status = call(&client, "status", json!({})).await;
    let find = &status["side_finds_waiting"][0];
    assert_eq!(find["id"], 1);
    assert_eq!(find["summary"], "Back flashes white");
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn a_tester_can_sleep_without_stalling() {
    let (server, url) = server().await;
    let client = client(&url).await;
    let dispatched = call(
        &client,
        "dispatch",
        json!({ "spec": "annotations/002-back", "platform": "android" }),
    )
    .await;
    let token = dispatched["token"].as_str().unwrap();
    let slept = call(&client, "sleep", json!({ "token": token, "ms": 10 })).await;
    assert_eq!(slept, "slept 10 ms");
    let absurd = call(&client, "sleep", json!({ "token": token, "ms": 60_000 })).await;
    assert!(absurd.as_str().unwrap().contains("over 30000"), "{absurd}");
    let stale = call(&client, "sleep", json!({ "token": "nope", "ms": 10 })).await;
    assert!(
        stale.as_str().unwrap().starts_with("refused: token nope"),
        "{stale}"
    );
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn post_result_resolves_evidence_to_the_attempts_own_shot() {
    let (server, url, dir) = server_in().await;
    let client = client(&url).await;
    let dispatched = call(
        &client,
        "dispatch",
        json!({ "spec": "annotations/002-back", "platform": "android" }),
    )
    .await;
    let token = dispatched["token"].as_str().unwrap();
    std::fs::create_dir_all(dir.join("evidence")).unwrap();
    std::fs::write(
        dir.join("evidence/annotations-002-back-1-android-wide.png"),
        b"png",
    )
    .unwrap();
    let posted = call(
        &client,
        "post_result",
        json!({
            "token": token,
            "judgments": [{ "outcome": 0, "answer": "held", "observed": "the collection showed", "evidence": ["wide"] }],
            "side_finds": [{ "summary": "Back flashes white", "observed": "a white frame", "evidence": ["wide.png"] }]
        }),
    )
    .await;
    assert_eq!(posted["claim"], "PASS", "{posted}");
    let status = call(&client, "status", json!({})).await;
    assert_eq!(
        status["side_finds_waiting"][0]["evidence"][0],
        "annotations-002-back-1-android-wide.png"
    );
    let journal = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap();
    assert!(
        journal.contains("\"evidence\":[\"annotations-002-back-1-android-wide.png\"]"),
        "{journal}"
    );
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn post_result_refuses_evidence_that_was_never_saved() {
    let (server, url) = server().await;
    let client = client(&url).await;
    let dispatched = call(
        &client,
        "dispatch",
        json!({ "spec": "annotations/002-back", "platform": "android" }),
    )
    .await;
    let token = dispatched["token"].as_str().unwrap();
    let refused = call(
        &client,
        "post_result",
        json!({
            "token": token,
            "judgments": [{ "outcome": 0, "answer": "held", "observed": "the collection showed", "evidence": ["wide2"] }]
        }),
    )
    .await;
    let refused = refused.as_str().unwrap();
    assert!(
        refused.starts_with("refused: no saved evidence named wide2"),
        "{refused}"
    );
    let status = call(&client, "status", json!({})).await;
    assert_eq!(
        status["finish_blockers"][0], "annotations/002-back is running on android",
        "the row waits for a corrected result"
    );
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn queue_adds_new_specs_reruns_old_ones_and_reopens_a_finished_run() {
    use soundcheck::run::saved;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!("soundcheck-add-{}-{nanos}", std::process::id()));
    let root = base.join("suite");
    let good = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/good");
    std::fs::create_dir_all(root.join("annotations")).unwrap();
    std::fs::copy(good.join("suite.yaml"), root.join("suite.yaml")).unwrap();
    for file in ["_feature.md", "001-draw.spec.md", "002-back.spec.md"] {
        std::fs::copy(
            good.join("annotations").join(file),
            root.join("annotations").join(file),
        )
        .unwrap();
    }
    let suite = suite::load(&root);
    let info = saved::RunInfo {
        suite: root.clone(),
        label: "v0.0.0 · test".into(),
        platforms: vec![Platform::Android, Platform::Ios],
        scope: vec!["annotations/001-draw".into()],
    };
    let dir = base.join("reports").join("20261021-090000");
    std::fs::create_dir_all(&dir).unwrap();
    saved::save(&dir, &info, &suite).unwrap();
    let plan = saved::plan_for(&suite, &info.platforms, &info.scope).unwrap();
    let run = Run::open(plan, &dir.join("journal.jsonl"), serve::now_ms()).unwrap();
    let server = serve::start(
        Hub::without_devices(
            suite,
            run,
            info.label.clone(),
            dir.clone(),
            120_000,
            info.platforms.clone(),
        ),
        0,
    )
    .await
    .unwrap();
    let client = client(&format!("http://{}", server.addr)).await;

    let refused = call(
        &client,
        "queue",
        json!({ "specs": ["annotations/404-nope"] }),
    )
    .await;
    assert!(
        refused
            .as_str()
            .unwrap()
            .contains("annotations/404-nope: no feature or spec by that name"),
        "{refused}"
    );
    let refused = call(
        &client,
        "queue",
        json!({ "specs": ["annotations/001-draw"] }),
    )
    .await;
    assert!(
        refused.as_str().unwrap().contains("already ran: say why"),
        "{refused}"
    );

    let added = call(
        &client,
        "queue",
        json!({ "specs": ["annotations/002-back"] }),
    )
    .await;
    assert_eq!(
        added,
        json!({ "added": [{ "spec": "annotations/002-back", "platform": "android" }], "requeued": [] })
    );
    let finished = call(&client, "finish", json!({})).await;
    assert!(finished["report"].is_string(), "{finished}");

    // A spec written after the run started comes from the suite as it is now.
    let text = std::fs::read_to_string(root.join("annotations/001-draw.spec.md"))
        .unwrap()
        .replace("Draw a pen stroke, undo and redo it", "Erase a pen stroke");
    std::fs::write(root.join("annotations/003-erase.spec.md"), text).unwrap();
    let added = call(
        &client,
        "queue",
        json!({ "specs": ["annotations"], "reason": "the eraser joined; check the rest again" }),
    )
    .await;
    assert_eq!(
        added,
        json!({ "added": [
            { "spec": "annotations/003-erase", "platform": "android" },
            { "spec": "annotations/003-erase", "platform": "ios" }
        ], "requeued": [
            { "spec": "annotations/001-draw", "platform": "android", "was": "not run" },
            { "spec": "annotations/001-draw", "platform": "ios", "was": "not run" },
            { "spec": "annotations/002-back", "platform": "android", "was": "not run" }
        ] })
    );
    let dispatched = call(
        &client,
        "dispatch",
        json!({ "spec": "annotations/003-erase", "platform": "ios" }),
    )
    .await;
    let brief = call(&client, "brief", json!({ "token": dispatched["token"] })).await;
    assert!(
        brief
            .as_str()
            .is_some_and(|b| b.contains("Erase a pen stroke")),
        "the run is open again and dispatches the new spec: {dispatched}"
    );

    let info = saved::read_info(&dir).unwrap();
    assert_eq!(
        info.scope,
        vec![
            "annotations/001-draw",
            "annotations/002-back",
            "annotations/003-erase"
        ]
    );
    assert!(
        dir.join("suite/annotations/003-erase.spec.md").exists(),
        "the snapshot keeps the run self-contained"
    );
    let journal = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap();
    assert!(journal.contains(r#""specs":["annotations/003-erase"]"#));
    let (_, reloaded, _) = saved::load(&dir, serve::now_ms()).unwrap();
    assert!(
        reloaded
            .row("annotations/003-erase", Platform::Ios)
            .is_some()
    );
    assert!(!reloaded.state().finished);

    // A spec edited mid-run is reread when it's queued again, and the next brief has the new wording.
    let back = root.join("annotations/002-back.spec.md");
    let edited = std::fs::read_to_string(&back)
        .unwrap()
        .replace("double-tap back", "tap back twice, a second apart");
    std::fs::write(&back, edited).unwrap();
    let queued = call(
        &client,
        "queue",
        json!({ "specs": ["annotations/002-back", "annotations/003-erase"], "reason": "the goal was reworded" }),
    )
    .await;
    assert!(
        queued["requeued"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["spec"] == "annotations/003-erase" && r["was"] == "running"),
        "a running attempt is abandoned: {queued}"
    );
    let dispatched = call(
        &client,
        "dispatch",
        json!({ "spec": "annotations/002-back", "platform": "android" }),
    )
    .await;
    let brief = call(&client, "brief", json!({ "token": dispatched["token"] })).await;
    assert!(
        brief
            .as_str()
            .is_some_and(|b| b.contains("tap back twice, a second apart")),
        "the rerun gets the edited spec: {brief}"
    );
    assert!(
        std::fs::read_to_string(dir.join("suite/annotations/002-back.spec.md"))
            .unwrap()
            .contains("a second apart"),
        "the snapshot takes the new text"
    );
    let journal = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap();
    assert!(journal.contains(r#""event":"spec-changed","spec":"annotations/002-back""#));

    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

const DRAW: &str = "annotations/001-draw";

/// Posts a pass for a running attempt of 001-draw, whose probes need a word.
async fn pass_draw(client: &RunningService<RoleClient, ClientConfig>, token: &str) {
    let posted = call(
        client,
        "post_result",
        json!({
            "token": token,
            "judgments": [
                { "outcome": 0, "answer": "held", "observed": "the stroke went and came back" },
                { "outcome": 1, "answer": "held", "observed": "undo and redo matched" }
            ],
            "probed": "undid twice: nothing else changed"
        }),
    )
    .await;
    assert_eq!(posted["claim"], "PASS", "{posted}");
}

#[tokio::test]
async fn dispatch_with_no_spec_starts_the_next_row() {
    let (server, url) = server().await;
    let client = client(&url).await;
    let first = call(&client, "dispatch", json!({})).await;
    assert_eq!(first["token"], format!("{DRAW}#1@android"));
    let refused = call(&client, "dispatch", json!({})).await;
    assert!(
        refused.as_str().unwrap().contains("one spec at a time"),
        "{refused}"
    );

    pass_draw(&client, first["token"].as_str().unwrap()).await;
    let next = reqwest::get(format!("{url}/next")).await.unwrap();
    assert_eq!(
        next.json::<Value>().await.unwrap(),
        json!({ "spec": DRAW, "platform": "ios" }),
        "asking doesn't dispatch"
    );
    let back = call(&client, "dispatch", json!({ "platform": "android" })).await;
    assert_eq!(back["token"], "annotations/002-back#1@android");
    assert_eq!(back["agent"], "soundcheck-tester-mechanics");
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn the_tester_hook_turns_an_agent_call_into_a_dispatch() {
    let (server, _) = server().await;
    let port = server.addr.port();
    let hook = |agent: &str, prompt: &str| {
        json!({
            "session_id": "coordinator-1",
            "tool_name": "Agent",
            "tool_input": { "subagent_type": agent, "prompt": prompt, "description": "next tester", "model": "haiku" },
        })
        .to_string()
    };
    let other = soundcheck::plugin_hooks::pre_tool_use(&hook("general-purpose", "next"), port)
        .await
        .unwrap();
    assert_eq!(other, None, "other agents are left alone");

    let named = hook(
        "soundcheck:soundcheck-tester",
        "annotations/002-back, please",
    );
    let reply: Value = serde_json::from_str(
        &soundcheck::plugin_hooks::pre_tool_use(&named, port)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let out = &reply["hookSpecificOutput"];
    assert_eq!(out["permissionDecision"], "allow");
    let token = "annotations/002-back#1@android";
    let updated = &out["updatedInput"];
    assert_eq!(
        updated["subagent_type"],
        "soundcheck:soundcheck-tester-mechanics"
    );
    assert_eq!(updated["description"], token);
    assert_eq!(updated["model"], "opus", "the spec's model");
    assert_eq!(updated["run_in_background"], true);
    assert!(
        updated["prompt"]
            .as_str()
            .unwrap()
            .starts_with(&format!("Run soundcheck spec `{token}`"))
    );

    let next = hook("soundcheck:soundcheck-tester", "next");
    let reply: Value = serde_json::from_str(
        &soundcheck::plugin_hooks::pre_tool_use(&next, port)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let out = &reply["hookSpecificOutput"];
    assert_eq!(out["permissionDecision"], "deny");
    assert!(
        out["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("one spec at a time"),
        "{out}"
    );

    server.shutdown.cancel();
    server.handle.await.unwrap();
    let down = soundcheck::plugin_hooks::pre_tool_use(&next, port)
        .await
        .unwrap();
    assert_eq!(down, None, "no run up, no change");
}

#[tokio::test]
async fn the_stop_hook_holds_only_a_stalled_coordinator() {
    let (server, url) = server().await;
    let port = server.addr.port();
    let client = client(&url).await;
    let stop = |session: &str, active: bool| {
        json!({ "session_id": session, "stop_hook_active": active }).to_string()
    };
    let stopping =
        |input: String| async move { soundcheck::plugin_hooks::stop(&input, port).await.unwrap() };
    assert_eq!(
        stopping(stop("coordinator-1", false)).await,
        None,
        "not a coordinator yet"
    );

    let dispatched: Value = reqwest::Client::new()
        .post(format!("{url}/dispatch"))
        .json(&json!({ "prompt": "next", "session": "coordinator-1" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        stopping(stop("coordinator-1", false)).await,
        None,
        "a tester is running"
    );

    pass_draw(&client, dispatched["token"].as_str().unwrap()).await;
    let blocked: Value =
        serde_json::from_str(&stopping(stop("coordinator-1", false)).await.unwrap()).unwrap();
    assert_eq!(blocked["decision"], "block");
    assert!(
        blocked["reason"]
            .as_str()
            .unwrap()
            .starts_with(&format!("dispatch {DRAW} on ios")),
        "{blocked}"
    );
    assert_eq!(
        stopping(stop("coordinator-1", true)).await,
        None,
        "never twice in a row"
    );
    assert_eq!(
        stopping(stop("someone-else", false)).await,
        None,
        "other sessions stop freely"
    );

    call(&client, "pause", json!({ "reason": "a fix first" })).await;
    assert_eq!(
        stopping(stop("coordinator-1", false)).await,
        None,
        "a paused run waits"
    );

    client.cancel().await.unwrap();
    server.shutdown.cancel();
    server.handle.await.unwrap();
    assert_eq!(
        stopping(stop("coordinator-1", false)).await,
        None,
        "no run up"
    );
}

#[tokio::test]
async fn a_new_session_hears_where_the_run_stands() {
    let (server, _, dir) = server_in().await;
    let port = server.addr.port();
    let context = soundcheck::plugin_hooks::session_start(port)
        .await
        .unwrap()
        .unwrap();
    let reply: Value = serde_json::from_str(&context).unwrap();
    let text = reply["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(text.contains(&dir.display().to_string()), "{text}");
    assert!(text.contains("v0.0.0 · test"), "{text}");
    assert!(text.contains("verdict not-ready"), "{text}");
    assert!(text.contains("4 of 4 rows left"), "{text}");
    assert!(text.contains("soundcheck:run"), "{text}");

    server.shutdown.cancel();
    server.handle.await.unwrap();
    assert_eq!(
        soundcheck::plugin_hooks::session_start(port).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn the_run_finishes_itself_once_the_last_row_files() {
    let (server, url, dir) = server_on(&[Platform::Android, Platform::Ios]).await;
    let client = client(&url).await;
    for _ in 0..2 {
        let d = call(&client, "dispatch", json!({ "spec": DRAW })).await;
        pass_draw(&client, d["token"].as_str().unwrap()).await;
    }
    let status = call(&client, "status", json!({})).await;
    assert_eq!(status["finished"], false, "a row is still untested");

    let back = call(&client, "dispatch", json!({})).await;
    call(
        &client,
        "post_result",
        json!({
            "token": back["token"],
            "judgments": [{ "outcome": 0, "answer": "held", "observed": "the collection showed" }]
        }),
    )
    .await;
    let status = call(&client, "status", json!({})).await;
    assert_eq!(status["finished"], true);
    let report = std::fs::read_to_string(dir.join("report.html")).unwrap();
    assert!(report.contains("Final report, exported"));
    assert!(!report.contains("used 9876 tokens"));

    let late = reqwest::Client::new()
        .post(format!("{url}/tokens"))
        .json(&json!({ "token": back["token"], "tokens": 9_876,
            "spent": { "input": 1, "cache_write": 9_000, "cache_read": 0, "output": 875 } }))
        .send()
        .await
        .unwrap();
    assert_eq!(late.status().as_u16(), 200);
    let report = std::fs::read_to_string(dir.join("report.html")).unwrap();
    assert!(
        report.contains("used 9876 tokens"),
        "the last tester's tokens reach the report"
    );
    assert!(report.contains("Final report, exported"));
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}
