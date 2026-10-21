//! The one process a run needs: MCP tools for testers and the coordinator, and the event stream, on one local port.

pub mod events;
pub mod hooks;
pub mod prompt;
pub mod tools;

use crate::report;
use crate::run::Run;
use crate::run::journal::Entry;
use crate::run::model::Build;
use crate::suite::Suite;
use crate::suite::config::Platform;
use crate::suite::spec::Spec;
use anyhow::Context;
use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde::Deserialize;
use soundcheck_driver::Driver;
use soundcheck_driver::web::Web;
use soundcheck_driver::{Device, DriverError, Matcher};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub struct Hub {
    pub suite: Suite,
    /// The release under test, shown as the page's title.
    pub label: String,
    /// The run's folder: journal, evidence, summary and exports. Its siblings are earlier runs.
    pub run_dir: PathBuf,
    run: Mutex<Run>,
    events: broadcast::Sender<Entry>,
    /// The web driver, launched on the first device tool call for a web spec.
    pub web: tokio::sync::Mutex<Option<Driver<Web>>>,
    pub stall_ms: u64,
}

impl Hub {
    pub fn new(suite: Suite, run: Run, label: String, run_dir: PathBuf, stall_ms: u64) -> Arc<Hub> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Hub {
            suite,
            label,
            run_dir,
            run: Mutex::new(run),
            events,
            web: tokio::sync::Mutex::new(None),
            stall_ms,
        })
    }

    /// Runs a command against the run, then pushes whatever it journalled to the event stream.
    pub fn with_run<T>(&self, f: impl FnOnce(&mut Run, u64) -> Result<T>) -> Result<T> {
        let mut run = self.run.lock().unwrap_or_else(|e| e.into_inner());
        let before = run.last_seq();
        let result = f(&mut run, now_ms());
        let fresh = run.entries_after(before);
        for entry in fresh {
            let _ = self.events.send(entry.clone());
        }
        if !fresh.is_empty() {
            let _ = std::fs::write(
                self.run_dir.join(SUMMARY_FILE),
                report::run_summary(&run, &self.label).to_string(),
            );
        }
        result
    }

    fn page<'a>(&'a self, run: &'a Run, export: Option<report::Export>) -> report::Page<'a> {
        report::Page {
            suite: &self.suite,
            run,
            label: &self.label,
            live: export.is_none(),
            run_dir: Some(&self.run_dir),
            export,
        }
    }

    /// Runs a device operation on a platform's driver. Web's starts on first use against the suite's base-url.
    pub async fn on_device<T>(
        &self,
        platform: Platform,
        op: impl AsyncFnOnce(&Driver<Web>) -> Result<T, DriverError>,
    ) -> Result<T> {
        if platform != Platform::Web {
            anyhow::bail!(
                "device tools for {platform} aren't in soundcheck yet (its driver is still to come); only web is"
            );
        }
        let mut web = self.web.lock().await;
        if web.is_none() {
            let base_url = self
                .suite
                .config
                .as_ref()
                .and_then(|c| c.platforms.get(&Platform::Web))
                .and_then(|p| p.base_url.clone())
                .ok_or_else(|| anyhow::anyhow!("suite.yaml gives web no base-url"))?;
            *web = Some(Driver::new(Web::launch(&base_url, 1280, 800).await?));
        }
        Ok(op(web.as_ref().unwrap()).await?)
    }

    /// Resets a platform for a spec: guard, seed, relaunch, then its starts-on labels. Tried three times; the
    /// error after the last says why, and the caller files the spec BLOCKED as a reset failure.
    pub async fn prepare(&self, spec: &Spec, platform: Platform) -> Result<Option<String>> {
        let config = self
            .suite
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no suite.yaml"))?;
        let root = &self.suite.root;
        let mut last = anyhow::anyhow!("not tried");
        for _ in 0..3 {
            let attempt = async {
                hooks::run(root, &config.hooks.guard, hooks::GUARD_TIMEOUT)
                    .await
                    .context("the guard refused")?;
                let seed = hooks::fill(&config.hooks.seed, &spec.front.seed, &platform.to_string());
                let out = hooks::run(root, &seed, hooks::SEED_TIMEOUT).await?;
                if platform == Platform::Web {
                    let starts_on = spec.front.starts_on.clone();
                    self.on_device(platform, async |d| {
                        Device::launch(&d.device).await?;
                        for label in &starts_on {
                            let m = Matcher::parse(label)
                                .map_err(|e| DriverError::Invalid(e.to_string()))?;
                            d.wait(&m, 10_000).await.map_err(|_| {
                                DriverError::NotFound(format!(
                                    "starts-on {label:?} wasn't on screen"
                                ))
                            })?;
                        }
                        Ok(())
                    })
                    .await?;
                }
                anyhow::Ok(hooks::context(&out))
            };
            match attempt.await {
                Ok(context) => return Ok(context),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Asks the project's build-identity hook which build is installed on a platform.
    pub async fn read_build(&self, platform: Platform) -> Result<Build> {
        let config = self
            .suite
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no suite.yaml"))?;
        let command = hooks::fill(&config.hooks.build_identity, "", &platform.to_string());
        let out = hooks::run(&self.suite.root, &command, hooks::GUARD_TIMEOUT).await?;
        let build: Build = serde_json::from_str(out.trim()).with_context(|| {
            format!(
                "the build-identity hook should print {{\"commit\": ...}}, but printed {:?}",
                out.trim()
            )
        })?;
        if build.commit.trim().is_empty() {
            anyhow::bail!("the build-identity hook gave no commit");
        }
        Ok(build)
    }

    /// Records the build each platform has now, as the hook reports it. A platform whose hook fails is left
    /// unknown and named in the result, rather than stopping the run.
    pub async fn read_builds(&self, platforms: &[Platform]) -> Vec<String> {
        let mut problems = Vec::new();
        for &platform in platforms {
            match self.read_build(platform).await {
                Ok(build) => {
                    let result = self.with_run(|run, now| {
                        if run.state().builds.get(&platform) == Some(&build) {
                            return Ok(());
                        }
                        run.install_build(now, platform, build, vec![]).map(|_| ())
                    });
                    if let Err(e) = result {
                        problems.push(format!("{platform}: {e:#}"));
                    }
                }
                Err(e) => problems.push(format!("{platform}: {e:#}")),
            }
        }
        problems
    }

    /// Saves what the device shows now for an attempt's non-pass: a screenshot, the element tree and the errors.
    /// Returns the file names it managed; a platform without a driver yet saves nothing.
    pub async fn capture(&self, spec: &str, platform: Platform, attempt: u32) -> Vec<String> {
        if platform != Platform::Web {
            return Vec::new();
        }
        let dir = self.run_dir.join(report::EVIDENCE_DIR);
        if std::fs::create_dir_all(&dir).is_err() {
            return Vec::new();
        }
        let base = format!("{}-{attempt}-{platform}", spec.replace('/', "-"));
        let names = [
            format!("{base}-screen.png"),
            format!("{base}-tree.txt"),
            format!("{base}-errors.txt"),
        ];
        let saved = self
            .on_device(platform, async |d| {
                let mut saved = Vec::new();
                if d.device.screenshot(&dir.join(&names[0])).await.is_ok() {
                    saved.push(names[0].clone());
                }
                if let Ok(tree) = d.see(None, true).await {
                    let lines: Vec<String> = tree
                        .iter()
                        .map(|e| {
                            let (x, y) = e.tap_point();
                            format!("{x:.0},{y:.0}\t{}", e.line())
                        })
                        .collect();
                    if std::fs::write(dir.join(&names[1]), lines.join("\n")).is_ok() {
                        saved.push(names[1].clone());
                    }
                }
                let errors = d.device.errors().await.unwrap_or_default();
                let text = if errors.is_empty() {
                    "No errors since launch.".to_string()
                } else {
                    errors.join("\n")
                };
                if std::fs::write(dir.join(&names[2]), text).is_ok() {
                    saved.push(names[2].clone());
                }
                Ok(saved)
            })
            .await;
        saved.unwrap_or_default()
    }

    /// Writes the self-contained copy of the report into the run's folder and returns its path.
    pub fn export(&self, run: &Run, now: u64) -> Result<PathBuf> {
        let finished = run.state().finished;
        let html = report::document(&self.page(run, Some(report::Export { at: now, finished })));
        let path = self.run_dir.join(EXPORT_FILE);
        std::fs::write(&path, html)?;
        Ok(path)
    }

    fn stall_silent_testers(&self) {
        let silence = self.stall_ms;
        let _ = self.with_run(|run, now| {
            for token in run.stalled(now, silence) {
                run.stall(
                    now,
                    &token,
                    &format!("no tool call for {} s", silence / 1000),
                )?;
            }
            Ok(())
        });
    }
}

pub struct Server {
    pub addr: SocketAddr,
    pub shutdown: CancellationToken,
    pub handle: tokio::task::JoinHandle<()>,
}

/// Binds to 127.0.0.1 only; port 0 picks a free one.
pub async fn start(hub: Arc<Hub>, port: u16) -> Result<Server> {
    let shutdown = CancellationToken::new();
    let mcp: StreamableHttpService<tools::Tools, LocalSessionManager> = StreamableHttpService::new(
        {
            let hub = hub.clone();
            move || Ok(tools::Tools::new(hub.clone()))
        },
        Default::default(),
        StreamableHttpServerConfig::default().with_cancellation_token(shutdown.child_token()),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    let app = axum::Router::new()
        .nest_service("/mcp", mcp)
        .route("/events", get(events_stream))
        .route("/", get(page))
        .route("/fragment", get(fragment))
        .route("/files/evidence/{name}", get(evidence_file))
        .route("/runs", get(runs_index))
        .route("/runs/{name}/report.html", get(past_report))
        .layer(middleware::from_fn_with_state(addr.port(), local_only))
        .with_state(hub.clone());

    let ticker = {
        let hub = hub.clone();
        let shutdown = shutdown.clone();
        async move {
            let mut every = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = every.tick() => hub.stall_silent_testers(),
                }
            }
        }
    };
    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            tokio::spawn(ticker);
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
                .await;
        }
    });
    Ok(Server {
        addr,
        shutdown,
        handle,
    })
}

/// Only this machine's own callers: agents and the CLI send no Origin, and the live page sends this server's own.
/// Checking Host as well stops a page whose domain was rebound to 127.0.0.1.
async fn local_only(State(port): State<u16>, request: Request, next: Next) -> Response {
    if is_local(port, request.headers()) {
        next.run(request).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "soundcheck only answers this machine's own callers\n",
        )
            .into_response()
    }
}

fn is_local(port: u16, headers: &axum::http::HeaderMap) -> bool {
    let ours = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let host_ok = header(header::HOST).is_some_and(|h| ours.iter().any(|o| o == h));
    let origin_ok = header(header::ORIGIN)
        .is_none_or(|origin| ours.iter().any(|o| origin == format!("http://{o}")));
    host_ok && origin_ok
}

pub const SUMMARY_FILE: &str = "summary.json";
pub const EXPORT_FILE: &str = "report.html";

async fn page(State(hub): State<Arc<Hub>>) -> Response {
    let html = hub
        .with_run(|run, _| Ok(report::document(&hub.page(run, None))))
        .unwrap_or_default();
    axum::response::Html(html).into_response()
}

async fn fragment(State(hub): State<Arc<Hub>>) -> Response {
    let html = hub
        .with_run(|run, _| Ok(report::content(&hub.page(run, None))))
        .unwrap_or_default();
    axum::response::Html(html).into_response()
}

async fn evidence_file(State(hub): State<Arc<Hub>>, Path(name): Path<String>) -> Response {
    if !report::safe_name(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = hub.run_dir.join(report::EVIDENCE_DIR).join(&name);
    let kind = match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("json" | "jsonl") => "application/json",
        _ => "text/plain; charset=utf-8",
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, kind)], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Every run beside this one, newest first, from their summaries.
async fn runs_index(State(hub): State<Arc<Hub>>) -> Response {
    let current = hub
        .run_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut runs = Vec::new();
    if let Some(reports) = hub.run_dir.parent()
        && let Ok(entries) = std::fs::read_dir(reports)
    {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Ok(text) = std::fs::read_to_string(entry.path().join(SUMMARY_FILE))
                && let Ok(summary) = serde_json::from_str::<serde_json::Value>(&text)
            {
                let exported = entry.path().join(EXPORT_FILE).exists();
                runs.push((name, summary, exported));
            }
        }
    }
    // The current run comes from memory: before its first event it has no summary on disk.
    runs.retain(|(name, _, _)| *name != current);
    let live = hub
        .with_run(|run, _| Ok(report::run_summary(run, &hub.label)))
        .unwrap_or_default();
    runs.push((current.clone(), live, false));
    runs.sort_by(|a, b| b.0.cmp(&a.0));
    axum::response::Html(report::index(&runs, &current)).into_response()
}

async fn past_report(State(hub): State<Arc<Hub>>, Path(name): Path<String>) -> Response {
    if !report::safe_name(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(reports) = hub.run_dir.parent() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read_to_string(reports.join(&name).join(EXPORT_FILE)).await {
        Ok(html) => axum::response::Html(html).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    after: u64,
}

/// Newline-delimited journal entries after `after`, then each new one as it's written. Replaying from a cursor
/// means a coordinator whose Monitor lapsed misses nothing.
async fn events_stream(State(hub): State<Arc<Hub>>, Query(q): Query<EventsQuery>) -> Response {
    // Subscribe before reading history, so nothing written in between is lost.
    let mut live = hub.events.subscribe();
    let history: Vec<Entry> = hub
        .with_run(|run, _| Ok(run.entries_after(q.after).to_vec()))
        .unwrap_or_default();
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    tokio::spawn(async move {
        let mut last = q.after;
        let line = |e: &Entry| {
            Bytes::from(format!(
                "{}\n",
                serde_json::to_string(e).unwrap_or_default()
            ))
        };
        for entry in &history {
            last = entry.seq;
            if tx.send(Ok(line(entry))).await.is_err() {
                return;
            }
        }
        loop {
            match live.recv().await {
                Ok(entry) if entry.seq > last => {
                    last = entry.seq;
                    if tx.send(Ok(line(&entry))).await.is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let missed: Vec<Entry> = hub
                        .with_run(|run, _| Ok(run.entries_after(last).to_vec()))
                        .unwrap_or_default();
                    for entry in &missed {
                        last = entry.seq;
                        if tx.send(Ok(line(entry))).await.is_err() {
                            return;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}
