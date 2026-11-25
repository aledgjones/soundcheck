use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use soundcheck::run::event::Event;
use soundcheck::run::event::Spent;
use soundcheck::run::{Plan, PlannedSpec, Run, saved};
use soundcheck::serve::agents::{self, Ended};
use soundcheck::serve::{self, Hub, orchestrate};
use soundcheck::suite;
use soundcheck::suite::config::Platform;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ANDROID: Platform = Platform::literal("android");
const IOS: Platform = Platform::literal("ios");
const WEB: Platform = Platform::literal("web");

async fn server() -> (serve::Server, String) {
    let (server, url, _) = server_in().await;
    (server, url)
}

/// A server whose run folder sits in its own reports folder, beside an earlier run.
async fn server_in() -> (serve::Server, String, std::path::PathBuf) {
    server_on(&[ANDROID, IOS, WEB]).await
}

/// [`server_in`] with rows only on `platforms`.
async fn server_on(platforms: &[Platform]) -> (serve::Server, String, std::path::PathBuf) {
    let (server, url, dir, _) = server_hub(platforms).await;
    (server, url, dir)
}

/// [`server_on`], with its hub.
async fn server_hub(
    platforms: &[Platform],
) -> (
    serve::Server,
    String,
    std::path::PathBuf,
    std::sync::Arc<Hub>,
) {
    server_hub_as(platforms, false).await
}

/// [`server_hub`], as a trial on uncommitted code when `uncommitted`.
async fn server_hub_as(
    platforms: &[Platform],
    uncommitted: bool,
) -> (
    serve::Server,
    String,
    std::path::PathBuf,
    std::sync::Arc<Hub>,
) {
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
                outcomes: soundcheck::run::outcomes_by_platform(s, &s.front.platforms),
            })
            .collect(),
        parallel: false,
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // The clock alone doesn't tell apart two tests that start in the same microsecond.
    static SERVERS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let nth = SERVERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir()
        .join(format!(
            "soundcheck-serve-{}-{nanos}-{nth}",
            std::process::id()
        ))
        .join("20261021-090000");
    std::fs::create_dir_all(&dir).unwrap();
    let run = Run::open(plan, &dir.join("journal.jsonl"), serve::now_ms()).unwrap();
    let mut hub = Hub::without_devices(
        suite,
        run,
        "v0.0.0 · test".into(),
        dir.clone(),
        120_000,
        vec![ANDROID, IOS],
    );
    std::sync::Arc::get_mut(&mut hub).unwrap().uncommitted = uncommitted;
    let server = serve::start(hub.clone(), 0).await.unwrap();
    let url = format!("http://{}", server.addr);
    HUBS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((url.clone(), hub.clone()));
    (server, url, dir, hub)
}

/// The hub of each server started here, by its URL, so a test that kept only the URL can dispatch.
static HUBS: Mutex<Vec<(String, Arc<Hub>)>> = Mutex::new(Vec::new());

/// Starts a row's next attempt as the server's own lanes do (nothing here starts them, so each test dispatches by
/// hand): the attempt as JSON, or the refusal as the text a tool would answer with.
async fn dispatch(hub: &Arc<Hub>, args: Value) -> Value {
    let started = async {
        let platform = args["platform"]
            .as_str()
            .map(|name| hub.platform(name))
            .transpose()?;
        hub.dispatch(args["spec"].as_str(), platform).await
    };
    match started.await {
        Ok(dispatched) => serde_json::to_value(dispatched).unwrap(),
        Err(e) => Value::String(format!("refused: {e:#}")),
    }
}

/// [`dispatch`], on the server at `url`.
async fn dispatch_at(url: &str, args: Value) -> Value {
    let hub = HUBS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(at, _)| at == url)
        .map(|(_, hub)| hub.clone())
        .expect("a server started by server_hub_as");
    dispatch(&hub, args).await
}

/// The run's journal, one entry per line.
fn journal(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("journal.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// The view a page carries in its `<script type="application/json">`.
fn view_in(html: &str) -> Value {
    let json = &html[html.find("id=\"view\">").unwrap() + "id=\"view\">".len()..];
    serde_json::from_str(&json[..json.find("</script>").unwrap()]).unwrap()
}

async fn view_at(url: &str) -> Value {
    reqwest::get(url).await.unwrap().json().await.unwrap()
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
        // `sleep` answers in plain text.
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
async fn an_uncommitted_run_files_no_tickets_and_its_page_says_so() {
    let (_server, url, _, hub) = server_hub_as(&[ANDROID, IOS, WEB], true).await;
    let client = client(&url).await;
    let refused = call(
        &client,
        "triage",
        json!({"problem": 1, "decision": {"decision": "defer", "ticket": "SA-1"}, "reason": "known"}),
    )
    .await;
    assert!(
        refused.as_str().unwrap().contains("files no tickets"),
        "{refused}"
    );
    // The suite's tracker asks for no ticket: the reason is the record.
    let untracked = call(
        &client,
        "triage",
        json!({"problem": 1, "decision": {"decision": "defer"}, "reason": "known"}),
    )
    .await;
    assert!(!untracked.to_string().contains("ticket"), "{untracked}");

    assert_eq!(
        call(&client, "status", json!({})).await["uncommitted"],
        true
    );
    let view = view_at(&format!("{url}/{}/view", hub.page_key())).await;
    assert_eq!(view["trial"], soundcheck::report::view::TRIAL);
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
async fn a_running_run_opens_and_closes_its_remote_link() {
    let (_server, url) = server().await;
    let remote = |on: bool| {
        let url = url.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{url}/remote"))
                .json(&json!({ "on": on }))
                .send()
                .await
                .unwrap()
        }
    };

    let closed = remote(false).await;
    assert_eq!(closed.status().as_u16(), 200);
    assert_eq!(
        closed.json::<Value>().await.unwrap(),
        json!({ "url": null })
    );

    // Opening asks suite.yaml who may sign in; this fixture names nobody.
    let refused = remote(true).await;
    assert_eq!(refused.status().as_u16(), 422);
    assert!(
        refused.text().await.unwrap().contains("remote: { allow:"),
        "says what suite.yaml needs"
    );
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
async fn a_tester_that_ends_without_posting_leaves_its_tokens_and_a_stall() {
    let (_server, url, dir, hub) = server_hub(&[ANDROID, IOS, WEB]).await;
    let client = client(&url).await;
    let dispatched = dispatch(
        &hub,
        json!({ "spec": "annotations/001-draw", "platform": "android" }),
    )
    .await;
    let token = dispatched["token"].as_str().unwrap();
    // It got as far as driving the device: one that never makes a call is the harness's failure, not a stall.
    call(&client, "sleep", json!({ "token": token, "ms": 10 })).await;
    let spent = Spent {
        input: 3,
        cache_write: 13_400,
        cache_read: 13_000,
        output: 160,
    };
    hub.tester_done(
        token,
        Ended {
            error: None,
            usage: Some((13_461, spent)),
        },
    );

    let status = call(&client, "status", json!({})).await;
    assert_eq!(
        status["rows"][0]["state"], "queued",
        "a tester that never posted is queued again"
    );
    assert_eq!(status["paused"], false, "a clean end pauses nothing");
    let entries = journal(&dir);
    let recorded = entries.iter().find(|e| e["event"] == "tokens").unwrap();
    assert_eq!(recorded["token"], token);
    assert_eq!(recorded["tokens"], 13_461);
    assert_eq!(
        recorded["spent"],
        json!({ "input": 3, "cache_write": 13_400, "cache_read": 13_000, "output": 160 })
    );
    let stalled = entries.iter().find(|e| e["event"] == "stalled").unwrap();
    assert_eq!(stalled["token"], token);
    assert_eq!(stalled["reason"], serve::ENDED_WITHOUT_POSTING);

    // A token no attempt has is left out of the journal.
    hub.tester_done(
        "nope#1@ios",
        Ended {
            error: None,
            usage: Some((1, Spent::default())),
        },
    );
    assert_eq!(journal(&dir).len(), entries.len());
}

/// What a tester's process ending in `error` leaves behind.
fn failed(error: &str) -> Ended {
    Ended {
        error: Some(error.to_string()),
        usage: None,
    }
}

#[tokio::test]
async fn a_tester_that_could_not_run_puts_its_row_back_and_pauses_the_run() {
    let (_server, url, dir, hub) = server_hub(&[ANDROID, IOS, WEB]).await;
    let client = client(&url).await;
    let dispatched = dispatch(&hub, json!({ "spec": DRAW, "platform": "android" })).await;
    let token = dispatched["token"].as_str().unwrap();
    hub.tester_done(token, failed("usage limit reached"));

    let entries = journal(&dir);
    assert!(
        entries
            .iter()
            .any(|e| e["event"] == "interrupted" && e["token"] == token),
        "{entries:?}"
    );
    assert!(
        !entries.iter().any(|e| e["event"] == "stalled"),
        "the harness failed it, not the app"
    );
    let paused = entries.iter().find(|e| e["event"] == "paused").unwrap();
    let reason = paused["reason"].as_str().unwrap();
    assert!(
        reason.contains("a tester couldn't run (usage limit reached)"),
        "{reason}"
    );
    let row = hub
        .with_run(|run, _| Ok(run.row(DRAW, ANDROID).cloned()))
        .unwrap()
        .unwrap();
    assert_eq!(row.budget, 0, "no attempt was used");
    assert!(matches!(row.row, soundcheck::run::Row::Queued { .. }));

    // The rows after it aren't thrown at the same wall.
    let refused = dispatch(&hub, json!({})).await;
    assert!(
        refused.as_str().is_some_and(|r| r.contains("paused")),
        "{refused}"
    );
    call(&client, "resume", json!({})).await;
    let again = dispatch(&hub, json!({ "spec": DRAW, "platform": "android" })).await;
    assert!(again["token"].is_string(), "{again}");
}

#[tokio::test]
async fn a_tester_that_failed_after_a_call_counts_as_a_stall_and_pauses_the_run() {
    let (_server, url, dir, hub) = server_hub(&[ANDROID, IOS, WEB]).await;
    let client = client(&url).await;
    let dispatched = dispatch(&hub, json!({ "spec": DRAW, "platform": "android" })).await;
    let token = dispatched["token"].as_str().unwrap();
    call(&client, "sleep", json!({ "token": token, "ms": 10 })).await;
    hub.tester_done(token, failed("the model is overloaded"));

    let entries = journal(&dir);
    let stalled = entries.iter().find(|e| e["event"] == "stalled").unwrap();
    assert_eq!(stalled["token"], token);
    assert_eq!(
        stalled["reason"],
        format!("{}: the model is overloaded", serve::ENDED_WITHOUT_POSTING)
    );
    assert!(!entries.iter().any(|e| e["event"] == "interrupted"));
    let paused = entries.iter().find(|e| e["event"] == "paused").unwrap();
    assert!(
        paused["reason"]
            .as_str()
            .unwrap()
            .contains("the model is overloaded"),
        "{paused}"
    );
    let budget = hub
        .with_run(|run, _| Ok(run.row(DRAW, ANDROID).map(|r| r.budget)))
        .unwrap();
    assert_eq!(budget, Some(1), "the attempt was used");
}

#[tokio::test]
async fn a_tester_that_ends_after_posting_changes_nothing() {
    let (_server, url, dir, hub) = server_hub(&[ANDROID, IOS, WEB]).await;
    let client = client(&url).await;
    let dispatched = dispatch(&hub, json!({ "spec": DRAW, "platform": "android" })).await;
    let token = dispatched["token"].as_str().unwrap();
    pass_draw(&client, token).await;
    let before = journal(&dir).len();

    hub.tester_done(token, Ended::default());
    // Even one that ended in error: its result is in, so nothing is left to settle.
    hub.tester_done(token, failed("it was stopped"));

    assert_eq!(journal(&dir).len(), before);
    let status = call(&client, "status", json!({})).await;
    assert_eq!(status["paused"], false);
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

    for gone in ["dispatch", "brief"] {
        assert!(
            !names.contains(&gone.to_string()),
            "the server dispatches and briefs by itself: {gone}"
        );
    }

    let dispatched = dispatch_at(
        &url,
        json!({ "spec": "annotations/001-draw", "platform": "android" }),
    )
    .await;
    let token = dispatched["token"].as_str().unwrap().to_string();
    assert_eq!(dispatched["agent"], agents::TESTER);
    // The tester is launched with its brief as the prompt.
    let brief = dispatched["prompt"].as_str().unwrap();
    assert!(brief.contains(&token));
    assert!(brief.contains("0. The stroke disappears on undo"));
    assert!(
        brief.contains("Open a score, then tap Annotate."),
        "feature context is in the brief"
    );
    // How to post is the same for every spec, so it's in the tester's own instructions instead.
    assert!(!brief.contains("## Posting your result"), "{brief}");
    let definitions = agents::definitions();
    for tester in [agents::TESTER, agents::MECHANICS_TESTER] {
        let instructions = definitions[tester]["prompt"].as_str().unwrap();
        for part in ["## Posting your result", "## Pace", "## Fewer calls"] {
            assert!(instructions.contains(part), "{tester} lacks {part}");
        }
    }

    let refused = dispatch_at(
        &url,
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
    assert_eq!(posted["next"], "you're done: reply `done` and stop");

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
        view_in(&report).get("bar").is_none(),
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
    assert!(index.contains("href=\"20261020-090000/report.html\""));
    let past = reqwest::get(format!("{url}/20261020-090000/report.html"))
        .await
        .unwrap();
    assert_eq!(past.text().await.unwrap(), "<h1>earlier</h1>");
    let old_link = reqwest::get(format!("{url}/runs/20261020-090000/report.html"))
        .await
        .unwrap();
    assert_eq!(
        old_link.text().await.unwrap(),
        "<h1>earlier</h1>",
        "old /runs/ links still open"
    );
    let root = reqwest::get(format!("{url}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        root.contains("v2.16.1 · candidate"),
        "the root lists the runs"
    );

    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn the_live_page_gets_its_whole_view_then_only_what_changed() {
    let (server, url, _, hub) = server_hub(&[ANDROID]).await;
    hub.set_heartbeat(Duration::from_millis(400));
    let mut updates = reqwest::get(format!("{url}/{}/updates", hub.page_key()))
        .await
        .unwrap();
    assert_eq!(updates.headers()["content-type"], "text/event-stream");
    assert_eq!(updates.headers()["cache-control"], "no-cache");
    let mut buffered = String::new();
    // One Server-Sent Event as (event, data).
    let mut next = async || -> (String, Value) {
        loop {
            if let Some(end) = buffered.find("\n\n") {
                let frame: String = buffered.drain(..end + 2).collect();
                let field = |name: &str| {
                    frame
                        .lines()
                        .find_map(|l| l.strip_prefix(&format!("{name}: ")))
                        .unwrap()
                        .to_string()
                };
                return (
                    field("event"),
                    serde_json::from_str(&field("data")).unwrap(),
                );
            }

            let chunk = tokio::time::timeout(Duration::from_secs(5), updates.chunk())
                .await
                .expect("an event within 5 s")
                .unwrap()
                .expect("the feed stays open");
            buffered.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    };
    let (event, view) = next().await;
    assert_eq!(event, "view");
    assert_eq!(view["bar"]["paused"], false);

    let client = client(&url).await;
    call(&client, "pause", json!({ "reason": "watching the feed" })).await;
    // A ping may come first on a slow machine; the patch is the next thing that isn't one.
    let mut received = next().await;
    while received.0 == "ping" {
        received = next().await;
    }

    let (event, patch) = received;
    assert_eq!(event, "patch");
    assert_eq!(patch["bar"]["paused"], true, "{patch}");
    assert!(patch.get("checks").is_none(), "no row changed: {patch}");

    let (event, _) = next().await;
    assert_eq!(
        event, "ping",
        "nothing changed, so the feed says it's there"
    );

    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn the_page_offers_its_report_as_a_download() {
    let (server, url, dir) = server_in().await;
    let client = client(&url).await;
    call(&client, "pause", json!({ "reason": "an event to report" })).await;

    let page = reqwest::get(format!("{url}/{}/", saved::page_key(&dir)))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let view = view_in(&page);
    assert_eq!(view["export"], false, "a live page offers the download");
    assert!(
        view["bar"]["timer"]["active_ms"].is_u64(),
        "one timer for the run"
    );
    assert_eq!(
        view_at(&format!("{url}/{}/view", saved::page_key(&dir))).await,
        view
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
    assert_eq!(view_in(&html)["export"], true, "an export has no button");

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

/// A hub on a copy of the good fixture's annotation specs, its `build-identity` and `install` hooks replaced by
/// `identity` and `install` with `{p}` standing for the platform, and its run folder.
fn hooked_hub(identity: &str, install: &str) -> (std::sync::Arc<Hub>, std::path::PathBuf) {
    hooked_hub_with(identity, install, None)
}

/// [`hooked_hub`], with annotations/002-back's prepare step replaced by `prepare` when given.
fn hooked_hub_with(
    identity: &str,
    install: &str,
    prepare: Option<&str>,
) -> (std::sync::Arc<Hub>, std::path::PathBuf) {
    hooked_hub_run(identity, install, prepare, false)
}

/// [`hooked_hub_with`], its platforms running side by side when `parallel`.
fn hooked_hub_run(
    identity: &str,
    install: &str,
    prepare: Option<&str>,
    parallel: bool,
) -> (std::sync::Arc<Hub>, std::path::PathBuf) {
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
    if let Some(prepare) = prepare {
        let spec = suite_dir.join("annotations/002-back.spec.md");
        let text = std::fs::read_to_string(&spec).unwrap().replace(
            r#"  - echo '{"context":"seeded"}'"#,
            &format!("  - {prepare}"),
        );
        std::fs::write(&spec, text).unwrap();
    }

    // The platform whose block a line is in, so each block's hooks name their own.
    let mut platform = String::new();
    let config: String = std::fs::read_to_string(good.join("suite.yaml"))
        .unwrap()
        .lines()
        .map(|line| {
            if let Some(name) = line
                .strip_prefix("  ")
                .and_then(|l| l.strip_suffix(':'))
                .filter(|n| !n.starts_with(' '))
            {
                platform = name.to_string();
            }

            let hook = line.trim_start();
            if hook.starts_with("build-identity:") {
                format!(
                    r#"      build-identity: "{}""#,
                    identity.replace("{p}", &platform)
                )
            } else if hook.starts_with("install:") {
                format!(r#"      install: "{}""#, install.replace("{p}", &platform))
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
                outcomes: soundcheck::run::outcomes_by_platform(s, &s.front.platforms),
            })
            .collect(),
        parallel,
    };
    let run_dir = base.join("reports/run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let run = Run::open(plan, &run_dir.join("journal.jsonl"), serve::now_ms()).unwrap();
    let hub = Hub::without_devices(
        suite,
        run,
        "builds".into(),
        run_dir.clone(),
        120_000,
        vec![ANDROID, IOS],
    );
    (hub, run_dir)
}

#[tokio::test]
async fn the_build_identity_hook_records_each_platforms_build() {
    // iOS has no build installed until the install hook runs: its identity hook fails.
    let (hub, _) = hooked_hub(
        r#"test {p} = 'ios' && ! test -f installed-ios && exit 1; echo '{\"commit\": \"abc1234def\", \"fingerprint\": \"fp-{p}\", \"version\": \"2.16.2\"}'"#,
        "touch installed-{p}",
    );

    let problems = hub.read_builds(&[ANDROID, IOS]).await;
    assert_eq!(problems.len(), 1);
    assert!(problems[0].starts_with("ios:"), "{problems:?}");
    let android = hub
        .with_run(|run, _| Ok(run.state().builds.get(&ANDROID).cloned()))
        .unwrap()
        .unwrap();
    assert_eq!(android.commit, "abc1234def");
    assert_eq!(android.fingerprint.as_deref(), Some("fp-android"));
    assert_eq!(android.label(), "2.16.2 (abc1234)");

    // Reading the same build again changes nothing.
    let before = hub.with_run(|run, _| Ok(run.last_seq())).unwrap();
    assert!(hub.read_builds(&[ANDROID]).await.is_empty());
    assert_eq!(hub.with_run(|run, _| Ok(run.last_seq())).unwrap(), before);

    // The install hook builds and installs it; then it can be identified.
    hub.install(IOS).await.unwrap();
    assert!(hub.read_builds(&[IOS]).await.is_empty());
}

/// The run's events, once `done` holds for them (at most 10 s).
async fn events_until(hub: &Hub, done: impl Fn(&[Event]) -> bool) -> Vec<Event> {
    for _ in 0..100 {
        let events: Vec<Event> = hub
            .with_run(|run, _| {
                Ok(run
                    .entries_after(0)
                    .iter()
                    .map(|e| e.event.clone())
                    .collect())
            })
            .unwrap();
        if done(&events) {
            return events;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the events never got there");
}

#[tokio::test]
async fn an_install_outlives_the_call_that_started_it_and_ends_in_an_event() {
    // Android's install takes a second and works; iOS's fails.
    let (hub, _) = hooked_hub(
        r#"test -f installed-{p} || exit 1; echo '{\"commit\": \"abc1234def\", \"fingerprint\": \"fp-{p}\"}'"#,
        "test {p} = ios && echo 'no simulator' >&2 && exit 3; sleep 1; touch installed-{p}",
    );
    let server = serve::start(hub.clone(), 0).await.unwrap();
    let client = client(&format!("http://{}", server.addr)).await;

    let started = call(&client, "install_build", json!({ "platform": "android" })).await;
    assert_eq!(started["installing"], "android", "{started}");
    let again = call(&client, "install_build", json!({ "platform": "android" })).await;
    assert!(
        again
            .as_str()
            .is_some_and(|t| t.contains("already installing")),
        "{again}"
    );
    // The old shape, which recorded an install made outside soundcheck, is refused by name.
    let old = call(
        &client,
        "install_build",
        json!({ "platform": "ios", "install": false }),
    )
    .await;
    assert!(
        old.as_str()
            .is_some_and(|t| t.contains("unknown field `install`")),
        "{old}"
    );
    call(&client, "install_build", json!({ "platform": "ios" })).await;
    // The session ends mid-install, as Claude Code's did in run 9decb1a9.
    client.cancel().await.unwrap();

    let events = events_until(&hub, |events| {
        events
            .iter()
            .any(|e| matches!(e, Event::BuildInstalled { .. }))
            && events
                .iter()
                .any(|e| matches!(e, Event::InstallFailed { .. }))
    })
    .await;
    let installs: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::InstallStarted { platform } => Some(format!("started {platform}")),
            Event::InstallFailed { platform, reason } => {
                Some(format!("failed {platform}: {reason}"))
            }
            Event::BuildInstalled {
                platform, build, ..
            } => Some(format!("installed {platform} {}", build.commit)),
            _ => None,
        })
        .collect();
    assert_eq!(installs[0], "started android");
    assert_eq!(installs[1], "started ios");
    assert!(
        installs[2].starts_with("failed ios: the install hook failed on ios"),
        "{installs:?}"
    );
    assert_eq!(installs[3], "installed android abc1234def");
    server.shutdown.cancel();
}

#[tokio::test]
async fn a_platform_takes_no_spec_while_its_build_installs() {
    let (hub, _) = hooked_hub(
        r#"test -f installed-{p} || exit 1; echo '{\"commit\": \"abc1234def\", \"fingerprint\": \"fp-{p}\"}'"#,
        "sleep 2; touch installed-{p}",
    );
    let server = serve::start(hub.clone(), 0).await.unwrap();
    let url = format!("http://{}", server.addr);
    let client = client(&url).await;

    call(&client, "install_build", json!({ "platform": "android" })).await;

    let named = dispatch(
        &hub,
        json!({ "spec": "annotations/002-back", "platform": "android" }),
    )
    .await;
    assert_eq!(named, "refused: held: installing a build");
    let ready = reqwest::get(format!("{url}/ready/android")).await.unwrap();
    assert_eq!(ready.status().as_u16(), 409);
    assert!(ready.text().await.unwrap().contains("installing a build"));
    // A dispatch of the next row passes android over: it gets as far as iOS, which has no build either.
    let next = dispatch(&hub, json!({})).await;
    assert!(
        next.as_str()
            .is_some_and(|t| t.contains("which build is on ios") && !t.contains("held")),
        "{next}"
    );

    events_until(&hub, |events| {
        events
            .iter()
            .any(|e| matches!(e, Event::BuildInstalled { .. }))
    })
    .await;
    let ready = reqwest::get(format!("{url}/ready/android")).await.unwrap();
    assert_eq!(ready.status().as_u16(), 200);
    server.shutdown.cancel();
}

#[tokio::test]
async fn a_failed_install_holds_its_platform_until_a_build_reads_again() {
    // Every install fails, and a platform's build reads only once `installed-<platform>` exists.
    let (hub, run_dir) = hooked_hub(
        r#"test -f installed-{p} || exit 1; echo '{\"commit\": \"abc1234def\", \"fingerprint\": \"fp-{p}\"}'"#,
        "echo 'no emulator.' >&2; exit 3",
    );
    let suite_dir = run_dir.parent().unwrap().parent().unwrap().join("suite");
    let server = serve::start(hub.clone(), 0).await.unwrap();
    let url = format!("http://{}", server.addr);
    let client = client(&url).await;

    call(&client, "install_build", json!({ "platform": "android" })).await;
    events_until(&hub, |events| {
        events
            .iter()
            .any(|e| matches!(e, Event::InstallFailed { .. }))
    })
    .await;

    let named = dispatch(
        &hub,
        json!({ "spec": "annotations/002-back", "platform": "android" }),
    )
    .await;
    assert!(
        named.as_str().is_some_and(
            |t| t.contains("held: install failed — the install hook failed on android")
        ),
        "{named}"
    );
    assert!(
        !named.to_string().contains("soundcheck ready"),
        "nobody is told to wait for it: the server dispatches again by itself: {named}"
    );
    let ready = reqwest::get(format!("{url}/ready/android")).await.unwrap();
    assert_eq!(ready.status().as_u16(), 409);
    let next = dispatch(&hub, json!({})).await;
    assert!(
        next.as_str()
            .is_some_and(|t| t.contains("which build is on ios") && !t.contains("held")),
        "the next row passes android over for iOS, which has no build either: {next}"
    );
    let view = view_at(&format!("{url}/{}/view", hub.page_key())).await;
    assert!(
        view.to_string().contains("android: install failed"),
        "the page says why: {view}"
    );
    assert!(
        view.to_string().contains("no emulator. Nothing runs there"),
        "one full stop: {view}"
    );
    let status = call(&client, "status", json!({})).await;
    assert!(
        status["installs"]["android"]
            .as_str()
            .is_some_and(|s| s.starts_with("install failed — ") && s.ends_with("no emulator")),
        "{status}"
    );

    // The old build is still there after all: the hold lifts and it's logged.
    std::fs::write(suite_dir.join("installed-android"), "").unwrap();
    let ready = reqwest::get(format!("{url}/ready/android")).await.unwrap();
    assert_eq!(ready.status().as_u16(), 200);
    assert!(
        hub.with_run(|run, _| Ok(run.state().installs.is_empty()))
            .unwrap()
    );
    server.shutdown.cancel();
}

/// A hub whose builds always read and whose installs work at once, its platforms side by side: what a queued install
/// needs around it.
fn installable_hub() -> (std::sync::Arc<Hub>, std::path::PathBuf) {
    hooked_hub_run(
        r#"echo '{\"commit\": \"abc1234def\", \"fingerprint\": \"fp-{p}\"}'"#,
        "touch installed-{p}",
        None,
        true,
    )
}

/// What the journal says of installs and of 001-draw's claim on android, in order.
fn install_story(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Posted { .. } => Some("posted".to_string()),
            Event::Reviewed { .. } => Some("reviewed".to_string()),
            Event::InstallStarted { platform } => Some(format!("started {platform}")),
            Event::InstallFailed { platform, reason } => {
                Some(format!("failed {platform}: {reason}"))
            }
            Event::BuildInstalled {
                platform, reruns, ..
            } if !reruns.is_empty() => Some(format!("installed {platform}")),
            _ => None,
        })
        .collect()
}

/// The reruns every queued install here asks for, so its `build-installed` is told from the ones a dispatch logs.
fn rerun_draw() -> Value {
    json!([{ "spec": DRAW, "reason": "the fix touches undo" }])
}

#[tokio::test]
async fn an_install_asked_for_mid_spec_is_queued_and_starts_once_the_spec_is_filed() {
    let (hub, _) = installable_hub();
    let server = serve::start(hub.clone(), 0).await.unwrap();
    let url = format!("http://{}", server.addr);
    let client = client(&url).await;
    let running = dispatch(&hub, json!({ "spec": DRAW, "platform": "android" })).await;
    let token = running["token"].as_str().expect("an attempt").to_string();

    let queued = call(
        &client,
        "install_build",
        json!({ "platform": "android", "reruns": rerun_draw() }),
    )
    .await;
    assert_eq!(queued["queued"], "android", "{queued}");
    assert!(
        queued["result"].as_str().is_some_and(|t| t
            .contains("annotations/001-draw still running or waiting for review")
            && t.contains("nothing new starts on android")),
        "{queued}"
    );

    // Nothing new starts on the platform that waits to install, and the others carry on.
    let held = dispatch(
        &hub,
        json!({ "spec": "annotations/002-back", "platform": "android" }),
    )
    .await;
    assert!(
        held.as_str().is_some_and(|t| t.starts_with("refused: ")),
        "{held}"
    );
    let ready = reqwest::get(format!("{url}/ready/android")).await.unwrap();
    assert_eq!(ready.status().as_u16(), 409);
    assert!(
        ready
            .text()
            .await
            .unwrap()
            .contains("waiting to install a build once its spec is settled")
    );
    let elsewhere = dispatch(&hub, json!({ "spec": DRAW, "platform": "ios" })).await;
    assert!(elsewhere["token"].is_string(), "{elsewhere}");
    assert_eq!(
        install_story(&events_until(&hub, |_| true).await),
        [] as [&str; 0],
        "nothing is installed under a running spec"
    );

    // The pass files the row, and the install starts with no second call.
    pass_draw(&client, &token).await;
    let events = events_until(&hub, |events| {
        install_story(events).contains(&"installed android".to_string())
    })
    .await;
    assert_eq!(
        install_story(&events),
        ["posted", "started android", "installed android"]
    );
    let rerun = events.iter().any(|e| {
        matches!(e, Event::BuildInstalled { platform: ANDROID, reruns, .. }
            if reruns.iter().any(|r| r.spec == DRAW))
    });
    assert!(
        rerun,
        "the queued install keeps the reruns it was asked for"
    );
    // The hold lifts with the install: the platform takes a spec again.
    let ready = reqwest::get(format!("{url}/ready/android")).await.unwrap();
    assert_eq!(ready.status().as_u16(), 200);
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn a_second_install_is_refused_while_one_is_queued() {
    let (hub, _) = installable_hub();
    let server = serve::start(hub.clone(), 0).await.unwrap();
    let client = client(&format!("http://{}", server.addr)).await;
    dispatch(&hub, json!({ "spec": DRAW, "platform": "android" })).await;

    let queued = call(&client, "install_build", json!({ "platform": "android" })).await;
    assert_eq!(queued["queued"], "android", "{queued}");
    let again = call(&client, "install_build", json!({ "platform": "android" })).await;
    assert!(
        again
            .as_str()
            .is_some_and(|t| t.contains("android already has an install queued")),
        "{again}"
    );
    // A platform with nothing running isn't caught up in it: its install starts at once.
    let free = call(&client, "install_build", json!({ "platform": "ios" })).await;
    assert_eq!(free["installing"], "ios", "{free}");
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn a_queued_install_waits_for_a_claim_to_be_reviewed() {
    let (hub, run_dir) = installable_hub();
    let server = serve::start(hub.clone(), 0).await.unwrap();
    let client = client(&format!("http://{}", server.addr)).await;
    let running = dispatch(&hub, json!({ "spec": DRAW, "platform": "android" })).await;
    fail_draw(&client, &run_dir, running["token"].as_str().unwrap()).await;

    // Nothing runs on android now, but its claim isn't settled: the install is queued all the same.
    let queued = call(
        &client,
        "install_build",
        json!({ "platform": "android", "reruns": rerun_draw() }),
    )
    .await;
    assert_eq!(queued["queued"], "android", "{queued}");
    // With no spec running there, only the queued install keeps the next row off the platform.
    let held = dispatch(
        &hub,
        json!({ "spec": "annotations/002-back", "platform": "android" }),
    )
    .await;
    assert_eq!(
        held,
        "refused: held: waiting to install a build once its spec is settled"
    );
    let next = dispatch(&hub, json!({})).await;
    assert_ne!(next["platform"], "android", "{next}");
    assert!(
        next["token"].is_string(),
        "another platform takes the next row: {next}"
    );

    let reviewed = call(
        &client,
        "review",
        json!({
            "spec": DRAW,
            "platform": "android",
            "call": { "call": "uphold", "title": "Undo leaves the stroke on screen" },
            "reason": "screenshot shows the stroke after undo"
        }),
    )
    .await;
    assert!(reviewed.is_object(), "{reviewed}");
    let events = events_until(&hub, |events| {
        install_story(events).contains(&"installed android".to_string())
    })
    .await;
    // The order is the point: an install that started under the claim would come before its review.
    assert_eq!(
        install_story(&events),
        ["posted", "reviewed", "started android", "installed android"]
    );
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

/// A hub whose 002-back resets only once `prepared` exists in the suite folder, and that folder.
fn wedged_hub() -> (std::sync::Arc<Hub>, std::path::PathBuf) {
    let (hub, run_dir) = hooked_hub_with(
        r#"echo '{\"commit\": \"abc1234def\", \"fingerprint\": \"fp-{p}\"}'"#,
        "true",
        Some("test -f prepared"),
    );
    let suite_dir = run_dir.parent().unwrap().parent().unwrap().join("suite");
    (hub, suite_dir)
}

/// Dispatches 002-back on android, returning the reply.
async fn dispatch_back(hub: &Arc<Hub>) -> Value {
    dispatch(
        hub,
        json!({ "spec": "annotations/002-back", "platform": "android" }),
    )
    .await
}

#[tokio::test]
async fn a_failed_reset_restarts_the_device_and_files_nothing() {
    let (hub, suite_dir) = wedged_hub();
    let server = serve::start(hub.clone(), 0).await.unwrap();
    let url = format!("http://{}", server.addr);

    let refused = dispatch_back(&hub).await;
    assert!(
        refused.as_str().is_some_and(|t| t.contains(
            "held: restarting the android device after annotations/002-back: its reset failed"
        ) && t.contains("still queued")),
        "{refused}"
    );
    events_until(&hub, |events| {
        events
            .iter()
            .any(|e| matches!(e, Event::DeviceRestarted { .. }))
    })
    .await;
    let ready = reqwest::get(format!("{url}/ready/android")).await.unwrap();
    assert_eq!(ready.status().as_u16(), 200);

    // The restart is what it needed: the next dispatch starts, and nothing was filed.
    std::fs::write(suite_dir.join("prepared"), "").unwrap();
    let dispatched = dispatch_back(&hub).await;
    assert!(dispatched["token"].is_string(), "{dispatched}");
    let events = events_until(&hub, |_| true).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ResetFailed { .. }))
    );
    server.shutdown.cancel();
}

#[tokio::test]
async fn a_reset_that_fails_on_a_restarted_device_is_filed_blocked() {
    let (hub, _) = wedged_hub();
    let server = serve::start(hub.clone(), 0).await.unwrap();
    let url = format!("http://{}", server.addr);

    dispatch_back(&hub).await;
    events_until(&hub, |events| {
        events
            .iter()
            .any(|e| matches!(e, Event::DeviceRestarted { .. }))
    })
    .await;
    // Wait for the restart's hold to clear before dispatching again.
    while reqwest::get(format!("{url}/ready/android"))
        .await
        .unwrap()
        .status()
        .as_u16()
        != 200
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let filed = dispatch_back(&hub).await;
    assert!(
        filed
            .as_str()
            .is_some_and(|t| t.contains("failed on a restarted device too")),
        "{filed}"
    );
    let events = events_until(&hub, |_| true).await;
    let resets = events
        .iter()
        .filter(|e| matches!(e, Event::ResetFailed { .. }))
        .count();
    assert_eq!(resets, 1);
    server.shutdown.cancel();
}

#[tokio::test]
async fn an_install_cut_off_by_a_server_stop_holds_its_platform() {
    let (hub, _) = hooked_hub("exit 1", "true");
    // As a journal reads after the server stopped mid-install: started, never ended.
    hub.with_run(|run, now| run.install_started(now, ANDROID))
        .unwrap();
    let held = hub.hold(ANDROID).await.unwrap();
    assert!(held.starts_with("its install was cut off"), "{held}");
}

#[tokio::test]
async fn only_an_uncommitted_run_tells_the_build_identity_hook_so() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!("soundcheck-uncommitted-{nanos}"));
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
    // As a project's hook does: a dirty tree's build is refused unless the run is a trial.
    let config: String = std::fs::read_to_string(good.join("suite.yaml"))
        .unwrap()
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("build-identity:") {
                r#"      build-identity: "test \"$SOUNDCHECK_UNCOMMITTED\" = 1 || { echo 'uncommitted changes' >&2; exit 1; }; echo '{\"commit\": \"abc1234def\", \"version\": \"2.16.2\"}'""#.to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(suite_dir.join("suite.yaml"), config).unwrap();
    for uncommitted in [false, true] {
        let suite = suite::load(&suite_dir);
        assert!(suite.problems.is_empty(), "{:?}", suite.problems);
        let plan = Plan {
            specs: Vec::new(),
            parallel: false,
        };
        let run_dir = base.join(format!("reports/{uncommitted}"));
        std::fs::create_dir_all(&run_dir).unwrap();
        let run = Run::open(plan, &run_dir.join("journal.jsonl"), serve::now_ms()).unwrap();
        let mut hub = Hub::without_devices(
            suite,
            run,
            "trial".into(),
            run_dir,
            120_000,
            vec![ANDROID, IOS],
        );
        std::sync::Arc::get_mut(&mut hub).unwrap().uncommitted = uncommitted;
        let read = hub.read_build(ANDROID).await;

        if uncommitted {
            assert_eq!(read.unwrap().commit, "abc1234def");
        } else {
            let refused = format!("{:#}", read.unwrap_err());
            assert!(refused.contains("uncommitted changes"), "{refused}");
        }
    }
}

#[tokio::test]
async fn each_dispatch_logs_a_build_installed_since_without_rerunning_anything() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!("soundcheck-probe-{nanos}"));
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
    // Whatever is "installed" is the commit in a file, so the test can install a build behind the run's back.
    let config: String = std::fs::read_to_string(good.join("suite.yaml"))
        .unwrap()
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("build-identity:") {
                r#"      build-identity: "printf '{\"commit\": \"%s\"}' \"$(cat installed)\"""#
                    .to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(suite_dir.join("suite.yaml"), config).unwrap();
    std::fs::write(suite_dir.join("installed"), "aaaaaaa1").unwrap();
    let suite = suite::load(&suite_dir);
    assert!(suite.problems.is_empty(), "{:?}", suite.problems);
    let plan = Plan {
        specs: suite
            .specs
            .iter()
            .map(|s| PlannedSpec {
                id: s.id.clone(),
                platforms: s.front.platforms.clone(),
                outcomes: soundcheck::run::outcomes_by_platform(s, &s.front.platforms),
            })
            .collect(),
        parallel: true,
    };
    let run_dir = base.join("reports/run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let run = Run::open(plan, &run_dir.join("journal.jsonl"), serve::now_ms()).unwrap();
    let hub = Hub::without_devices(
        suite,
        run,
        "probe".into(),
        run_dir,
        120_000,
        vec![ANDROID, IOS],
    );
    let android = |hub: &std::sync::Arc<Hub>| {
        hub.with_run(|run, _| Ok(run.state().builds.get(&ANDROID).cloned()))
            .unwrap()
            .map(|b| b.commit)
    };

    hub.dispatch(Some(DRAW), Some(ANDROID)).await.unwrap();
    assert_eq!(android(&hub).as_deref(), Some("aaaaaaa1"));

    std::fs::write(base.join("suite/installed"), "bbbbbbb2").unwrap();
    let before = hub.with_run(|run, _| Ok(run.last_seq())).unwrap();
    hub.dispatch(Some(DRAW), Some(IOS)).await.unwrap();
    let logged = hub
        .with_run(|run, _| Ok(run.entries_after(before).to_vec()))
        .unwrap();
    assert!(
        logged.iter().any(|e| matches!(&e.event,
            Event::BuildInstalled { platform: IOS, build, reruns }
                if build.commit == "bbbbbbb2" && reruns.is_empty())),
        "the new build is logged, and reruns nothing"
    );
    assert_eq!(
        android(&hub).as_deref(),
        Some("aaaaaaa1"),
        "only the platform dispatched to is probed"
    );
}

#[tokio::test]
async fn simultaneous_next_dispatches_each_take_a_free_platform() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/good");
    let suite = suite::load(&root);
    let platforms = [ANDROID, IOS];
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
                outcomes: soundcheck::run::outcomes_by_platform(s, &s.front.platforms),
            })
            .collect(),
        parallel: true,
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soundcheck-parallel-{nanos}/run"));
    std::fs::create_dir_all(&dir).unwrap();
    let run = Run::open(plan, &dir.join("journal.jsonl"), serve::now_ms()).unwrap();
    let hub = Hub::without_devices(
        suite,
        run,
        "parallel".into(),
        dir,
        120_000,
        platforms.to_vec(),
    );

    // As the run skill sends them: one `next` per free platform, all at once.
    let (a, b) = tokio::join!(hub.dispatch(None, None), hub.dispatch(None, None));
    let mut started: Vec<String> = [a, b]
        .into_iter()
        .map(|d| d.unwrap().token.rsplit('@').next().unwrap().to_string())
        .collect();
    started.sort();
    assert_eq!(started, ["android", "ios"]);

    // With every platform busy, the next is refused for that reason alone.
    let third = hub.dispatch(None, None).await.unwrap_err().to_string();
    assert!(!third.contains("starting another attempt"), "{third}");
}

#[test]
fn a_run_is_found_again_by_its_id() {
    let id = saved::new_id();
    assert_eq!(id.len(), 36);
    assert_eq!(&id[14..15], "4", "a version-4 UUID");
    assert_ne!(id, saved::new_id());

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let reports = std::env::temp_dir().join(format!("soundcheck-ids-{nanos}"));
    let run_dir = reports.join("20261028-174254");
    std::fs::create_dir_all(&run_dir).unwrap();
    saved::write_info(
        &run_dir,
        &saved::RunInfo {
            id: id.clone(),
            suite: reports.clone(),
            label: "Release 5".into(),
            platforms: vec![IOS],
            scope: vec![],
            uncommitted: false,
        },
    )
    .unwrap();

    assert_eq!(saved::find_run(&reports, Path::new(&id)).unwrap(), run_dir);
    assert_eq!(
        saved::find_run(&reports, Path::new(&id[..8])).unwrap(),
        run_dir,
        "a unique start of the id is enough"
    );
    assert_eq!(saved::find_run(&reports, &run_dir).unwrap(), run_dir);
    assert!(saved::find_run(&reports, Path::new("ffffffff-none")).is_err());
}

#[tokio::test]
async fn a_tester_can_run_the_projects_data_checks() {
    let (server, url) = server().await;
    let client = client(&url).await;
    let dispatched = dispatch_at(
        &url,
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
    let dispatched = dispatch_at(
        &url,
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
    let dispatched = dispatch_at(
        &url,
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
    let dispatched = dispatch_at(
        &url,
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
    let dispatched = dispatch_at(
        &url,
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
        id: saved::new_id(),
        suite: root.clone(),
        label: "v0.0.0 · test".into(),
        platforms: vec![ANDROID, IOS],
        scope: vec!["annotations/001-draw".into()],
        uncommitted: false,
    };
    let dir = base.join("reports").join("20261021-090000");
    std::fs::create_dir_all(&dir).unwrap();
    saved::save(&dir, &info, &suite).unwrap();
    let plan = saved::plan_for(&suite, &info.platforms, &info.scope).unwrap();
    let run = Run::open(plan, &dir.join("journal.jsonl"), serve::now_ms()).unwrap();
    let hub = Hub::without_devices(
        suite,
        run,
        info.label.clone(),
        dir.clone(),
        120_000,
        info.platforms.clone(),
    );
    let server = serve::start(hub.clone(), 0).await.unwrap();
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
    let dispatched = dispatch(
        &hub,
        json!({ "spec": "annotations/003-erase", "platform": "ios" }),
    )
    .await;
    assert!(
        dispatched["prompt"]
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
    assert!(reloaded.row("annotations/003-erase", IOS).is_some());
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
    let dispatched = dispatch(
        &hub,
        json!({ "spec": "annotations/002-back", "platform": "android" }),
    )
    .await;
    assert!(
        dispatched["prompt"]
            .as_str()
            .is_some_and(|b| b.contains("tap back twice, a second apart")),
        "the rerun gets the edited spec: {dispatched}"
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
    let first = dispatch_at(&url, json!({})).await;
    assert_eq!(first["token"], format!("{DRAW}#1@android"));
    let refused = dispatch_at(&url, json!({})).await;
    assert!(
        refused.as_str().unwrap().contains("one spec at a time"),
        "{refused}"
    );

    pass_draw(&client, first["token"].as_str().unwrap()).await;
    let back = dispatch_at(&url, json!({ "platform": "android" })).await;
    assert_eq!(back["token"], "annotations/002-back#1@android");
    assert_eq!(back["agent"], agents::MECHANICS_TESTER);
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn a_tester_is_launched_with_every_tool_the_server_has_for_testers() {
    let (_server, url) = server().await;
    let client = client(&url).await;
    let definitions = agents::definitions();
    let given = |agent: &str| -> Vec<String> {
        definitions[agent]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_string())
            .collect()
    };
    let (plain, mechanics) = (given(agents::TESTER), given(agents::MECHANICS_TESTER));
    let mut for_testers = 0;
    for tool in client.list_all_tools().await.unwrap() {
        let name = format!("mcp__soundcheck__{}", tool.name);
        let description = tool.description.as_deref().unwrap_or_default();

        // The server says who each tool is for at the start of its description.
        if !description.starts_with("Tester") {
            assert!(!mechanics.contains(&name), "{name} isn't a tester's tool");
            continue;
        }

        for_testers += 1;
        assert!(mechanics.contains(&name), "a tester is never given {name}");
        let mechanic = soundcheck_driver::TOOLS
            .iter()
            .any(|t| t.mcp_name() == tool.name && t.kind == soundcheck_driver::Kind::Mechanic);
        assert_eq!(
            plain.contains(&name),
            !mechanic,
            "{name}: only a spec that lists a mechanic gets it"
        );
    }

    assert!(for_testers > 20, "the server lists its testers' tools");
}

#[tokio::test]
async fn a_dispatch_names_the_specs_tester_and_model() {
    let (server, url) = server().await;
    let back = dispatch_at(
        &url,
        json!({ "spec": "annotations/002-back", "platform": "android" }),
    )
    .await;
    assert_eq!(back["token"], "annotations/002-back#1@android");
    assert_eq!(
        back["agent"],
        agents::MECHANICS_TESTER,
        "the spec lists driver tools"
    );
    assert_eq!(back["model"], "opus", "the spec's model");
    server.shutdown.cancel();
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
async fn the_run_stays_open_until_it_is_closed() {
    let (server, url, dir, hub) = server_hub(&[ANDROID, IOS]).await;
    let client = client(&url).await;
    for _ in 0..2 {
        let d = dispatch(&hub, json!({ "spec": DRAW })).await;
        pass_draw(&client, d["token"].as_str().unwrap()).await;
    }
    let status = call(&client, "status", json!({})).await;
    assert_eq!(status["finished"], false, "a row is still untested");

    let back = dispatch(&hub, json!({})).await;
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
    assert_eq!(
        status["finished"], false,
        "filing the last row doesn't close the run"
    );

    call(&client, "finish", json!({})).await;
    let status = call(&client, "status", json!({})).await;
    assert_eq!(status["finished"], true);
    let report = std::fs::read_to_string(dir.join("report.html")).unwrap();
    assert!(report.contains("Final report, exported"));
    assert!(!report.contains("used 9876 tokens"));

    // The last tester's process ends after its result has closed the run.
    let spent = Spent {
        input: 1,
        cache_write: 9_000,
        cache_read: 0,
        output: 875,
    };
    hub.tester_done(
        back["token"].as_str().unwrap(),
        Ended {
            error: None,
            usage: Some((9_876, spent)),
        },
    );
    let report = std::fs::read_to_string(dir.join("report.html")).unwrap();
    assert!(
        report.contains("used 9876 tokens"),
        "the last tester's tokens reach the report"
    );
    assert!(report.contains("Final report, exported"));
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn a_starting_run_shows_its_checks_and_dispatches_nothing() {
    let (_server, url, _, hub) = server_hub(&[ANDROID]).await;
    hub.begin_start(vec!["the suite lints clean".into()]);
    hub.start_step("android: building and installing the app");
    let refused = dispatch(&hub, json!({})).await;
    assert_eq!(
        refused,
        "refused: the run is still starting (android: building and installing the app)"
    );
    // `soundcheck ready --wait` keeps waiting too.
    let ready = reqwest::get(format!("{url}/ready/android")).await.unwrap();
    assert_eq!(ready.status().as_u16(), 409);
    assert!(ready.text().await.unwrap().contains("still starting"));
    let view = format!("{url}/{}/view", hub.page_key());
    let bar = view_at(&view).await["bar"].clone();
    assert_eq!(bar["kind"], "starting");
    assert_eq!(
        bar["step"]["text"],
        "android: building and installing the app"
    );

    hub.start_failed("the install hook failed on android");
    let bar = view_at(&view).await["bar"].clone();
    assert_eq!(bar["failed"], "the install hook failed on android");
    assert!(bar.get("step").is_none());
    let ready = reqwest::get(format!("{url}/ready/android")).await.unwrap();
    assert_eq!(ready.status().as_u16(), 409);
    assert!(
        ready
            .text()
            .await
            .unwrap()
            .contains("couldn't start: the install hook failed")
    );
    // A run that couldn't start makes way at once for the next.
    let stepped = reqwest::Client::new()
        .post(format!("{url}/step-down"))
        .send()
        .await
        .unwrap();
    assert!(stepped.status().is_success());
}

#[tokio::test]
async fn a_platform_takes_no_spec_while_its_health_is_sampled() {
    let (server, url, _, hub) = server_hub(&[ANDROID]).await;
    // As `post_result` leaves it. Its own sample is over before its reply arrives here, where no device is read,
    // so the hold is set by hand.
    hub.sampling().insert(ANDROID);
    let held = dispatch(&hub, json!({ "spec": DRAW, "platform": "android" })).await;
    assert_eq!(
        held,
        "refused: held: sampling the device's health after the last spec"
    );
    let ready = reqwest::get(format!("{url}/ready/android")).await.unwrap();
    assert_eq!(ready.status().as_u16(), 409);

    hub.check_health(DRAW, ANDROID, 1).await;
    let dispatched = dispatch(&hub, json!({ "spec": DRAW, "platform": "android" })).await;
    assert!(dispatched["token"].is_string(), "{dispatched}");
    server.shutdown.cancel();
}

/// Has the server launch a stand-in for Claude Code from here on: a script that reads its prompt and then does what
/// the run folder's `fake-claude` file says. `exit` ends cleanly having done nothing; `limit` ends in error as
/// Claude does at a usage limit. The variable is the whole process's, so every test shares the one script.
fn fake_claude(run_dir: &Path, behaviour: &str) {
    static SCRIPT: std::sync::Once = std::sync::Once::new();
    SCRIPT.call_once(|| {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("soundcheck-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        // Its third argument is the agents file, which is in the run folder's `agents`.
        std::fs::write(
            &script,
            r#"#!/bin/sh
behaviour="$(cat "$(dirname "$3")/../fake-claude")"
cat > /dev/null
if [ "$behaviour" = limit ]; then
  echo '{"type":"result","is_error":true,"result":"usage limit reached"}'
  exit 1
fi
exit 0
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // No test reads this variable itself, and the server reads it only as it launches an agent.
        unsafe { std::env::set_var(agents::CLAUDE_ENV, &script) };
    });
    std::fs::write(run_dir.join("fake-claude"), behaviour).unwrap();
}

#[tokio::test]
async fn a_lane_pauses_the_run_when_its_tester_ends_without_a_single_call() {
    let (server, url, dir, hub) = server_hub(&[ANDROID]).await;
    hub.url.set(url).unwrap();
    fake_claude(&dir, "exit");
    orchestrate::start(&hub);

    let events = events_until(&hub, |events| {
        events.iter().any(|e| matches!(e, Event::Paused { .. }))
    })
    .await;
    // Long enough for a lane that hadn't stopped to dispatch again.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = events_until(&hub, |_| true).await;
    server.shutdown.cancel();

    let first = format!("{DRAW}#1@android");
    let at = |wanted: &dyn Fn(&Event) -> bool| events.iter().position(wanted);
    let dispatched =
        at(&|e| matches!(e, Event::Dispatched { token, .. } if *token == first)).unwrap();
    let interrupted = at(&|e| matches!(e, Event::Interrupted { token } if *token == first))
        .expect("a tester that never drove the device uses no attempt");
    let paused = at(&|e| {
        matches!(e, Event::Paused { reason }
            if reason.starts_with(orchestrate::PAUSED_BY_SERVER)
                && reason.contains("without making a single call"))
    })
    .expect("the run pauses rather than throwing every row at the same wall");
    assert!(
        dispatched < interrupted && interrupted < paused,
        "{events:?}"
    );
    assert_eq!(
        after
            .iter()
            .filter(|e| matches!(e, Event::Dispatched { .. }))
            .count(),
        1,
        "nothing more starts while the run is paused"
    );
    // The tester was launched with its brief, as the agent the dispatch named.
    let definitions: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("agents/agents.json")).unwrap())
            .unwrap();
    assert_eq!(definitions, agents::definitions());
    assert!(
        dir.join("agents/annotations-001-draw-1-android.jsonl")
            .exists()
    );
}

#[tokio::test]
async fn a_tester_stopped_by_a_usage_limit_pauses_the_run_without_using_an_attempt() {
    let (server, url, dir, hub) = server_hub(&[ANDROID]).await;
    hub.url.set(url).unwrap();
    fake_claude(&dir, "limit");
    orchestrate::start(&hub);

    let events = events_until(&hub, |events| {
        events.iter().any(|e| matches!(e, Event::Paused { .. }))
    })
    .await;
    let reason = events
        .iter()
        .find_map(|e| match e {
            Event::Paused { reason } => Some(reason.clone()),
            _ => None,
        })
        .unwrap();
    assert!(reason.contains("usage limit reached"), "{reason}");
    let token = format!("{DRAW}#1@android");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Interrupted { token: t } if *t == token)),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, Event::Stalled { .. })));

    // The lane has been woken by now: a paused run starts nothing more.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (dispatches, budget) = hub
        .with_run(|run, _| {
            let dispatches = run
                .entries_after(0)
                .iter()
                .filter(|e| matches!(e.event, Event::Dispatched { .. }))
                .count();
            Ok((dispatches, run.row(DRAW, ANDROID).map(|r| r.budget)))
        })
        .unwrap();
    assert_eq!(dispatches, 1);
    assert_eq!(budget, Some(0), "no attempt was used");
    server.shutdown.cancel();
}

/// Posts a FAIL for a running attempt of 001-draw with a screenshot and a side find, leaving both for review.
async fn fail_draw(
    client: &RunningService<RoleClient, ClientConfig>,
    dir: &Path,
    token: &str,
) -> std::path::PathBuf {
    let shot = dir.join("evidence/annotations-001-draw-1-android-after-undo.png");
    std::fs::create_dir_all(dir.join("evidence")).unwrap();
    std::fs::write(&shot, b"png").unwrap();
    let posted = call(
        client,
        "post_result",
        json!({
            "token": token,
            "judgments": [
                { "outcome": 0, "answer": "failed", "observed": "the stroke stayed after undo", "evidence": ["after-undo"] },
                { "outcome": 1, "answer": "held", "observed": "Undo greyed out after the undo" }
            ],
            "side_finds": [{ "summary": "The toolbar flickers", "observed": "it blinked twice on undo" }]
        }),
    )
    .await;
    assert_eq!(posted["claim"], "FAIL", "{posted}");
    shot
}

#[tokio::test]
async fn a_review_brief_carries_the_claim_its_evidence_and_the_rows_side_finds() {
    let (server, url, dir, hub) = server_hub(&[ANDROID]).await;
    let client = client(&url).await;
    let dispatched = dispatch(&hub, json!({ "spec": DRAW, "platform": "android" })).await;
    let shot = fail_draw(&client, &dir, dispatched["token"].as_str().unwrap()).await;

    let suite = hub.suite();
    let brief = hub
        .with_run(|run, _| Ok(orchestrate::review_brief(run, &suite, &hub, DRAW, ANDROID)))
        .unwrap();
    for part in [
        "# Review annotations/001-draw on android",
        "## The claim: FAIL",
        "The stroke disappears on undo and returns in the same place on redo.",
        "failed: the stroke stayed after undo",
        "held: Undo greyed out after the undo",
        &shot.display().to_string(),
        "Record your call with `review`, `spec` `annotations/001-draw` and `platform` `android`.",
        "- id 1: The toolbar flickers",
        "observed: it blinked twice on undo",
        "Record a call on each with `review_find` and its `id`.",
    ] {
        assert!(brief.contains(part), "no {part:?} in:\n{brief}");
    }
    assert!(shot.is_absolute());
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn a_claim_two_reviewers_leave_unsettled_is_left_for_the_coordinator() {
    let (server, url, dir, hub) = server_hub(&[ANDROID]).await;
    hub.url.set(url.clone()).unwrap();
    let client = client(&url).await;
    let dispatched = dispatch(&hub, json!({ "spec": DRAW, "platform": "android" })).await;
    fail_draw(&client, &dir, dispatched["token"].as_str().unwrap()).await;
    // Only the reviewers are wanted here: a paused run's lane starts no row.
    call(&client, "pause", json!({ "reason": "reviews only" })).await;
    fake_claude(&dir, "exit");
    orchestrate::start(&hub);

    let events = events_until(&hub, |events| {
        events.iter().any(|e| {
            matches!(
                e,
                Event::Reviewer {
                    unsettled: Some(_),
                    ..
                }
            )
        })
    })
    .await;
    let reviewers: Vec<Option<String>> = events
        .iter()
        .filter_map(|e| match e {
            Event::Reviewer {
                spec,
                platform,
                unsettled,
                ..
            } if spec == DRAW && *platform == ANDROID => Some(unsettled.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        reviewers,
        [
            None,
            Some("2 reviewers ended without settling it".to_string())
        ]
    );
    let status = call(&client, "status", json!({})).await;
    assert!(
        status.to_string().contains("needs review"),
        "the claim still waits: {status}"
    );
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[test]
fn attention_picks_out_what_needs_the_coordinator() {
    let mut attention = serve::events::Attention::default();
    let mut seq = 0;
    let mut heard = |event: Value| {
        seq += 1;
        let mut entry = json!({ "seq": seq, "at": 0 });
        entry
            .as_object_mut()
            .unwrap()
            .extend(event.as_object().unwrap().clone());
        attention
            .of(&serde_json::from_value(entry).unwrap())
            .is_some()
    };
    let build = json!({ "commit": "abc1234def" });

    assert!(
        !heard(
            json!({ "event": "build-installed", "platform": "ios", "build": build, "reruns": [] })
        ),
        "a build the run logs at its start is no news"
    );
    assert!(!heard(
        json!({ "event": "install-started", "platform": "android" })
    ));
    assert!(
        heard(
            json!({ "event": "build-installed", "platform": "android", "build": build, "reruns": [] })
        ),
        "an install the coordinator started has ended"
    );
    assert!(!heard(
        json!({ "event": "install-started", "platform": "ios" })
    ));
    assert!(heard(
        json!({ "event": "install-failed", "platform": "ios", "reason": "no simulator" })
    ));
    assert!(
        !heard(json!({ "event": "paused", "reason": "a fix first" })),
        "the coordinator's own pause is no news to it"
    );
    assert!(!heard(json!({ "event": "resumed" })));
    assert!(
        heard(
            json!({ "event": "paused", "reason": "soundcheck paused the run: a tester couldn't run (usage limit reached); resume once that's fixed" })
        ),
        "a pause the server made"
    );
    let failed =
        json!({ "event": "restart-failed", "platform": "ios", "spec": DRAW, "error": "no boot" });
    assert!(heard(failed.clone()), "a device that couldn't restart");
    assert!(
        !heard(failed.clone()),
        "its retries every minute say nothing new"
    );
    assert!(!heard(
        json!({ "event": "device-restarted", "platform": "ios", "spec": DRAW, "reason": "degraded" })
    ));
    assert!(heard(failed), "a restart that fails after one worked");
    assert!(
        !heard(json!({ "event": "reviewer", "spec": DRAW, "platform": "android", "tokens": 9 })),
        "a reviewer that settled its row"
    );
    assert!(heard(
        json!({ "event": "reviewer", "spec": DRAW, "platform": "android", "unsettled": "2 reviewers ended without settling it" })
    ));
    assert!(!heard(
        json!({ "event": "stalled", "token": "annotations/001-draw#1@android", "reason": "silent" })
    ));
}
