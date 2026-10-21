//! The one process a run needs: MCP tools for testers and the coordinator, and the event stream, on one local port.

pub mod events;
pub mod hooks;
pub mod prompt;
pub mod reports;
pub mod tools;

use crate::report;
use crate::run::Run;
use crate::run::journal::Entry;
use crate::run::model::Build;
use crate::suite::Suite;
use crate::suite::config::{Platform, PlatformConfig};
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
use soundcheck_driver::android::Android;
use soundcheck_driver::any::AnyDevice;
use soundcheck_driver::ios::Ios;
use soundcheck_driver::web::Web;
use soundcheck_driver::{Device, DriverError, Matcher};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

/// A platform's reader: `SOUNDCHECK_<PLATFORM>_READER`, or the one built in this checkout.
fn reader(platform: &str, built: &str) -> Option<PathBuf> {
    std::env::var_os(format!("SOUNDCHECK_{}_READER", platform.to_uppercase()))
        .map(PathBuf::from)
        .or_else(|| {
            let built = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../readers")
                .join(platform)
                .join("build")
                .join(built);
            built.exists().then_some(built)
        })
}

/// Connects a platform's device the way a run does: web launches its own Chrome against the suite's base-url; Android
/// and iOS connect to the running emulator or booted simulator to drive the suite's app-id. `headed` shows web's
/// Chrome in a window to watch.
pub async fn connect(
    platform: Platform,
    config: Option<&PlatformConfig>,
    headed: bool,
) -> Result<AnyDevice> {
    Ok(match platform {
        Platform::Web => {
            let base_url = config
                .and_then(|p| p.base_url.clone())
                .ok_or_else(|| anyhow::anyhow!("suite.yaml gives web no base-url"))?;
            AnyDevice::Web(Box::new(Web::launch(&base_url, 1280, 800, headed).await?))
        }
        Platform::Android => {
            let app = config
                .and_then(|p| p.app_id.clone())
                .ok_or_else(|| anyhow::anyhow!("suite.yaml gives android no app-id"))?;
            AnyDevice::Android(Android::connect(&app, reader("android", "reader.apk")).await?)
        }
        Platform::Ios => {
            let app = config
                .and_then(|p| p.app_id.clone())
                .ok_or_else(|| anyhow::anyhow!("suite.yaml gives ios no app-id"))?;
            AnyDevice::Ios(Ios::connect(&app, reader("ios", "axe-reader")).await?)
        }
    })
}

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
    /// Platforms whose devices are left alone: specs on them seed but never touch a device (the server's own tests).
    pub without_devices: Vec<Platform>,
    /// A driver per platform, connected on its first device call.
    pub devices: tokio::sync::Mutex<std::collections::HashMap<Platform, Driver<AnyDevice>>>,
    pub stall_ms: u64,
    /// Web's Chrome in a window to watch, rather than headless.
    pub headed: bool,
    /// Devices found degraded after a spec, to restart before the next: platform → (spec, reason).
    restart_due: Mutex<std::collections::HashMap<Platform, (String, String)>>,
    /// When each platform's device was last restarted, so a device that stays degraded isn't rebooted in a loop.
    last_restart: Mutex<std::collections::HashMap<Platform, u64>>,
    /// Platforms whose dispatches wait, for a restart or a busy host to settle: platform → why.
    held: Mutex<std::collections::HashMap<Platform, String>>,
    /// When a wait for the host last ran out, so a host that stays busy doesn't hold every spec.
    gave_up: Mutex<std::collections::HashMap<Platform, u64>>,
}

/// At most one restart per platform in this window; a device still degraded after that is only noted.
const RESTART_COOLDOWN_MS: u64 = 15 * 60_000;
/// How long dispatches wait for a busy host before going ahead; a simulator's boot settled in about four minutes.
const HOST_WAIT_MAX_MS: u64 = 10 * 60_000;
const HOST_WAIT_POLL_MS: u64 = 15_000;

impl Hub {
    pub fn new(suite: Suite, run: Run, label: String, run_dir: PathBuf, stall_ms: u64) -> Arc<Hub> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Hub {
            suite,
            label,
            run_dir,
            run: Mutex::new(run),
            events,
            devices: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            without_devices: Vec::new(),
            stall_ms,
            headed: false,
            restart_due: Mutex::new(std::collections::HashMap::new()),
            last_restart: Mutex::new(std::collections::HashMap::new()),
            held: Mutex::new(std::collections::HashMap::new()),
            gave_up: Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// A hub that never touches the devices of `platforms`, for testing the server where no device is attached.
    pub fn without_devices(
        suite: Suite,
        run: Run,
        label: String,
        run_dir: PathBuf,
        stall_ms: u64,
        platforms: Vec<Platform>,
    ) -> Arc<Hub> {
        let mut hub = Arc::into_inner(Hub::new(suite, run, label, run_dir, stall_ms)).unwrap();
        hub.without_devices = platforms;
        Arc::new(hub)
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

    /// Runs a device operation on a platform's driver, connecting it on first use: web launches its own Chrome
    /// against the suite's base-url; Android and iOS connect to the emulator or simulator to drive the suite's app-id.
    pub async fn on_device<T>(
        &self,
        platform: Platform,
        op: impl AsyncFnOnce(&Driver<AnyDevice>) -> Result<T, DriverError>,
    ) -> Result<T> {
        if self.without_devices.contains(&platform) {
            anyhow::bail!("this server leaves {platform}'s device alone");
        }
        let mut devices = self.devices.lock().await;
        if let std::collections::hash_map::Entry::Vacant(slot) = devices.entry(platform) {
            let config = self
                .suite
                .config
                .as_ref()
                .and_then(|c| c.platforms.get(&platform));
            let device = connect(platform, config, self.headed).await?;
            slot.insert(Driver::new(device));
        }
        Ok(op(&devices[&platform]).await?)
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
                if !self.without_devices.contains(&platform) {
                    let starts_on = spec.front.starts_on.clone();
                    let intercept = spec.front.tools.iter().any(|t| t == "network-drop");
                    self.on_device(platform, async |d| {
                        // Before the launch, so the app's connections go through the proxy that can freeze them.
                        d.device.intercept_network(intercept).await?;
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

    /// Why a dispatch on `platform` must wait, after starting what it waits for: a due restart, or a busy host. The
    /// dispatch refuses straight away rather than blocking (a tool call times out long before a boot settles), and
    /// the platform's `ready` event says when to dispatch again.
    pub async fn hold(self: &Arc<Self>, spec: &str, platform: Platform) -> Option<String> {
        if self.without_devices.contains(&platform) {
            return None;
        }
        if let Some(reason) = self
            .held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&platform)
        {
            return Some(reason.clone());
        }
        let due = self
            .restart_due
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&platform);
        if let Some((after, reason)) = due {
            let held =
                format!("restarting the {platform} device, degraded after {after} ({reason})");
            self.hold_for(platform, &held);
            let hub = self.clone();
            let spec = spec.to_string();
            tokio::spawn(async move {
                let restarted = hub
                    .on_device(platform, async |d| d.device.restart().await)
                    .await;
                hub.last_restart
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(platform, now_ms());
                if restarted.is_ok() {
                    let _ = hub
                        .with_run(|run, now| run.device_restarted(now, platform, &after, &reason));
                }
                hub.watch_host(&spec, platform, "restarted".into()).await;
            });
            return Some(held);
        }
        let gave_up_lately = self
            .gave_up
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&platform)
            .is_some_and(|at| now_ms().saturating_sub(*at) < RESTART_COOLDOWN_MS);
        if gave_up_lately {
            return None;
        }
        let busy = self.host_busy(platform).await;
        if busy.is_empty() {
            return None;
        }
        let held = format!("the Mac is busy ({})", busy.join(", "));
        self.hold_for(platform, &held);
        let hub = self.clone();
        let spec = spec.to_string();
        tokio::spawn(async move { hub.watch_host(&spec, platform, busy.join(", ")).await });
        Some(held)
    }

    fn hold_for(&self, platform: Platform, reason: &str) {
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(platform, reason.to_string());
    }

    async fn host_busy(&self, platform: Platform) -> Vec<String> {
        self.on_device(platform, async |d| d.device.health().await)
            .await
            .map(|h| h.busy)
            .unwrap_or_default()
    }

    /// Waits for the host to settle, up to [`HOST_WAIT_MAX_MS`], then releases the platform and journals its
    /// `ready` event, which is the coordinator's cue to dispatch again.
    async fn watch_host(&self, spec: &str, platform: Platform, mut reason: String) {
        let started = now_ms();
        let settled = loop {
            let busy = self.host_busy(platform).await;
            if busy.is_empty() {
                break true;
            }
            reason = busy.join(", ");
            if now_ms().saturating_sub(started) >= HOST_WAIT_MAX_MS {
                self.gave_up
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(platform, now_ms());
                break false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(HOST_WAIT_POLL_MS)).await;
        };
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&platform);
        let waited = now_ms().saturating_sub(started);
        let _ =
            self.with_run(|run, now| run.host_busy(now, platform, spec, &reason, waited, settled));
    }

    /// Samples the device's health after an attempt posted and journals it with that attempt. A degraded device is
    /// restarted before the next spec on it; a busy host is waited out before the next spec instead.
    pub async fn check_health(&self, spec: &str, platform: Platform, attempt: u32) {
        if self.without_devices.contains(&platform) {
            return;
        }
        let Ok(health) = self
            .on_device(platform, async |d| d.device.health().await)
            .await
        else {
            return;
        };
        let restarted_lately = self
            .last_restart
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&platform)
            .is_some_and(|at| now_ms().saturating_sub(*at) < RESTART_COOLDOWN_MS);
        if !health.degraded.is_empty() && !restarted_lately {
            self.restart_due
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(platform, (spec.to_string(), health.degraded.join("; ")));
        }
        let _ = self.with_run(|run, now| run.device_health(now, spec, platform, attempt, health));
    }

    /// The folder holding this run and its siblings.
    pub fn reports(&self) -> reports::Reports {
        reports::Reports {
            dir: self.run_dir.parent().map(PathBuf::from).unwrap_or_default(),
            current: self
                .run_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
        }
    }

    pub fn fixtures_dir(&self) -> PathBuf {
        let dir = self.suite.config.as_ref().and_then(|c| c.fixtures.clone());
        self.suite
            .root
            .join(dir.unwrap_or_else(|| "fixtures".into()))
    }

    /// Runs the project's check or act hook: `<hook> <name> <args...>`, each quoted for the shell. A failing call
    /// is still evidence, so its exit and output come back rather than an error.
    pub async fn project_hook(&self, kind: &str, name: &str, args: &[String]) -> Result<String> {
        let config = self
            .suite
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no suite.yaml"))?;
        let hook = if kind == "check" {
            &config.hooks.check
        } else {
            &config.hooks.act
        };
        let Some(hook) = hook else {
            anyhow::bail!("suite.yaml has no {kind} hook");
        };
        let quoted: Vec<String> = std::iter::once(name)
            .chain(args.iter().map(String::as_str))
            .map(|a| format!("'{}'", a.replace('\'', r"'\''")))
            .collect();
        let command = format!("{hook} {}", quoted.join(" "));
        match hooks::run(&self.suite.root, &command, hooks::SEED_TIMEOUT).await {
            Ok(out) => Ok(out),
            Err(e) => Ok(format!("{kind} failed: {e:#}")),
        }
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

    /// Builds and installs the current tree on a platform with the project's install hook.
    pub async fn install(&self, platform: Platform) -> Result<()> {
        let config = self
            .suite
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no suite.yaml"))?;
        let Some(install) = &config.hooks.install else {
            anyhow::bail!("suite.yaml has no install hook, so builds are installed by hand");
        };
        let command = hooks::fill(install, "", &platform.to_string());
        hooks::run(&self.suite.root, &command, hooks::INSTALL_TIMEOUT)
            .await
            .with_context(|| format!("the install hook failed on {platform}"))?;
        Ok(())
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
    /// Returns the file names it managed.
    pub async fn capture(&self, spec: &str, platform: Platform, attempt: u32) -> Vec<String> {
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

/// A server for looking at runs when none is running: the list of runs and each run's page.
pub async fn start_reports(dir: PathBuf, port: u16) -> Result<Server> {
    let shutdown = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| {
            format!("port {port} is taken; if a run's server is up, its runs are at http://127.0.0.1:{port}/runs")
        })?;
    let addr = listener.local_addr()?;
    let reports = reports::Reports { dir, current: None };
    let index = {
        let reports = reports.clone();
        move || {
            let reports = reports.clone();
            async move { axum::response::Html(report::index(&reports::list(&reports), "")) }
        }
    };
    let app = axum::Router::new()
        .route("/", get(index.clone()))
        .route("/runs", get(index))
        .merge(reports::router(reports))
        .layer(middleware::from_fn_with_state(addr.port(), local_only));
    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
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
        .with_state(hub.clone())
        .merge(reports::router(hub.reports()))
        .layer(middleware::from_fn_with_state(addr.port(), local_only));

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
    let mut runs = reports::list(&hub.reports());
    // The current run comes from memory: before its first event it has no summary on disk.
    runs.retain(|(name, _, _)| *name != current);
    let live = hub
        .with_run(|run, _| Ok(report::run_summary(run, &hub.label)))
        .unwrap_or_default();
    runs.push((current.clone(), live, None));
    runs.sort_by(|a, b| b.0.cmp(&a.0));
    axum::response::Html(report::index(&runs, &current)).into_response()
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
