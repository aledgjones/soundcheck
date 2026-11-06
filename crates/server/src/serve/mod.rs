//! The one process a run needs: MCP tools for testers and the coordinator, and the event stream, on one local port.

pub mod events;
pub mod hooks;
pub mod prompt;
pub mod reports;
pub mod tools;

use crate::report;
use crate::run::event::{PreparedStep, RowRef};
use crate::run::journal::Entry;
use crate::run::model::{Build, Rerun};
use crate::run::{Install, Run, saved};
use crate::suite::Suite;
use crate::suite::config::{DriverKind, Platform, PlatformConfig, SuiteConfig, parse_viewport};
use crate::suite::spec::Spec;
use anyhow::Context;
use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde::{Deserialize, Serialize};
use soundcheck_driver::Driver;
use soundcheck_driver::android::Android;
use soundcheck_driver::any::AnyDevice;
use soundcheck_driver::ios::Ios;
use soundcheck_driver::web::Chrome;
use soundcheck_driver::{Device, DriverError};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

/// A platform's reader: `SOUNDCHECK_<PLATFORM>_READER`, the one beside a prebuilt `bin/soundcheck`
/// (`readers/<platform>/<built>`), or the one built in this checkout.
fn reader(platform: &str, built: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(format!("SOUNDCHECK_{}_READER", platform.to_uppercase())) {
        return Some(PathBuf::from(path));
    }
    let prebuilt = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .ok()
        .and_then(|exe| {
            Some(
                exe.parent()?
                    .parent()?
                    .join("readers")
                    .join(platform)
                    .join(built),
            )
        });
    let checkout = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../readers")
        .join(platform)
        .join("build")
        .join(built);
    prebuilt.into_iter().chain([checkout]).find(|p| p.exists())
}

/// Connects a platform's device the way a run does: `chrome` launches its own Chrome at the platform's `url` and
/// `viewport`; `android` and `ios` connect to the running emulator or booted simulator to drive the platform's `app`.
/// `headed` shows each device in a window to watch: Chrome, Simulator.app, and the window of an emulator soundcheck
/// boots.
pub async fn connect(
    platform: Platform,
    config: &PlatformConfig,
    headed: bool,
) -> Result<AnyDevice> {
    let app = || {
        config
            .app
            .clone()
            .ok_or_else(|| anyhow::anyhow!("suite.yaml gives {platform} no app"))
    };
    Ok(match config.driver {
        DriverKind::Chrome => {
            let url = config
                .url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("suite.yaml gives {platform} no url"))?;
            let (width, height) = config
                .viewport()
                .ok_or_else(|| anyhow::anyhow!("suite.yaml gives {platform} no viewport"))?;
            let profile = config.profile_dir();
            AnyDevice::Chrome(Box::new(
                Chrome::launch_from(&url, width, height, headed, profile.as_deref()).await?,
            ))
        }
        DriverKind::Android => AnyDevice::Android(
            Android::connect(
                &app()?,
                reader("android", "reader.apk"),
                &config.device,
                headed,
            )
            .await?,
        ),
        DriverKind::Ios => AnyDevice::Ios(
            Ios::connect(&app()?, reader("ios", "reader"), &config.device)
                .await?
                .headed(headed),
        ),
    })
}

/// A platform's driver as a run uses it: its device and the suite's network proxy for it (started if it isn't up),
/// with Android's ports forwarded to it.
pub async fn driver(platform: Platform, suite: &Suite, headed: bool) -> Result<Driver<AnyDevice>> {
    let config = suite
        .config
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{} doesn't load", crate::suite::CONFIG_FILE))?;
    let device = connect(platform, config.of(&platform), headed).await?;
    let routes = crate::network::routes(config, platform);
    let (device, network) = match (device, routes) {
        (device, None) => (device, None),
        (AnyDevice::Android(android), Some(routes)) => {
            let network = crate::network::ensure(&suite.root, platform, &routes).await?;
            let forwards = crate::network::forwards(config, platform);
            (
                AnyDevice::Android(android.forward(&forwards).await?),
                Some(network),
            )
        }
        (device, Some(routes)) => (
            device,
            Some(crate::network::ensure(&suite.root, platform, &routes).await?),
        ),
    };
    Ok(Driver::new(device).with_network(network))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Waits for the Mac to settle before a run reads or builds anything, as a held dispatch does, so a hook that's
/// slow after this is slow on its own account. Whether the Mac settled: false when it gave up or couldn't tell.
pub async fn wait_for_calm_host() -> bool {
    let started = now_ms();
    let mut told = false;
    loop {
        let busy = match soundcheck_driver::host::busy().await {
            Ok(busy) => busy,
            Err(e) => {
                eprintln!(
                    "soundcheck: the Mac's memory and CPU couldn't be read ({e}); going ahead"
                );
                return false;
            }
        };
        let waited = now_ms().saturating_sub(started) / 1000;
        if busy.is_empty() {
            if told {
                println!("the Mac settled after {waited} s");
            }

            return true;
        }

        if now_ms().saturating_sub(started) >= HOST_WAIT_MAX_MS {
            println!(
                "the Mac is still busy after {waited} s ({}); going ahead",
                busy.join(", ")
            );
            return false;
        }

        if !told {
            println!(
                "waiting for the Mac to settle before building or reading builds: {}",
                busy.join(", ")
            );
            told = true;
        }
        tokio::time::sleep(Duration::from_millis(READY_POLL_MS)).await;
    }
}

pub const ENDED_WITHOUT_POSTING: &str = "the tester ended without posting";

/// A started attempt: what its tester agent is started with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dispatched {
    pub token: String,
    pub model: Option<String>,
    pub agent: String,
    pub prompt: String,
}

/// A dispatch's claim on a platform while it resets it; dropping it lets the next dispatch take the platform.
pub struct Starting {
    hub: Arc<Hub>,
    platform: Platform,
}

impl Drop for Starting {
    fn drop(&mut self) {
        self.hub.starting().remove(&self.platform);
    }
}

/// One platform's driver behind its own lock: empty until its first device call connects it.
pub type DeviceSlot = Arc<tokio::sync::Mutex<Option<Driver<AnyDevice>>>>;

/// What a session needs to know about the run: the SessionStart and Stop hooks read it.
#[derive(Debug, Serialize, Deserialize)]
pub struct Summary {
    pub folder: PathBuf,
    pub label: String,
    pub verdict: String,
    pub finished: bool,
    pub paused: bool,
    pub rows: usize,
    /// Rows without a filed result.
    pub left: usize,
    /// Running attempts' tokens.
    pub running: Vec<String>,
    /// Claims waiting for review, as "spec on platform".
    pub review: Vec<String>,
    pub side_finds: usize,
    /// Whether the session asked about has started testers in this run.
    pub coordinator: bool,
    /// A trial on uncommitted code, which files no tickets.
    pub uncommitted: bool,
}

/// What `queue` did: rows that joined the run, and rows queued again with what each was.
pub struct Queued {
    pub added: Vec<(String, Platform)>,
    pub requeued: Vec<(RowRef, String)>,
}

pub struct Hub {
    /// Swapped whole when specs join mid-run, so a reader keeps a consistent copy.
    suite: RwLock<Arc<Suite>>,
    /// The release under test, shown as the page's title.
    pub label: String,
    /// The run's folder: journal, evidence, summary and exports. Its siblings are earlier runs.
    pub run_dir: PathBuf,
    run: Mutex<Run>,
    events: broadcast::Sender<Entry>,
    /// Platforms whose devices are left alone: specs on them seed but never touch a device (the server's own tests).
    pub without_devices: Vec<Platform>,
    /// A driver per platform, connected on its first device call. Each has its own lock, so with platforms in parallel
    /// one platform's long call (a `wait`, a restart) never holds up the other's.
    pub devices: Mutex<std::collections::HashMap<Platform, DeviceSlot>>,
    pub stall_ms: u64,
    /// How long a dispatched tester has to make its first tool call before its spec counts as stalled.
    pub start_ms: u64,
    /// Web's Chrome in a window to watch, rather than headless.
    pub headed: bool,
    /// A trial on uncommitted code (`run --uncommitted`, kept in run.json): every hook is told so, and it files no
    /// tickets.
    pub uncommitted: bool,
    /// How long one prepare step may take before the reset fails (`run --prepare-timeout`).
    pub prepare_timeout: std::time::Duration,
    /// The server's own address, set once it listens: prepare steps get it, so `soundcheck drive` acts through it.
    pub url: std::sync::OnceLock<String>,
    /// Devices found degraded after a spec, to restart before the next: platform → (spec, reason).
    restart_due: Mutex<std::collections::HashMap<Platform, (String, String)>>,
    /// When each platform's device was last restarted, so a device that stays degraded isn't rebooted in a loop.
    last_restart: Mutex<std::collections::HashMap<Platform, u64>>,
    /// Platforms whose device is restarting, so their dispatches wait: platform → why.
    restarting: Mutex<std::collections::HashMap<Platform, String>>,
    /// Restarts that failed, tried again once [`RESTART_RETRY_MS`] has passed: platform → (when, why, and the
    /// restart's own spec and reason). The platform is held meanwhile.
    restart_failed: Mutex<std::collections::HashMap<Platform, (u64, String, String, String)>>,
    /// Platforms restarted because a reset failed: the next failed reset there files its row BLOCKED rather than
    /// restarting again. A reset that works clears it (SA-925).
    reset_restarted: Mutex<std::collections::HashSet<Platform>>,
    /// Platforms a dispatch is resetting for its attempt, before the run marks anything running there: another
    /// dispatch meanwhile takes another platform, or, with platforms taking turns, none.
    starting: Mutex<std::collections::HashSet<Platform>>,
    /// Attempts the `dispatch` tool started whose tester the coordinator hasn't started yet: token → what it starts
    /// with. The plugin's hook takes one when the coordinator's Agent call is for it (SA-926).
    handed_out: Mutex<std::collections::HashMap<String, Dispatched>>,
    /// Platforms whose install hook is running on the server's own task, started by `install_build`.
    installing: Mutex<std::collections::HashSet<Platform>>,
    /// Each platform's current or just-ended wait for a busy host, until a dispatch journals it.
    waits: Mutex<std::collections::HashMap<Platform, Wait>>,
    /// When a wait for the host last ran out, so a host that stays busy doesn't hold every spec.
    gave_up: Mutex<std::collections::HashMap<Platform, u64>>,
    /// Each running tester's last full read of the screen and when, saved with a FAIL: by the time it posts, the
    /// screen has often moved on from the moment it judged.
    seen: Mutex<std::collections::HashMap<String, (u64, Vec<String>)>>,
    /// When an agent last called a tool, so a finished run still being triaged isn't taken over.
    last_call: std::sync::atomic::AtomicU64,
    /// How often a live page's feed pings while nothing changes; [`UPDATE_HEARTBEAT`] but in tests.
    heartbeat_ms: std::sync::atomic::AtomicU64,
    /// Stops the server, when a new run takes the port.
    stop: CancellationToken,
    /// The Claude Code sessions that started testers through the plugin's hook: only they are told to dispatch.
    coordinators: Mutex<std::collections::HashSet<String>>,
    /// Set from the server's start until the run's checks pass: nothing is dispatched meanwhile.
    start_up: Mutex<Option<report::StartUp>>,
    /// Tells live pages to redraw for what isn't in the journal: the start's progress.
    nudges: broadcast::Sender<()>,
    /// The web app the suite's start hook serves for the run, started again whenever web is installed.
    web_app: tokio::sync::Mutex<Option<hooks::Service>>,
}

/// How long a finished run must go without a tool call before a new run may take its port.
const STEP_DOWN_IDLE_MS: u64 = 5 * 60_000;

/// Why a platform whose build `install_build` is installing takes no spec (SA-923).
const INSTALLING: &str = "installing a build";

/// At most one restart per platform in this window; a device still degraded after that is only noted.
const RESTART_COOLDOWN_MS: u64 = 15 * 60_000;
/// How long a platform whose restart failed waits before the restart is tried again.
const RESTART_RETRY_MS: u64 = 60_000;
/// How long dispatches wait for a busy host before going ahead; a simulator's boot settled in about four minutes.
const HOST_WAIT_MAX_MS: u64 = 10 * 60_000;
/// How often `soundcheck ready --wait` asks whether a held platform is free.
pub const READY_POLL_MS: u64 = 15_000;
pub const START_STALL_MS: u64 = 10 * 60_000;
/// `run --prepare-timeout`'s default: one step seeding an account or signing in.
pub const PREPARE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The PATH with this binary's folder first, so a prepare step's `soundcheck drive` is the version running the run.
/// None when this binary isn't a `soundcheck` (a test).
fn path_with_this_soundcheck() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    if exe.file_name()? != "soundcheck" {
        return None;
    }

    let mut dirs = vec![exe.parent()?.to_path_buf()];
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }

    std::env::join_paths(dirs).ok()?.into_string().ok()
}

/// What a reset's prepare steps left: the context they printed for the tester, and the steps for the journal.
#[derive(Debug, Default)]
pub struct Prepared {
    pub context: Option<String>,
    pub steps: Vec<PreparedStep>,
}

/// A wait for a busy host: when it started, the latest reading, and when it ended and whether the host settled.
struct Wait {
    started: u64,
    reason: String,
    ended: Option<(u64, bool)>,
}

impl Wait {
    fn new(started: u64) -> Wait {
        Wait {
            started,
            reason: String::new(),
            ended: None,
        }
    }
}

impl Hub {
    pub fn new(suite: Suite, run: Run, label: String, run_dir: PathBuf, stall_ms: u64) -> Arc<Hub> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Hub {
            suite: RwLock::new(Arc::new(suite)),
            label,
            run_dir,
            run: Mutex::new(run),
            events,
            devices: Mutex::new(std::collections::HashMap::new()),
            without_devices: Vec::new(),
            stall_ms,
            start_ms: START_STALL_MS,
            headed: false,
            uncommitted: false,
            prepare_timeout: PREPARE_TIMEOUT,
            url: std::sync::OnceLock::new(),
            restart_due: Mutex::new(std::collections::HashMap::new()),
            last_restart: Mutex::new(std::collections::HashMap::new()),
            restarting: Mutex::new(std::collections::HashMap::new()),
            restart_failed: Mutex::new(std::collections::HashMap::new()),
            reset_restarted: Mutex::new(std::collections::HashSet::new()),
            starting: Mutex::new(std::collections::HashSet::new()),
            handed_out: Mutex::new(std::collections::HashMap::new()),
            installing: Mutex::new(std::collections::HashSet::new()),
            waits: Mutex::new(std::collections::HashMap::new()),
            gave_up: Mutex::new(std::collections::HashMap::new()),
            seen: Mutex::new(std::collections::HashMap::new()),
            coordinators: Mutex::new(std::collections::HashSet::new()),
            start_up: Mutex::new(None),
            nudges: broadcast::channel(16).0,
            web_app: tokio::sync::Mutex::new(None),
            last_call: std::sync::atomic::AtomicU64::new(now_ms()),
            heartbeat_ms: std::sync::atomic::AtomicU64::new(UPDATE_HEARTBEAT.as_millis() as u64),
            stop: CancellationToken::new(),
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
        let now = now_ms();
        let result = f(&mut run, now);
        let fresh = run.entries_after(before);
        for entry in fresh {
            let _ = self.events.send(entry.clone());
        }
        if !fresh.is_empty() {
            let _ = std::fs::write(
                self.run_dir.join(SUMMARY_FILE),
                report::run_summary(&run, self.label_of(&run), self.uncommitted).to_string(),
            );
        }
        result
    }

    /// The suite specs are dispatched from: as loaded at start, plus any specs added since.
    pub fn suite(&self) -> Arc<Suite> {
        self.suite.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The platform the run's suite calls `name`, for a name a tool call or a dispatch gave.
    pub fn platform(&self, name: &str) -> Result<Platform> {
        let suite = self.suite();
        let config = suite
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("{} doesn't load", crate::suite::CONFIG_FILE))?;
        config.platform(name).map_err(anyhow::Error::msg)
    }

    /// [`Hub::platform`] for each of a list of names.
    pub fn platforms(&self, names: &[String]) -> Result<Vec<Platform>> {
        names.iter().map(|n| self.platform(n)).collect()
    }

    /// Queues spec IDs or features: specs not in the run join it, read from the suite as it is on disk now; specs in
    /// it run again on `platforms` (all of theirs by default) whatever state their rows are in, which needs a reason.
    /// Returns the rows added, and the rows queued again with what each was.
    pub fn queue(
        &self,
        requested: &[String],
        platforms: Option<&[Platform]>,
        reason: Option<&str>,
        note: Option<String>,
    ) -> Result<Queued> {
        if requested.is_empty() {
            anyhow::bail!("name at least one spec ID or feature");
        }
        let again: Vec<RowRef> = self.with_run(|run, _| {
            Ok(run
                .plan()
                .specs
                .iter()
                .filter(|s| {
                    requested.iter().any(|i| {
                        *i == s.id
                            || s.id
                                .strip_prefix(i.as_str())
                                .is_some_and(|rest| rest.starts_with('/'))
                    })
                })
                .flat_map(|s| s.platforms.iter().map(move |p| (s, *p)))
                .filter(|(_, p)| platforms.is_none_or(|ps| ps.contains(p)))
                .map(|(s, platform)| RowRef {
                    spec: s.id.clone(),
                    platform,
                })
                .collect())
        })?;
        let reason = reason.map(str::trim).filter(|r| !r.is_empty());
        if !again.is_empty() && reason.is_none() {
            anyhow::bail!(
                "{} already ran: say why it runs again, and the report shows it beside the result",
                again
                    .iter()
                    .map(|r| r.spec.as_str())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let rerun: std::collections::BTreeSet<String> =
            again.iter().map(|r| r.spec.clone()).collect();
        self.refresh_specs(&rerun.into_iter().collect::<Vec<_>>())?;
        let added = self.add_specs(requested)?;
        let requeued = match reason {
            Some(reason) if !again.is_empty() => self.with_run(|run, now| {
                Ok(run
                    .requeue(now, again, reason, note)?
                    .into_iter()
                    .map(|(r, was)| (r, was.to_string()))
                    .collect())
            })?,
            _ => Vec::new(),
        };
        if added.is_empty() && requeued.is_empty() {
            anyhow::bail!("nothing to queue: none of them runs on the platforms named");
        }
        Ok(Queued { added, requeued })
    }

    /// Rereads each spec whose file changed since the run's snapshot, so a rerun uses what's on disk: it must lint
    /// clean, joins the snapshot, and the run takes up its outcomes. Returns the specs that changed.
    fn refresh_specs(&self, ids: &[String]) -> Result<Vec<String>> {
        let info = saved::read_info(&self.run_dir)?;
        let live = crate::suite::load(&info.suite);
        let text = |root: &std::path::Path, id: &str| {
            std::fs::read_to_string(root.join(&live.specs_dir).join(format!("{id}.spec.md"))).ok()
        };
        let snapshot = self.run_dir.join(saved::SNAPSHOT_DIR);
        let changed: Vec<String> = ids
            .iter()
            .filter(|id| {
                let now = text(&live.root, id);
                now.is_some() && now != text(&snapshot, id)
            })
            .cloned()
            .collect();
        if changed.is_empty() {
            return Ok(changed);
        }
        let files: Vec<PathBuf> = changed
            .iter()
            .map(|id| live.specs_dir.join(format!("{id}.spec.md")))
            .collect();
        let problems: Vec<String> = crate::lint::lint(&live, crate::VERSION)
            .into_iter()
            .filter(|p| files.contains(&p.path))
            .map(|p| format!("{}: {}", p.path.display(), p.message))
            .collect();
        if !problems.is_empty() {
            anyhow::bail!(
                "the edited spec doesn't lint clean: {}",
                problems.join("; ")
            );
        }
        let planned = saved::plan_for(&live, &info.platforms, &changed)?.specs;
        saved::snapshot_specs(&self.run_dir, &live, &changed)?;
        {
            let mut suite = self.suite.write().unwrap_or_else(|e| e.into_inner());
            let mut next = (**suite).clone();
            for id in &changed {
                if let Some(spec) = live.specs.iter().find(|s| s.id == *id) {
                    next.specs.retain(|s| s.id != *id);
                    next.specs.push(spec.clone());
                }
            }
            next.specs
                .sort_by(|a, b| (&a.feature, &a.id).cmp(&(&b.feature, &b.id)));
            *suite = Arc::new(next);
        }
        self.with_run(|run, now| {
            for spec in planned {
                run.spec_changed(now, &spec.id, spec.outcomes)?;
            }
            Ok(())
        })?;
        Ok(changed)
    }

    /// Adds the specs named that aren't in the run yet, with their snapshot and `run.json`'s scope, so the run folder
    /// stays self-contained; returns the rows added.
    fn add_specs(&self, requested: &[String]) -> Result<Vec<(String, Platform)>> {
        let info = saved::read_info(&self.run_dir)?;
        let live = crate::suite::load(&info.suite);
        for item in requested {
            let covered = live
                .specs
                .iter()
                .filter(|s| s.id == *item || s.feature == *item)
                .map(|s| s.front.platforms.iter().any(|p| info.platforms.contains(p)))
                .collect::<Vec<_>>();
            if covered.is_empty() {
                let file = live.specs_dir.join(format!("{item}.spec.md"));
                if let Some(p) = live.problems.iter().find(|p| p.path == file) {
                    anyhow::bail!("{item} doesn't lint clean: {}", p.message);
                }
                anyhow::bail!("{item}: no feature or spec by that name");
            }
            if !covered.contains(&true) {
                anyhow::bail!("{item} runs on none of the run's platforms");
            }
        }
        let in_run: Vec<String> =
            self.with_run(|run, _| Ok(run.plan().specs.iter().map(|s| s.id.clone()).collect()))?;
        let new: Vec<&Spec> = live
            .specs
            .iter()
            .filter(|s| requested.iter().any(|i| *i == s.id || *i == s.feature))
            .filter(|s| s.front.platforms.iter().any(|p| info.platforms.contains(p)))
            .filter(|s| !in_run.contains(&s.id))
            .collect();
        if new.is_empty() {
            return Ok(Vec::new());
        }
        let files: Vec<PathBuf> = new
            .iter()
            .flat_map(|s| {
                [
                    live.specs_dir.join(format!("{}.spec.md", s.id)),
                    live.specs_dir
                        .join(&s.feature)
                        .join(crate::suite::FEATURE_FILE),
                ]
            })
            .collect();
        let problems: Vec<String> = crate::lint::lint(&live, crate::VERSION)
            .into_iter()
            .filter(|p| files.contains(&p.path))
            .map(|p| format!("{}: {}", p.path.display(), p.message))
            .collect();
        if !problems.is_empty() {
            anyhow::bail!("they don't lint clean: {}", problems.join("; "));
        }
        let ids: Vec<String> = new.iter().map(|s| s.id.clone()).collect();
        let planned = saved::plan_for(&live, &info.platforms, &ids)?.specs;
        saved::snapshot_specs(&self.run_dir, &live, &ids)?;
        saved::write_info(
            &self.run_dir,
            &saved::RunInfo {
                scope: saved::merge_scope(&info.scope, &ids),
                ..info.clone()
            },
        )?;
        {
            let mut suite = self.suite.write().unwrap_or_else(|e| e.into_inner());
            let mut next = (**suite).clone();
            for spec in &new {
                next.specs.retain(|s| s.id != spec.id);
                next.specs.push((*spec).clone());
                if !next.features.iter().any(|f| f.name == spec.feature)
                    && let Some(feature) = live.features.iter().find(|f| f.name == spec.feature)
                {
                    next.features.push(feature.clone());
                }
            }
            next.specs
                .sort_by(|a, b| (&a.feature, &a.id).cmp(&(&b.feature, &b.id)));
            next.features.sort_by(|a, b| a.name.cmp(&b.name));
            *suite = Arc::new(next);
        }
        self.with_run(|run, now| run.add_specs(now, planned))
    }

    /// The run's label: the latest one given mid-run, else the one it started with.
    pub fn label_of<'a>(&'a self, run: &'a Run) -> &'a str {
        run.state().label.as_deref().unwrap_or(&self.label)
    }

    /// What every hook this run starts is told about it.
    pub fn run_env(&self) -> hooks::RunEnv {
        hooks::RunEnv {
            uncommitted: self.uncommitted,
        }
    }

    fn page<'a>(
        &'a self,
        suite: &'a Suite,
        run: &'a Run,
        export: Option<report::Export>,
        starting: Option<&'a report::StartUp>,
    ) -> report::Page<'a> {
        report::Page {
            suite,
            run,
            label: self.label_of(run),
            uncommitted: self.uncommitted,
            live: export.is_none(),
            run_dir: Some(&self.run_dir),
            export,
            starting,
        }
    }

    /// How often a live page's feed pings while nothing changes.
    pub fn heartbeat(&self) -> Duration {
        Duration::from_millis(self.heartbeat_ms.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Sets the feed's ping interval, so a test needn't wait 15 s for one.
    pub fn set_heartbeat(&self, every: Duration) {
        self.heartbeat_ms.store(
            every.as_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// What the live page shows now, as data.
    pub fn view(&self) -> serde_json::Value {
        let starting = self.start_state();
        self.with_run(|run, _| {
            Ok(report::view::json(&self.page(
                &self.suite(),
                run,
                None,
                starting.as_ref(),
            )))
        })
        .unwrap_or_default()
    }

    fn start_up(&self) -> std::sync::MutexGuard<'_, Option<report::StartUp>> {
        self.start_up.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn change_start(&self, change: impl FnOnce(&mut report::StartUp)) {
        if let Some(starting) = self.start_up().as_mut() {
            change(starting);
        }
        let _ = self.nudges.send(());
    }

    /// The run is starting, with these checks already passed: dispatches wait until `started`.
    pub fn begin_start(&self, passed: Vec<String>) {
        *self.start_up() = Some(report::StartUp {
            passed,
            ..Default::default()
        });
        let _ = self.nudges.send(());
    }

    pub fn start_step(&self, step: impl Into<String>) {
        let step = step.into();
        self.change_start(|s| s.doing = Some((step, now_ms())));
    }

    pub fn start_passed(&self, check: impl Into<String>) {
        let check = check.into();
        self.change_start(|s| {
            s.doing = None;
            s.passed.push(check);
        });
    }

    pub fn start_failed(&self, why: impl Into<String>) {
        let why = why.into();
        self.change_start(|s| s.failed = Some(why));
    }

    /// The checks passed during the start.
    pub fn start_checks(&self) -> Vec<String> {
        self.start_up()
            .as_ref()
            .map(|s| s.passed.clone())
            .unwrap_or_default()
    }

    pub fn started(&self) {
        *self.start_up() = None;
        let _ = self.nudges.send(());
    }

    pub fn start_state(&self) -> Option<report::StartUp> {
        self.start_up().clone()
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
        let slot = self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(platform)
            .or_default()
            .clone();
        let mut device = slot.lock().await;
        if device.is_none() {
            *device = Some(driver(platform, &self.suite(), self.headed).await?);
        }
        let Some(driver) = device.as_ref() else {
            unreachable!("connected above");
        };
        Ok(op(driver).await?)
    }

    /// What every attempt starts from: online with no faults, whatever the last one
    /// left (first, since prepare steps may go through it), and the guard passed.
    async fn online_and_guarded(&self, platform: Platform) -> Result<()> {
        let suite = self.suite();
        let config = suite
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no suite.yaml"))?;
        let root = &suite.root;
        if let Some(routes) = crate::network::routes(config, platform)
            .filter(|_| !self.without_devices.contains(&platform))
        {
            crate::network::ensure(root, platform, &routes)
                .await?
                .reset()
                .await?;
        }
        hooks::run(
            root,
            &config.of(&platform).hooks.guard,
            hooks::GUARD_TIMEOUT,
            self.run_env(),
        )
        .await
        .context("the guard refused")?;
        Ok(())
    }

    /// Resets a platform for a spec (DESIGN.md 5.6): guard, a fresh relaunch, then the spec's prepare steps in order.
    /// Tried three times; the error after the last says why, and the caller files the spec BLOCKED as a reset failure.
    /// Whether the start screen is right is the tester's to judge: a wrong one is "couldn't check".
    pub async fn reset(&self, spec: &Spec, platform: Platform) -> Result<Prepared> {
        let suite = self.suite();
        let config = suite
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no suite.yaml"))?;
        let mut last = anyhow::anyhow!("not tried");
        for _ in 0..3 {
            let attempt = async {
                self.online_and_guarded(platform).await?;
                let platform_config = config.of(&platform);
                if !self.without_devices.contains(&platform) {
                    let hardware = spec.front.keyboard == crate::suite::spec::Keyboard::Hardware;
                    // Every spec sets it, so one spec's size never carries into the next.
                    let size = (platform_config.driver == DriverKind::Chrome)
                        .then(|| {
                            spec.front
                                .viewport
                                .as_deref()
                                .and_then(parse_viewport)
                                .or_else(|| platform_config.viewport())
                        })
                        .flatten();
                    self.on_device(platform, async |d| {
                        d.device.hardware_keyboard(hardware).await?;
                        if let Some((width, height)) = size {
                            d.device.viewport(width, height).await?;
                        }
                        Device::launch(&d.device).await
                    })
                    .await?;
                }

                self.prepare(&suite.root, spec, platform, platform_config)
                    .await
            };
            match attempt.await {
                Ok(prepared) => return Ok(prepared),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Runs a spec's prepare steps for `platform`, each with the platform's variables and the values earlier steps
    /// printed. Its commands go to the journal as written; the values never do.
    async fn prepare(
        &self,
        root: &std::path::Path,
        spec: &Spec,
        platform: Platform,
        config: &PlatformConfig,
    ) -> Result<Prepared> {
        let mut env = BTreeMap::from([
            ("SOUNDCHECK_PLATFORM".to_string(), platform.to_string()),
            ("SOUNDCHECK_DRIVER".to_string(), config.driver.to_string()),
            ("SOUNDCHECK_DEVICE".to_string(), config.device.clone()),
        ]);
        if let Some(url) = self.url.get() {
            env.insert("SOUNDCHECK_SERVER".to_string(), url.clone());
        }

        // A step's `soundcheck` is this run's own, never an older one earlier on the PATH.
        if let Some(path) = path_with_this_soundcheck() {
            env.insert("PATH".to_string(), path);
        }

        let mut prepared = Prepared::default();
        let mut contexts = Vec::new();
        for (n, step) in spec.front.prepare.iter().enumerate() {
            let Some(command) = step.on(platform) else {
                continue;
            };

            let at = format!("prepare step {}", n + 1);
            let started = std::time::Instant::now();
            let out = hooks::run_with(root, command, self.prepare_timeout, self.run_env(), &env)
                .await
                .map_err(|e| match e.downcast_ref::<hooks::TimedOut>() {
                    Some(_) => {
                        e.context(format!("{at} (`run --prepare-timeout` raises the limit)"))
                    }
                    None => e.context(at.clone()),
                })?;
            let printed = hooks::printed(&out).map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
            env.extend(printed.values);
            contexts.extend(printed.context);
            prepared.steps.push(PreparedStep {
                command: command.to_string(),
                ms: started.elapsed().as_millis() as u64,
            });
        }

        prepared.context = (!contexts.is_empty()).then(|| contexts.join("\n"));
        Ok(prepared)
    }

    /// Why `platform` is held by an install that failed, or was cut off when the server stopped: the app may be half
    /// replaced or gone (SA-924). The hold lifts once a later install succeeds, or the build-identity hook can read
    /// the build on it again, which is logged as its build.
    async fn install_unfinished(&self, platform: Platform) -> Option<String> {
        let install = self
            .with_run(|run, _| Ok(run.state().installs.get(&platform).cloned()))
            .ok()
            .flatten()?;
        let Ok(build) = self.read_build(platform).await else {
            let why = match install {
                Install::Failed(reason) => format!("install failed — {reason}"),
                Install::Started => "its install was cut off when the server stopped".to_string(),
            };
            return Some(format!("{why}; install_build installs it again"));
        };

        self.with_run(|run, now| run.install_build(now, platform, build, vec![]))
            .err()
            .map(|e| format!("{e:#}"))
    }

    /// Why `platform` can't take a spec yet, after starting what it waits for: a due restart, or a busy host. It's
    /// checked on demand, by a dispatch and by `soundcheck ready`, so nothing has to watch for the hold to end. A
    /// dispatch refuses rather than blocking, since a tool call times out long before a boot settles.
    pub async fn hold(self: &Arc<Self>, platform: Platform) -> Option<String> {
        if self.installing().contains(&platform) {
            return Some(INSTALLING.to_string());
        }

        if let Some(held) = self.install_unfinished(platform).await {
            return Some(held);
        }

        if let Some(reason) = self
            .restarting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&platform)
        {
            return Some(reason.clone());
        }

        let failed = self
            .restart_failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&platform);

        if let Some((at, why, after, reason)) = failed {
            if now_ms().saturating_sub(at) < RESTART_RETRY_MS {
                let held = format!(
                    "the {platform} device's restart failed ({why}); it's tried again within a minute"
                );
                self.restart_failed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(platform, (at, why, after, reason));
                return Some(held);
            }

            self.restart_due
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(platform, (after, reason));
        }

        let due = self
            .restart_due
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&platform);
        if let Some((after, reason)) = due {
            let held = format!("restarting the {platform} device after {after}: {reason}");
            self.restarting
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(platform, held.clone());
            let hub = self.clone();
            tokio::spawn(async move {
                let restarted = hub.restart_device(platform).await;
                hub.last_restart
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(platform, now_ms());
                match restarted {
                    Ok(()) => {
                        let _ = hub.with_run(|run, now| {
                            run.device_restarted(now, platform, &after, &reason)
                        });
                    }
                    // Held until the retry: a device that couldn't restart can't take a spec either.
                    Err(e) => {
                        hub.restart_failed
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(platform, (now_ms(), format!("{e:#}"), after, reason));
                    }
                }

                hub.restarting
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&platform);
            });
            return Some(held);
        }

        if self.without_devices.contains(&platform) {
            return None;
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
        let now = now_ms();
        let mut waits = self.waits.lock().unwrap_or_else(|e| e.into_inner());
        if busy.is_empty() {
            if let Some(wait) = waits.get_mut(&platform) {
                wait.ended.get_or_insert((now, true));
            }

            return None;
        }

        let reason = busy.join(", ");
        let wait = waits
            .entry(platform)
            .and_modify(|w| {
                if w.ended.is_some() {
                    *w = Wait::new(now);
                }
            })
            .or_insert_with(|| Wait::new(now));
        wait.reason = reason.clone();
        if now.saturating_sub(wait.started) >= HOST_WAIT_MAX_MS {
            wait.ended = Some((now, false));
            self.gave_up
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(platform, now);
            return None;
        }

        Some(format!("the Mac is busy ({reason})"))
    }

    /// Journals the wait a dispatch on `platform` has just ended, if there was one, against the spec it let through.
    pub fn end_wait(&self, spec: &str, platform: Platform) {
        let wait = self
            .waits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&platform);
        if let Some(Wait {
            started,
            reason,
            ended: Some((at, settled)),
        }) = wait
        {
            let waited = at.saturating_sub(started);
            let _ = self
                .with_run(|run, now| run.host_busy(now, platform, spec, &reason, waited, settled));
        }
    }

    /// Starts a row's next attempt: the row named, or with either half left out, the first [`Run::next_rows`] offers
    /// on a platform that can take a spec now.
    pub async fn dispatch(
        self: &Arc<Self>,
        spec: Option<&str>,
        platform: Option<Platform>,
    ) -> Result<Dispatched> {
        if let Some(starting) = self.start_state() {
            let step = starting
                .doing
                .map_or_else(|| "its checks".to_string(), |(step, _)| step);
            anyhow::bail!(
                "the run is still starting ({step}); dispatch once soundcheck prints `MCP at`"
            );
        }

        if let Some(spec) = spec
            && !self.suite().specs.iter().any(|s| s.id == spec)
        {
            anyhow::bail!("no spec {spec} in the suite");
        }

        // Two dispatches at once (platforms in parallel) mustn't reset the same platform: each claims one first.
        let parallel = self.with_run(|run, _| Ok(run.plan().parallel))?;
        let (spec, platform, _claim) = match (spec, platform) {
            (Some(spec), Some(platform)) => {
                // Check first, so a refused dispatch never resets a device.
                self.with_run(|run, _| run.check_dispatch(spec, platform))?;

                if let Some(reason) = self.hold(platform).await {
                    anyhow::bail!("{}", self.held(&[(platform, reason)]));
                }

                (spec.to_string(), platform, self.claim(platform, parallel)?)
            }
            _ => {
                let (spec, platform, claim) = self.pick_row(spec, platform, Some(parallel)).await?;
                let Some(claim) = claim else {
                    unreachable!("a pick that claims returns its claim");
                };
                (spec, platform, claim)
            }
        };

        self.probe_build(platform).await?;

        self.start_attempt(&spec, platform).await
    }

    fn starting(&self) -> std::sync::MutexGuard<'_, std::collections::HashSet<Platform>> {
        self.starting.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset_restarted(&self) -> std::sync::MutexGuard<'_, std::collections::HashSet<Platform>> {
        self.reset_restarted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Restarts `platform`'s device: relaunches Chrome, or restarts the simulator or emulator. A platform whose
    /// device this server leaves alone (its own tests) has nothing to restart.
    async fn restart_device(&self, platform: Platform) -> Result<()> {
        if self.without_devices.contains(&platform) {
            return Ok(());
        }

        self.on_device(platform, async |d| d.device.restart().await)
            .await
    }

    fn installing(&self) -> std::sync::MutexGuard<'_, std::collections::HashSet<Platform>> {
        self.installing.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Claims `platform` for a dispatch's reset, released when dropped; refused while another dispatch has it or it's
    /// installing. An install takes the same lock to start ([`Self::install_build`]), so a dispatch that passed its
    /// hold before an install began can't start on the half-replaced app.
    fn claim(self: &Arc<Self>, platform: Platform, parallel: bool) -> Result<Starting> {
        let mut starting = self.starting();
        if !parallel && !starting.is_empty() {
            anyhow::bail!("another dispatch is starting its attempt; one spec at a time");
        }

        if self.installing().contains(&platform) {
            anyhow::bail!("{}", self.held(&[(platform, INSTALLING.to_string())]));
        }

        if !starting.insert(platform) {
            anyhow::bail!(
                "{platform} is starting another attempt; dispatch again for the next one"
            );
        }

        Ok(Starting {
            hub: self.clone(),
            platform,
        })
    }

    /// The row `dispatch` would start next, asking each platform whether it's held at most once.
    pub async fn pick(
        self: &Arc<Self>,
        spec: Option<&str>,
        platform: Option<Platform>,
    ) -> Result<(String, Platform)> {
        let (spec, platform, _) = self.pick_row(spec, platform, None).await?;
        Ok((spec, platform))
    }

    /// [`Self::pick`], and with `claim` (whether platforms run in parallel) the row's platform claimed for its reset.
    /// Concurrent `next` dispatches each take a different free platform (SA-927): a platform claimed by another
    /// dispatch since the rows were read, or a row it started, is passed over for the next, so a call is refused only
    /// when no platform is free.
    async fn pick_row(
        self: &Arc<Self>,
        spec: Option<&str>,
        platform: Option<Platform>,
        claim: Option<bool>,
    ) -> Result<(String, Platform, Option<Starting>)> {
        let rows = self.with_run(|run, _| run.next_rows(spec, platform))?;
        let mut held: Vec<(Platform, String)> = Vec::new();
        let mut lost = None;
        for (spec, platform) in rows {
            if held.iter().any(|(p, _)| *p == platform) || self.starting().contains(&platform) {
                continue;
            }

            if let Some(reason) = self.hold(platform).await {
                held.push((platform, reason));
                continue;
            }

            let Some(parallel) = claim else {
                return Ok((spec, platform, None));
            };
            let claimed = match self.claim(platform, parallel) {
                Ok(claimed) => claimed,
                Err(e) => {
                    lost = Some(e);
                    continue;
                }
            };

            // Another dispatch may have started this row, or started one on this platform, since the rows were read.
            if let Err(e) = self.with_run(|run, _| run.check_dispatch(&spec, platform)) {
                lost = Some(e);
                continue;
            }

            return Ok((spec, platform, Some(claimed)));
        }

        match lost {
            Some(e) if held.is_empty() => Err(e),
            _ => anyhow::bail!("{}", self.held(&held)),
        }
    }

    fn held(&self, held: &[(Platform, String)]) -> String {
        let reasons: Vec<String> = match held {
            [(_, reason)] => vec![reason.clone()],
            _ => held.iter().map(|(p, r)| format!("{p}: {r}")).collect(),
        };
        let first = held.first().map(|(p, _)| p.to_string()).unwrap_or_default();
        format!(
            "held: {}. Run `soundcheck ready {} {first} --wait` in the background, and dispatch again when it exits",
            reasons.join("; "),
            self.run_dir.display(),
        )
    }

    async fn start_attempt(
        self: &Arc<Self>,
        spec_id: &str,
        platform: Platform,
    ) -> Result<Dispatched> {
        let suite = self.suite();
        let Some(spec) = suite.specs.iter().find(|s| s.id == spec_id) else {
            anyhow::bail!("no spec {spec_id} in the suite");
        };
        self.end_wait(spec_id, platform);
        let reset_started = std::time::Instant::now();
        let prepared = match self.reset(spec, platform).await {
            Ok(prepared) => {
                self.reset_restarted().remove(&platform);
                prepared
            }
            // A wedged device fails every reset: restart it first, and file the row only if a restarted device still
            // can't reset. The restart runs on the server's own task, as a degraded device's does, since it can
            // outlast the tool call (SA-925).
            Err(e) => {
                let reason = format!("{e:#}");

                if self.reset_restarted().insert(platform) {
                    self.restart_due
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(
                            platform,
                            (spec_id.to_string(), format!("its reset failed ({reason})")),
                        );
                    let held = self.hold(platform).await.unwrap_or_default();
                    anyhow::bail!(
                        "{}; {spec_id} is still queued",
                        self.held(&[(platform, held)])
                    );
                }

                self.reset_restarted().remove(&platform);
                let _ = self.with_run(|run, now| run.reset_failed(now, spec_id, platform, &reason));
                anyhow::bail!(
                    "the reset failed on a restarted device too, so {spec_id} is filed BLOCKED on {platform}: {reason}"
                );
            }
        };

        let context = suite
            .features
            .iter()
            .find(|f| f.name == spec.feature)
            .and_then(|f| f.context.as_deref());
        // A spec's viewport sizes a chrome platform's window, so a tester on any other platform isn't told of it.
        let mut briefed = spec.clone();
        let config = suite
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("{} doesn't load", crate::suite::CONFIG_FILE))?;

        if config.driver(&platform) != DriverKind::Chrome {
            briefed.front.viewport = None;
        }

        self.with_run(|run, now| {
            let rerun = run
                .state()
                .rows
                .get(&(spec_id.to_string(), platform))
                .and_then(|r| r.rerun_note.clone());
            let reset_ms = reset_started.elapsed().as_millis() as u64;
            let token = run.dispatch_after_reset(
                now,
                spec_id,
                platform,
                Some(reset_ms),
                prepared.steps,
                |token, verify| {
                    prompt::render(
                        &briefed,
                        context,
                        platform,
                        token,
                        verify,
                        rerun.as_deref(),
                        prepared.context.as_deref(),
                    )
                },
            )?;
            Ok(Dispatched {
                model: spec
                    .front
                    .model
                    .clone()
                    .or_else(|| suite.config.as_ref().map(|c| c.models.default.clone())),
                agent: prompt::agent(spec).to_string(),
                prompt: prompt::handoff(&token),
                token,
            })
        })
    }

    /// The row a tester agent's prompt names, if any: the longest spec ID in it, and a platform named as a word.
    /// The `dispatch` tool started an attempt: kept until the coordinator's Agent call for it comes through the hook.
    pub fn hand_out(&self, dispatched: &Dispatched) {
        self.handed_out
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(dispatched.token.clone(), dispatched.clone());
    }

    /// The attempt the `dispatch` tool started that an Agent call's prompt is for, by its token or else its spec and
    /// platform, while its tester hasn't started: the hook starts that tester rather than dispatching the row again,
    /// which a running row refuses (SA-926). Taken once, so one attempt never gets two testers.
    pub fn take_handed_out(&self, prompt: &str) -> Option<Dispatched> {
        let (spec, platform) = self.named_row(prompt);
        let mut handed_out = self.handed_out.lock().unwrap_or_else(|e| e.into_inner());
        let token = self
            .with_run(|run, _| {
                handed_out.retain(|token, _| run.waiting_for_tester(token));
                let by_token = handed_out
                    .keys()
                    .filter(|token| prompt.contains(token.as_str()))
                    .max_by_key(|token| token.len())
                    .cloned();
                let by_row = || {
                    handed_out
                        .keys()
                        .find(|token| {
                            run.row_of(token).is_some_and(|(s, p)| {
                                Some(s) == spec.as_deref() && Some(p) == platform
                            })
                        })
                        .cloned()
                };
                Ok(by_token.or_else(by_row))
            })
            .ok()
            .flatten()?;
        handed_out.remove(&token)
    }

    pub fn named_row(&self, prompt: &str) -> (Option<String>, Option<Platform>) {
        let suite = self.suite();
        let spec = suite
            .specs
            .iter()
            .map(|s| s.id.clone())
            .filter(|id| prompt.contains(id.as_str()))
            .max_by_key(String::len);
        let rest = match &spec {
            Some(id) => prompt.replace(id.as_str(), " "),
            None => prompt.to_string(),
        };
        let platform = suite.config.as_ref().and_then(|config| {
            rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .find_map(|word| config.platform(&word.to_ascii_lowercase()).ok())
        });
        (spec, platform)
    }

    pub fn summary(&self, session: Option<&str>) -> Summary {
        let run = self.run.lock().unwrap_or_else(|e| e.into_inner());
        let state = run.state();
        let mut running = Vec::new();
        let mut review = Vec::new();
        let mut left = 0;
        for ((spec, platform), r) in &state.rows {
            match &r.row {
                crate::run::Row::Filed { .. } => continue,
                crate::run::Row::Running { token, .. } => running.push(token.clone()),
                crate::run::Row::NeedsReview { .. } => review.push(format!("{spec} on {platform}")),
                _ => {}
            }

            left += 1;
        }

        let verdict = crate::run::verdict::verdict(state);
        Summary {
            folder: self.run_dir.clone(),
            label: self.label_of(&run).to_string(),
            verdict: serde_json::to_value(verdict.status)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default(),
            finished: state.finished,
            paused: state.paused,
            rows: state.rows.len(),
            left,
            running,
            review,
            side_finds: state.pending_finds.len(),
            coordinator: session.is_some_and(|s| self.is_coordinator(s)),
            uncommitted: self.uncommitted,
        }
    }

    pub fn add_coordinator(&self, session: &str) {
        self.coordinators
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.to_string());
    }

    pub fn is_coordinator(&self, session: &str) -> bool {
        self.coordinators
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(session)
    }

    /// A tester agent has stopped. If its attempt is still running it never posted, so the attempt counts as a
    /// stall and the row is queued again (or filed BLOCKED on its last attempt). Returns what happened, if anything.
    pub fn tester_ended(&self, token: &str) -> Result<Option<String>> {
        self.with_run(|run, now| {
            if run.state().finished || run.running(token).is_err() {
                return Ok(None);
            }

            run.stall(now, token, ENDED_WITHOUT_POSTING)?;
            Ok(Some(format!("{token}: {ENDED_WITHOUT_POSTING}")))
        })
    }

    async fn host_busy(&self, platform: Platform) -> Vec<String> {
        self.on_device(platform, async |d| d.device.health().await)
            .await
            .map(|h| h.busy)
            .unwrap_or_default()
    }

    /// Samples the device's health after an attempt posted and journals it with that attempt. A degraded device is
    /// restarted before the next spec on it; a busy host is waited out before the next spec instead.
    pub async fn check_health(&self, spec: &str, platform: Platform, attempt: u32) {
        if self.without_devices.contains(&platform) {
            return;
        }

        let driver = self.suite().config.as_ref().map(|c| c.driver(&platform));
        let Ok(health) = self
            .on_device(platform, async |d| {
                // An attempt that ended offline or with a fault mustn't leave the app cut off between specs.
                if let Some(network) = &d.network {
                    network.reset().await?;
                } else if driver == Some(DriverKind::Chrome) {
                    d.device.offline(false).await?;
                }
                d.device.health().await
            })
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
                .insert(
                    platform,
                    (
                        spec.to_string(),
                        format!("degraded ({})", health.degraded.join("; ")),
                    ),
                );
        }
        let _ = self.with_run(|run, now| run.device_health(now, spec, platform, attempt, health));
    }

    /// The folder holding this run and its siblings.
    pub fn reports(&self) -> reports::Reports {
        reports::Reports {
            dir: self.run_dir.parent().map(PathBuf::from).unwrap_or_default(),
            current: Some(self.page_key()),
        }
    }

    /// The run's page is at `/{key}/`: its id, or its folder's name for a run from before ids.
    pub fn page_key(&self) -> String {
        crate::run::saved::page_key(&self.run_dir)
    }

    pub fn fixtures_dir(&self) -> PathBuf {
        let suite = self.suite();
        let dir = suite.config.as_ref().and_then(|c| c.fixtures.clone());
        suite.root.join(dir.unwrap_or_else(|| "fixtures".into()))
    }

    pub fn references_dir(&self) -> PathBuf {
        self.suite().root.join("references")
    }

    /// Runs the project's check or act hook for the tester's platform: `<hook> <name> <args...>`, each quoted for the
    /// shell. A failing call is still evidence, so its exit and output come back rather than an error.
    pub async fn project_hook(
        &self,
        kind: &str,
        platform: Platform,
        name: &str,
        args: &[String],
    ) -> Result<String> {
        let suite = self.suite();
        let config = suite
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no suite.yaml"))?;
        let hooks = &config.of(&platform).hooks;
        let hook = if kind == "check" {
            Some(&hooks.check)
        } else {
            hooks.act.as_ref()
        };
        let Some(hook) = hook else {
            anyhow::bail!("suite.yaml gives {platform} no {kind} hook");
        };
        let quoted: Vec<String> = std::iter::once(name)
            .chain(args.iter().map(String::as_str))
            .map(|a| format!("'{}'", a.replace('\'', r"'\''")))
            .collect();
        let command = format!("{hook} {}", quoted.join(" "));
        match hooks::run(&suite.root, &command, hooks::CHECK_TIMEOUT, self.run_env()).await {
            Ok(out) => Ok(out),
            Err(e) => Ok(format!("{kind} failed: {e:#}")),
        }
    }

    /// Asks the project's build-identity hook which build is installed on a platform.
    pub async fn read_build(&self, platform: Platform) -> Result<Build> {
        let suite = self.suite();
        let config = suite
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no suite.yaml"))?;
        let command = &config.of(&platform).hooks.build_identity;
        let out = hooks::run(
            &suite.root,
            command,
            hooks::BUILD_IDENTITY_TIMEOUT,
            self.run_env(),
        )
        .await?;
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
        let suite = self.suite();
        let config = suite
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no suite.yaml"))?;
        let platform_config = config.of(&platform);
        hooks::run(
            &suite.root,
            &platform_config.hooks.install,
            hooks::INSTALL_TIMEOUT,
            self.run_env(),
        )
        .await
        .with_context(|| format!("the install hook failed on {platform}"))?;

        if platform_config.driver == DriverKind::Chrome && self.web_app.lock().await.is_some() {
            self.start_web_app().await?;
        }

        Ok(())
    }

    /// `install_build`: the project's install hook builds and installs the current tree on `platform`, on a task of
    /// the server's own, then the build is logged and the listed rows queued. A native build takes minutes, and a hook
    /// run inside the tool call died with the call when the client's session dropped (SA-922). The call returns as
    /// soon as the install starts, and the journal says how it ended: `build-installed` or `install-failed`.
    pub async fn install_build(
        self: &Arc<Self>,
        platform: Platform,
        reruns: Vec<Rerun>,
    ) -> Result<serde_json::Value> {
        {
            // Under the dispatch claim's lock: a dispatch resetting its attempt here has no running row yet.
            let starting = self.starting();
            if starting.contains(&platform) {
                anyhow::bail!(
                    "install between specs: {platform} is starting an attempt; install once it's running and filed"
                );
            }

            self.with_run(|run, _| run.between_specs(platform))?;
            if !self.installing().insert(platform) {
                anyhow::bail!("{platform} is already installing a build");
            }
        }

        if let Err(e) = self.with_run(|run, now| run.install_started(now, platform)) {
            self.installing().remove(&platform);
            return Err(e);
        }

        let hub = self.clone();
        tokio::spawn(async move {
            let installed = match hub.install(platform).await {
                Ok(()) => hub.record_build(platform, reruns).await,
                Err(e) => Err(e),
            };
            // The end is journalled before the platform leaves `installing`, so no hold reads it as cut off.
            if let Err(e) = installed {
                let reason = format!("{e:#}");
                let _ = hub.with_run(|run, now| run.install_failed(now, platform, &reason));
            }

            hub.installing().remove(&platform);
        });

        Ok(serde_json::json!({
            "installing": platform,
            "result": format!(
                "the install runs on the server and its end comes as an event: `build ... installed on {platform}` or `install failed on {platform}`"
            ),
        }))
    }

    /// Logs the build the build-identity hook reads on `platform` with the listed reruns.
    async fn record_build(&self, platform: Platform, reruns: Vec<Rerun>) -> Result<()> {
        let build = self.read_build(platform).await?;
        self.forget_old_build(platform).await?;
        self.with_run(|run, now| run.install_build(now, platform, build, reruns))
            .map(|_| ())
    }

    /// A new build is on the platform: a device already connected forgets what it kept of the old one.
    pub async fn forget_old_build(&self, platform: Platform) -> Result<()> {
        let slot = self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&platform)
            .cloned();
        if let Some(slot) = slot
            && let Some(driver) = slot.lock().await.as_ref()
        {
            driver
                .device
                .new_build()
                .await
                .with_context(|| format!("couldn't clear the old build on {platform}"))?;
        }

        Ok(())
    }

    /// Starts the web app with the `chrome` platform's `serve` hook, stopping the one already running first. It runs
    /// until `stop_web_app` or the hub drops it. Without a `serve` hook there's nothing to start.
    pub async fn start_web_app(&self) -> Result<()> {
        let suite = self.suite();
        let Some(served) = suite.config.as_ref().and_then(SuiteConfig::served) else {
            return Ok(());
        };
        let mut web_app = self.web_app.lock().await;
        if let Some(running) = web_app.take() {
            running.stop(&served.url, hooks::START_TIMEOUT).await?;
        }
        *web_app = Some(
            hooks::Service::start(
                &suite.root,
                &served.command,
                &served.url,
                hooks::START_TIMEOUT,
                self.run_env(),
            )
            .await?,
        );
        Ok(())
    }

    /// For a way out that can't wait: dropping the service signals it to stop. One mid-restart is left to its task.
    pub fn stop_web_app(&self) {
        if let Ok(mut web_app) = self.web_app.try_lock() {
            web_app.take();
        }
    }

    /// Logs the platform's build when it isn't the last one logged, whoever installed it, so every attempt belongs to
    /// the build logged last before it. A build the hook can't identify produces no evidence: the dispatch is refused.
    async fn probe_build(&self, platform: Platform) -> Result<()> {
        let build = self.read_build(platform).await.with_context(|| {
            format!("can't tell which build is on {platform}, so nothing is dispatched there")
        })?;
        self.with_run(|run, now| {
            if run.state().builds.get(&platform) == Some(&build) {
                return Ok(());
            }

            run.install_build(now, platform, build, vec![]).map(|_| ())
        })
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
    /// Keeps a tester's latest full read of the screen, for its evidence if it fails.
    pub fn remember_screen(&self, token: &str, lines: Vec<String>) {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token.to_string(), (now_ms(), lines));
    }

    pub fn forget_screen(&self, token: &str) {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(token);
    }

    pub async fn capture(
        &self,
        token: &str,
        spec: &str,
        platform: Platform,
        attempt: u32,
    ) -> Vec<String> {
        let dir = self.run_dir.join(report::EVIDENCE_DIR);
        if std::fs::create_dir_all(&dir).is_err() {
            return Vec::new();
        }
        let base = report::evidence_base(spec, attempt, platform);
        let seen = self
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(token);
        let mut earlier = Vec::new();
        if let Some((at, lines)) = seen {
            let name = format!("{base}-seen.txt");
            let ago = now_ms().saturating_sub(at) / 1000;
            let text = format!(
                "The last screen the tester read, {ago} s before posting.\n\n{}",
                lines.join("\n")
            );
            if std::fs::write(dir.join(&name), text).is_ok() {
                earlier.push(name);
            }
        }
        let names = [
            format!("{base}-screen.webp"),
            format!("{base}-tree.txt"),
            format!("{base}-errors.txt"),
        ];
        let saved = self
            .on_device(platform, async |d| {
                let mut saved = Vec::new();
                if crate::report::capture_shot(&dir.join(&names[0]), async |png| {
                    d.device.screenshot(png).await
                })
                .await
                .is_ok()
                {
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
        earlier.extend(saved.unwrap_or_default());
        earlier
    }

    /// Writes the self-contained copy of the report into the run's folder and returns its path.
    pub fn export(&self, run: &Run, now: u64) -> Result<PathBuf> {
        let finished = run.state().finished;
        let html = report::document(&self.page(
            &self.suite(),
            run,
            Some(report::Export { at: now, finished }),
            None,
        ));
        let path = self.run_dir.join(EXPORT_FILE);
        std::fs::write(&path, html)?;
        Ok(path)
    }

    fn stall_silent_testers(&self) {
        let (silence, start) = (self.stall_ms, self.start_ms);
        let _ = self.with_run(|run, now| {
            for token in run.stalled(now, silence, start) {
                let reason = if run.started(&token) {
                    format!("no tool call for {} s", silence / 1000)
                } else {
                    format!("no first tool call {} s after dispatch", start / 1000)
                };
                run.stall(now, &token, &reason)?;
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
    let step_down = {
        let shutdown = shutdown.clone();
        // Only for looking at runs, so it always makes way for a new one.
        move || async move {
            shutdown.cancel();
            "the reports server stepped down\n"
        }
    };
    let app = axum::Router::new()
        .route("/", get(index.clone()))
        .route("/runs", get(index))
        .route("/step-down", post(step_down))
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
    let shutdown = hub.stop.clone();
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
    // This run's own page; any other run's `/{key}/` is the reports router's. The bare endpoints are the old
    // addresses, kept for the CLI and older links.
    let key = hub.page_key();
    let app = axum::Router::new()
        .nest_service("/mcp", mcp)
        .route("/events", get(events_stream))
        .route("/ready/{platform}", get(ready))
        .route("/tokens", post(tokens))
        .route("/ended", post(ended))
        .route("/dispatch", post(dispatch_route))
        .route("/drive", post(drive_route))
        .route("/summary", get(summary))
        .route("/next", get(next))
        .route("/", get(runs_index))
        .route(&format!("/{key}"), get(to_page))
        .route(&format!("/{key}/"), get(page))
        .route(&format!("/{key}/view"), get(view))
        .route(&format!("/{key}/updates"), get(updates))
        .route(&format!("/{key}/events"), get(events_stream))
        .route(&format!("/{key}/download"), get(download))
        .route(
            &format!("/{key}/files/evidence/{{name}}"),
            get(evidence_file),
        )
        .route("/download", get(download))
        .route("/files/evidence/{name}", get(evidence_file))
        .route("/runs", get(runs_index))
        .route("/step-down", post(step_down))
        .with_state(hub.clone())
        .merge(reports::router(hub.reports()))
        .layer(middleware::from_fn_with_state(hub.clone(), note_call))
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
    let starting = hub.start_state();
    let html = hub
        .with_run(|run, _| {
            Ok(report::document(&hub.page(
                &hub.suite(),
                run,
                None,
                starting.as_ref(),
            )))
        })
        .unwrap_or_default();
    axum::response::Html(html).into_response()
}

async fn download(State(hub): State<Arc<Hub>>) -> Response {
    let exported = hub.with_run(|run, now| {
        let path = hub.export(run, now)?;
        Ok(std::fs::read_to_string(path)?)
    });
    match exported {
        Ok(html) => reports::attachment(&hub.run_dir, html),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

async fn view(State(hub): State<Arc<Hub>>) -> Response {
    axum::Json(hub.view()).into_response()
}

/// Sends one line of a stream, or gives up when the server stops: a reader that stopped reading (a suspended tab)
/// would otherwise hold up graceful shutdown with the channel full. False once nobody will read it.
async fn send_line(
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    line: Bytes,
    stop: &CancellationToken,
) -> bool {
    tokio::select! {
        _ = stop.cancelled() => false,
        sent = tx.send(Ok(line)) => sent.is_ok(),
    }
}

/// Gathers what lands in this long after a change into one update, as a tester's post brings several entries.
const UPDATE_COALESCE: Duration = Duration::from_millis(250);
/// How often the feed says it's still there when nothing changes, so a page can tell a quiet run from a dead link.
pub const UPDATE_HEARTBEAT: Duration = Duration::from_secs(15);

/// One Server-Sent Event. Named events, not comments: a comment never reaches the page's script.
fn sse(event: &str, data: &serde_json::Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

/// The live page's feed, as Server-Sent Events: `view` with the whole view on every connect, then `patch`, an
/// RFC 7386 merge patch against what this connection last sent, whenever the view changes, and `ping` while
/// nothing does. A page that loses the connection reconnects and starts again from a whole view.
async fn updates(State(hub): State<Arc<Hub>>) -> Response {
    // Subscribe before the first view, so nothing written in between is lost.
    let mut live = hub.events.subscribe();
    let mut nudged = hub.nudges.subscribe();
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        let mut sent = hub.view();
        if !send_line(&tx, sse("view", &sent), &hub.stop).await {
            return;
        }

        let every = hub.heartbeat();
        let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        loop {
            // A stream never ends on its own, and graceful shutdown waits for it.
            tokio::select! {
                _ = hub.stop.cancelled() => return,
                _ = heartbeat.tick() => {
                    if !send_line(&tx, sse("ping", &serde_json::json!({})), &hub.stop).await {
                        return;
                    }

                    continue;
                }
                received = live.recv() => {
                    if let Err(broadcast::error::RecvError::Closed) = received {
                        return;
                    }
                }
                received = nudged.recv() => {
                    if let Err(broadcast::error::RecvError::Closed) = received {
                        return;
                    }
                }
            }
            tokio::select! {
                _ = hub.stop.cancelled() => return,
                _ = tokio::time::sleep(UPDATE_COALESCE) => {}
            }
            while matches!(
                live.try_recv(),
                Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_))
            ) {}
            while matches!(
                nudged.try_recv(),
                Ok(()) | Err(broadcast::error::TryRecvError::Lagged(_))
            ) {}

            let next = hub.view();
            if let Some(patch) = report::view::merge_patch(&sent, &next) {
                if !send_line(&tx, sse("patch", &patch), &hub.stop).await {
                    return;
                }

                sent = next;
            }
        }
    });
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            // Proxies that buffer responses would hold each event back.
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
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
async fn to_page(State(hub): State<Arc<Hub>>) -> Response {
    axum::response::Redirect::permanent(&format!("/{}/", hub.page_key())).into_response()
}

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
        .with_run(|run, _| Ok(report::run_summary(run, hub.label_of(run), hub.uncommitted)))
        .unwrap_or_default();
    runs.push((current.clone(), live, Some(format!("{}/", hub.page_key()))));
    reports::newest_first(&mut runs);
    axum::response::Html(report::index(&runs, &current)).into_response()
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    after: u64,
    /// The live page also hears when it should redraw for something outside the journal, as a line with the last
    /// entry's `seq` again.
    #[serde(default)]
    nudge: bool,
}

/// Newline-delimited journal entries after `after`, then each new one as it's written. Replaying from a cursor
/// means a coordinator whose Monitor lapsed misses nothing.
/// Tool calls are POSTs to `/mcp`; the live page's polling isn't anyone using the run.
async fn note_call(State(hub): State<Arc<Hub>>, request: Request, next: Next) -> Response {
    let path = request.uri().path();

    if request.method() == axum::http::Method::POST
        && (path.starts_with("/mcp") || path == "/dispatch")
    {
        hub.last_call
            .store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    }
    next.run(request).await
}

/// Makes way for a new run, but only from a finished run nobody has used for a while; otherwise 409 says what it's
/// doing and how to stop it.
async fn step_down(State(hub): State<Arc<Hub>>) -> Response {
    let finished = hub
        .with_run(|run, _| Ok(run.state().finished))
        .unwrap_or(false);
    let idle = now_ms().saturating_sub(hub.last_call.load(std::sync::atomic::Ordering::Relaxed));
    // A run that couldn't start only stays up to show why.
    let failed_start = hub.start_state().is_some_and(|s| s.failed.is_some());
    let folder = hub.run_dir.display();
    let pid = std::process::id();
    if !finished && !failed_start {
        return (
            StatusCode::CONFLICT,
            format!(
                "run {folder} is still going; carry it on, resume it later with `soundcheck run <suite> --resume {folder}`, or stop its server (`kill {pid}`) first\n"
            ),
        )
            .into_response();
    }

    if !failed_start && idle < STEP_DOWN_IDLE_MS {
        return (
            StatusCode::CONFLICT,
            format!(
                "run {folder} has finished but was used {} s ago, so its triage may be under way; stop its server (`kill {pid}`) once it's done with\n",
                idle / 1000
            ),
        )
            .into_response();
    }

    hub.stop.cancel();
    (
        StatusCode::OK,
        format!("run {folder} stepped down; its report stays at /runs\n"),
    )
        .into_response()
}

#[derive(Deserialize)]
struct DispatchRequest {
    /// The tester agent's prompt as the coordinator wrote it, which may name a row.
    #[serde(default)]
    prompt: String,
    session: Option<String>,
}

/// The plugin's PreToolUse hook starts a tester through this: the attempt's details, or 409 with the refusal.
async fn dispatch_route(
    State(hub): State<Arc<Hub>>,
    axum::Json(d): axum::Json<DispatchRequest>,
) -> Response {
    if let Some(session) = &d.session {
        hub.add_coordinator(session);
    }

    if let Some(dispatched) = hub.take_handed_out(&d.prompt) {
        return axum::Json(dispatched).into_response();
    }

    let (spec, platform) = hub.named_row(&d.prompt);
    match hub.dispatch(spec.as_deref(), platform).await {
        Ok(dispatched) => axum::Json(dispatched).into_response(),
        Err(e) => (StatusCode::CONFLICT, format!("{e:#}\n")).into_response(),
    }
}

/// `soundcheck drive` from a prepare step (DESIGN.md 5.6): the action on the device the run already holds.
async fn drive_route(
    State(hub): State<Arc<Hub>>,
    axum::Json(request): axum::Json<crate::drive::Request>,
) -> Response {
    let acted = async {
        let suite = hub.suite();
        let config = suite
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no suite.yaml"))?;
        let platform = config
            .platform(&request.platform)
            .map_err(anyhow::Error::msg)?;
        let driver = config.driver(&platform);
        hub.on_device(platform, async |d| {
            Ok(crate::drive::act(d, platform, driver, request.action).await)
        })
        .await?
    };
    match acted.await {
        Ok(out) => out.into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
struct SummaryQuery {
    session: Option<String>,
}

async fn summary(State(hub): State<Arc<Hub>>, Query(q): Query<SummaryQuery>) -> Response {
    axum::Json(hub.summary(q.session.as_deref())).into_response()
}

/// The row a dispatch would start now, without starting it, else 409 with why none can.
async fn next(State(hub): State<Arc<Hub>>) -> Response {
    match hub.pick(None, None).await {
        Ok((spec, platform)) => {
            axum::Json(serde_json::json!({ "spec": spec, "platform": platform })).into_response()
        }
        Err(e) => (StatusCode::CONFLICT, format!("{e:#}\n")).into_response(),
    }
}

#[derive(Deserialize)]
struct EndedRequest {
    token: String,
}

/// A tester agent has stopped: the plugin's SubagentStop hook says so after posting its tokens.
async fn ended(State(hub): State<Arc<Hub>>, axum::Json(e): axum::Json<EndedRequest>) -> Response {
    match hub.tester_ended(&e.token) {
        Ok(said) => (StatusCode::OK, said.map(|s| s + "\n").unwrap_or_default()).into_response(),
        Err(e) => (StatusCode::CONFLICT, format!("{e:#}\n")).into_response(),
    }
}

async fn tokens(
    State(hub): State<Arc<Hub>>,
    axum::Json(t): axum::Json<crate::tester_usage::TesterTokens>,
) -> Response {
    let recorded = hub.with_run(|run, now| {
        run.tokens(now, &t.token, t.tokens, Some(t.spent))?;
        // The last tester's tokens land after its result has finished the run and exported it.
        if run.state().finished {
            let _ = hub.export(run, now);
        }

        Ok(())
    });
    match recorded {
        Ok(()) => (
            StatusCode::OK,
            format!("{} used {} tokens\n", t.token, t.tokens),
        )
            .into_response(),
        Err(e) => (StatusCode::CONFLICT, format!("{e:#}\n")).into_response(),
    }
}

/// 200 when the platform can take a spec, else 409 with why it's held; `soundcheck ready` asks this.
async fn ready(State(hub): State<Arc<Hub>>, Path(name): Path<String>) -> Response {
    let platform = match hub.platform(&name) {
        Ok(platform) => platform,
        Err(e) => return (StatusCode::NOT_FOUND, format!("{e:#}\n")).into_response(),
    };
    match hub.hold(platform).await {
        None => (StatusCode::OK, format!("{platform} ready\n")).into_response(),
        Some(why) => (StatusCode::CONFLICT, format!("{why}\n")).into_response(),
    }
}

async fn events_stream(State(hub): State<Arc<Hub>>, Query(q): Query<EventsQuery>) -> Response {
    // Subscribe before reading history, so nothing written in between is lost.
    let mut live = hub.events.subscribe();
    let mut nudged = hub.nudges.subscribe();
    let nudge = q.nudge;
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
            if !send_line(&tx, line(entry), &hub.stop).await {
                return;
            }
        }
        loop {
            // A stream never ends on its own, and graceful shutdown waits for it: an open live page kept Ctrl-C and
            // stepping down for a new run waiting for ever (2026-10-26).
            let received = tokio::select! {
                _ = hub.stop.cancelled() => return,
                Ok(()) = nudged.recv(), if nudge => {
                    if !send_line(&tx, Bytes::from(format!("{{\"seq\":{last}}}\n")), &hub.stop).await {
                        return;
                    }
                    continue;
                }
                received = live.recv() => received,
            };
            match received {
                Ok(entry) if entry.seq > last => {
                    last = entry.seq;
                    if !send_line(&tx, line(&entry), &hub.stop).await {
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
                        if !send_line(&tx, line(entry), &hub.stop).await {
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
