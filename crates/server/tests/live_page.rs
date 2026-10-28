//! The live page in a real browser: soundcheck's own web driver watches it update as the run moves.

use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt, service::RunningService};
use serde_json::{Value, json};
use soundcheck::run::{Plan, PlannedSpec, Run};
use soundcheck::serve::{self, Hub};
use soundcheck::suite;
use soundcheck::suite::config::Platform;
use soundcheck_driver::web::Web;
use soundcheck_driver::{Device, Driver, Matcher};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

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
    assert_ne!(result.is_error, Some(true), "{tool}: {text}");
    serde_json::from_str(&text).unwrap()
}

fn m(target: &str) -> Matcher {
    Matcher::parse(target).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_live_page_follows_the_run_in_a_real_browser() {
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
        prepare: false,
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir()
        .join(format!("soundcheck-live-{nanos}"))
        .join("run");
    std::fs::create_dir_all(&dir).unwrap();
    let run = Run::open(plan, &dir.join("journal.jsonl"), serve::now_ms()).unwrap();
    let server = serve::start(
        Hub::without_devices(
            suite,
            run,
            "v9.9.9 · live".into(),
            dir,
            120_000,
            vec![Platform::Android, Platform::Ios],
        ),
        0,
    )
    .await
    .unwrap();
    let url = format!("http://{}", server.addr);

    let web = match Web::launch(&format!("{url}/"), 1200, 900, false).await {
        Ok(web) => web,
        Err(e) => {
            eprintln!("skipping: Chrome didn't start ({e})");
            server.shutdown.cancel();
            return;
        }
    };
    let page = Driver::new(web);
    page.wait(&m("v9.9.9 · live"), 5000).await.unwrap();
    page.wait(&m("Not ready"), 5000).await.unwrap();

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{url}/mcp")),
    );
    let client = ClientConfig::default().serve(transport).await.unwrap();
    let dispatched = call(
        &client,
        "dispatch",
        json!({ "spec": "annotations/001-draw", "platform": "android" }),
    )
    .await;
    // The page hears the dispatch and shows the spec running, without a reload.
    page.wait(&m("Running"), 5000).await.unwrap();

    call(
        &client,
        "post_result",
        json!({
            "token": dispatched["token"],
            "judgments": [
                { "outcome": 0, "answer": "failed", "observed": "The stroke stayed after Undo." },
                { "outcome": 1, "answer": "held", "observed": "Undo greyed out." }
            ]
        }),
    )
    .await;
    page.wait(&m("Checking"), 5000).await.unwrap();

    call(
        &client,
        "review",
        json!({
            "spec": "annotations/001-draw",
            "platform": "android",
            "call": { "call": "uphold", "title": "Undo leaves the stroke on screen" },
            "reason": "The screenshot shows the stroke."
        }),
    )
    .await;
    page.wait(&m("Failing"), 5000).await.unwrap();
    page.wait(&m("Undo leaves the stroke on screen"), 5000)
        .await
        .unwrap();
    // A failed row opens by itself, with what failed first.
    page.expect(&m("The stroke stayed after Undo."))
        .await
        .unwrap();

    // The viewer closes the row; the next update must leave it closed.
    let row = page
        .expect(&m("Draw a pen stroke, undo and redo it"))
        .await
        .unwrap();
    let (x, y) = row.tap_point();
    page.device.tap_at(x, y).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    for e in page.see(None, false).await.unwrap() {
        if e.expanded.is_some()
            || e.label.contains("Draw a pen")
            || e.label.contains("stroke stayed")
        {
            eprintln!(
                "DEBUG {:?} role={:?} expanded={:?} frame={:?}",
                e.label, e.role, e.expanded, e.frame
            );
        }
    }
    eprintln!("DEBUG tapped at {x},{y}");
    page.gone(&m("The stroke stayed after Undo."))
        .await
        .unwrap();
    call(
        &client,
        "pause",
        json!({ "reason": "checking the page keeps state" }),
    )
    .await;
    page.wait(&m("Paused"), 5000).await.unwrap();
    page.gone(&m("The stroke stayed after Undo."))
        .await
        .unwrap();

    page.device.close().await;
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tester_drives_a_web_page_through_soundcheck() {
    // A suite like the fixture, whose web app is a local page.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!("soundcheck-tester-{nanos}"));
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
    let page = base.join("app.html");
    std::fs::write(
        &page,
        r#"<!doctype html><title>App</title><button>Annotate</button><button onclick="document.getElementById('s').textContent='Saved'">Save</button><p id="s">Not saved</p>"#,
    )
    .unwrap();
    let config = std::fs::read_to_string(good.join("suite.yaml"))
        .unwrap()
        .replace(
            "http://localhost:5173",
            &format!("file://{}", page.display()),
        );
    std::fs::write(suite_dir.join("suite.yaml"), config).unwrap();

    let suite = suite::load(&suite_dir);
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
        parallel: true,
        prepare: false,
    };
    let run_dir = base.join("reports/run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let run = Run::open(plan, &run_dir.join("journal.jsonl"), serve::now_ms()).unwrap();
    let server = serve::start(
        Hub::without_devices(
            suite,
            run,
            "web".into(),
            run_dir.clone(),
            120_000,
            vec![Platform::Android, Platform::Ios],
        ),
        0,
    )
    .await
    .unwrap();
    let url = format!("http://{}", server.addr);
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{url}/mcp")),
    );
    let client = ClientConfig::default().serve(transport).await.unwrap();

    let names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .iter()
        .map(|t| t.name.to_string())
        .collect();
    for tool in soundcheck_driver::TOOLS
        .iter()
        .filter(|t| t.name != "batch")
    {
        assert!(
            names.contains(&tool.mcp_name()),
            "no MCP tool {}: {names:?}",
            tool.mcp_name()
        );
    }

    let web = call(
        &client,
        "dispatch",
        json!({ "spec": "annotations/001-draw", "platform": "web" }),
    )
    .await;
    let token = web["token"].as_str().unwrap().to_string();
    assert_eq!(
        web["agent"], "soundcheck-tester",
        "a spec with no driver tools"
    );
    let seen = client
        .call_tool(
            CallToolRequestParams::new("see")
                .with_arguments(json!({ "token": token }).as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    let text = seen.content[0].as_text().unwrap().text.clone();
    if seen.is_error == Some(true) && text.contains("Chrome") {
        eprintln!("skipping: {text}");
        server.shutdown.cancel();
        return;
    }
    assert!(text.contains("Save"), "{text}");
    let done = call(
        &client,
        "batch",
        json!({ "token": token, "steps": [
            { "tool": "expect", "target": "Not saved" },
            { "tool": "tap", "target": "Save" },
            { "tool": "wait", "target": "Saved", "timeout_ms": 3000 },
            { "tool": "check", "assertions": ["Saved", "!Not saved"] }
        ]}),
    )
    .await;
    let done = done.as_str().unwrap();
    let (steps, screen) = done.split_once("\nScreen now:\n").expect(done);
    assert_eq!(steps.split("; ").count(), 4, "{done}");
    assert!(screen.contains("Saved"), "{done}");
    let stopped = client
        .call_tool(
            CallToolRequestParams::new("batch").with_arguments(
                json!({ "token": token, "steps": [
                    { "tool": "expect", "target": "Saved" },
                    { "tool": "expect", "target": "Nope" },
                    { "tool": "tap", "target": "Save" }
                ]})
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .unwrap();
    let text = stopped.content[0].as_text().unwrap().text.clone();
    assert!(
        text.contains("stopped at step 2 of 3")
            && text.contains("done before it: found")
            && text.contains("Screen now:")
            && text.contains("Saved"),
        "{text}"
    );
    let shot = call(&client, "shot", json!({ "token": token, "name": "saved" })).await;
    assert_eq!(
        shot,
        format!(
            "saved as annotations-001-draw-1-web-saved.webp: attach this name; Read it at {}",
            run_dir
                .join("evidence/annotations-001-draw-1-web-saved.webp")
                .display()
        )
    );
    assert!(
        run_dir
            .join("evidence/annotations-001-draw-1-web-saved.webp")
            .exists()
    );

    let posted = call(
        &client,
        "post_result",
        json!({ "token": token, "judgments": [
            { "outcome": 0, "answer": "held", "observed": "Saved showed.", "evidence": ["saved"] },
            { "outcome": 1, "answer": "failed", "observed": "Redo stayed greyed out." }
        ]}),
    )
    .await;
    assert_eq!(posted["claim"], "FAIL");
    // The server saved its own evidence at once, before any review or reseed.
    let evidence = run_dir.join("evidence");
    assert!(
        evidence
            .join("annotations-001-draw-1-web-screen.webp")
            .exists()
    );
    let tree =
        std::fs::read_to_string(evidence.join("annotations-001-draw-1-web-tree.txt")).unwrap();
    assert!(
        tree.contains("Annotate") && tree.contains("Saved"),
        "{tree}"
    );
    assert!(
        evidence
            .join("annotations-001-draw-1-web-errors.txt")
            .exists()
    );
    let html = reqwest::get(format!("{url}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        html.contains("src=\"files/evidence/annotations-001-draw-1-web-saved.webp\""),
        "the page shows the screenshot"
    );
    assert!(
        html.contains("files/evidence/annotations-001-draw-1-web-tree.txt"),
        "and links the tree"
    );

    // This server leaves Android's device alone: it says so rather than pretending.
    let android = call(
        &client,
        "dispatch",
        json!({ "spec": "annotations/002-back", "platform": "android" }),
    )
    .await;
    let refused = client
        .call_tool(
            CallToolRequestParams::new("see").with_arguments(
                json!({ "token": android["token"] })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(refused.is_error, Some(true));
    assert!(
        refused.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("leaves android")
    );

    client.cancel().await.unwrap();
    server.shutdown.cancel();
}

#[tokio::test]
async fn a_seed_that_keeps_failing_files_the_spec_blocked() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!("soundcheck-reset-{nanos}"));
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
    let tries = base.join("tries");
    // Every seed fails, noting each try, so the retries can be counted.
    let config: String = std::fs::read_to_string(good.join("suite.yaml"))
        .unwrap()
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("seed:") {
                format!(
                    "  seed: \"echo try >> {} && echo 'database is down' >&2 && exit 1 # {{scenario}}\"",
                    tries.display()
                )
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(config.contains("database is down"));
    std::fs::write(suite_dir.join("suite.yaml"), config).unwrap();
    let suite = suite::load(&suite_dir);
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
        prepare: false,
    };
    let run_dir = base.join("reports/run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let run = Run::open(plan, &run_dir.join("journal.jsonl"), serve::now_ms()).unwrap();
    let server = serve::start(
        Hub::without_devices(
            suite,
            run,
            "reset".into(),
            run_dir,
            120_000,
            vec![Platform::Android, Platform::Ios],
        ),
        0,
    )
    .await
    .unwrap();
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{}/mcp", server.addr)),
    );
    let client = ClientConfig::default().serve(transport).await.unwrap();

    let result = client
        .call_tool(
            CallToolRequestParams::new("dispatch").with_arguments(
                json!({ "spec": "annotations/002-back", "platform": "android" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    let text = result.content[0].as_text().unwrap().text.clone();
    assert!(
        text.contains("filed BLOCKED") && text.contains("database is down"),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(&tries).unwrap().lines().count(),
        3,
        "tried three times"
    );

    let status = call(&client, "status", json!({})).await;
    let row = status["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["spec"] == "annotations/002-back")
        .unwrap()
        .clone();
    assert_eq!(row["result"], "BLOCKED");
    assert_eq!(row["attempts"], 0, "a reset failure uses no attempt");
    client.cancel().await.unwrap();
    server.shutdown.cancel();
}
