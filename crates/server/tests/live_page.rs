//! The live page in a real browser: soundcheck's own web driver watches it update as the run moves.

use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt, service::RunningService};
use serde_json::{Value, json};
use soundcheck::run::{Plan, PlannedSpec, Run};
use soundcheck::serve::{self, Hub};
use soundcheck::suite;
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
        Hub::new(suite, run, "v9.9.9 · live".into(), dir, 120_000),
        0,
    )
    .await
    .unwrap();
    let url = format!("http://{}", server.addr);

    let web = match Web::launch(&format!("{url}/"), 1200, 900).await {
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
