//! The one process a run needs: MCP tools for testers and the coordinator, and the event stream, on one local port.

pub mod events;
pub mod hooks;
pub mod prompt;
pub mod reports;
pub mod tools;

use crate::report;
use crate::run::event::RowRef;
use crate::run::journal::Entry;
use crate::run::model::Build;
use crate::run::{Run, saved};
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

/// Connects a platform's device the way a run does: web launches its own Chrome against the suite's base-url; Android
/// and iOS connect to the running emulator or booted simulator to drive the suite's app-id. `headed` shows each
/// device in a window to watch: Chrome, Simulator.app, and the window of an emulator soundcheck boots.
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
            AnyDevice::Android(
                Android::connect(&app, reader("android", "reader.apk"))
                    .await?
                    .headed(headed),
            )
        }
        Platform::Ios => {
            let app = config
                .and_then(|p| p.app_id.clone())
                .ok_or_else(|| anyhow::anyhow!("suite.yaml gives ios no app-id"))?;
            AnyDevice::Ios(
                Ios::connect(&app, reader("ios", "reader"))
                    .await?
                    .headed(headed),
            )
        }
    })
}

/// A platform's driver as a run uses it: its device and, on Android and iOS, the suite's network proxy (started if
/// it isn't up), with Android's ports forwarded to it.
pub async fn driver(platform: Platform, suite: &Suite, headed: bool) -> Result<Driver<AnyDevice>> {
    let config = suite.config.as_ref();
    let device = connect(
        platform,
        config.and_then(|c| c.platforms.get(&platform)),
        headed,
    )
    .await?;
    let routes = config.and_then(crate::network::routes);
    let (device, network) = match (device, routes) {
        (device @ AnyDevice::Web(_), _) | (device, None) => (device, None),
        (AnyDevice::Android(android), Some(routes)) => {
            let network = crate::network::ensure(&suite.root, &routes).await?;
            let ports: Vec<u16> = routes.iter().map(|r| r.listen).collect();
            (
                AnyDevice::Android(android.forward(&ports).await?),
                Some(network),
            )
        }
        (device, Some(routes)) => (
            device,
            Some(crate::network::ensure(&suite.root, &routes).await?),
        ),
    };
    Ok(Driver::new(device).with_network(network))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
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
    /// A driver per platform, connected on its first device call.
    pub devices: tokio::sync::Mutex<std::collections::HashMap<Platform, Driver<AnyDevice>>>,
    pub stall_ms: u64,
    /// How long a dispatched tester has to make its first tool call before its spec counts as stalled.
    pub start_ms: u64,
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
    /// Each running tester's last full read of the screen and when, saved with a FAIL: by the time it posts, the
    /// screen has often moved on from the moment it judged.
    seen: Mutex<std::collections::HashMap<String, (u64, Vec<String>)>>,
}

/// At most one restart per platform in this window; a device still degraded after that is only noted.
const RESTART_COOLDOWN_MS: u64 = 15 * 60_000;
/// How long dispatches wait for a busy host before going ahead; a simulator's boot settled in about four minutes.
const HOST_WAIT_MAX_MS: u64 = 10 * 60_000;
const HOST_WAIT_POLL_MS: u64 = 15_000;
pub const START_STALL_MS: u64 = 10 * 60_000;

impl Hub {
    pub fn new(suite: Suite, run: Run, label: String, run_dir: PathBuf, stall_ms: u64) -> Arc<Hub> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Hub {
            suite: RwLock::new(Arc::new(suite)),
            label,
            run_dir,
            run: Mutex::new(run),
            events,
            devices: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            without_devices: Vec::new(),
            stall_ms,
            start_ms: START_STALL_MS,
            headed: false,
            restart_due: Mutex::new(std::collections::HashMap::new()),
            last_restart: Mutex::new(std::collections::HashMap::new()),
            held: Mutex::new(std::collections::HashMap::new()),
            gave_up: Mutex::new(std::collections::HashMap::new()),
            seen: Mutex::new(std::collections::HashMap::new()),
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

    /// The suite specs are dispatched from: as loaded at start, plus any specs added since.
    pub fn suite(&self) -> Arc<Suite> {
        self.suite.read().unwrap_or_else(|e| e.into_inner()).clone()
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

    fn page<'a>(
        &'a self,
        suite: &'a Suite,
        run: &'a Run,
        export: Option<report::Export>,
    ) -> report::Page<'a> {
        report::Page {
            suite,
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
            slot.insert(driver(platform, &self.suite(), self.headed).await?);
        }
        Ok(op(&devices[&platform]).await?)
    }

    /// Resets a platform for a spec: guard, seed, relaunch, then its starts-on labels. Tried three times; the
    /// error after the last says why, and the caller files the spec BLOCKED as a reset failure.
    pub async fn prepare(&self, spec: &Spec, platform: Platform) -> Result<Option<String>> {
        let suite = self.suite();
        let config = suite
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no suite.yaml"))?;
        let root = &suite.root;
        let mut last = anyhow::anyhow!("not tried");
        for _ in 0..3 {
            let attempt = async {
                // Every spec starts online with no faults, whatever the last one left; first, since the seed may go through it.
                if let Some(routes) = crate::network::routes(config).filter(|_| {
                    platform != Platform::Web && !self.without_devices.contains(&platform)
                }) {
                    crate::network::ensure(root, &routes).await?.reset().await?;
                }
                hooks::run(root, &config.hooks.guard, hooks::GUARD_TIMEOUT)
                    .await
                    .context("the guard refused")?;
                let seed = hooks::fill(&config.hooks.seed, &spec.front.seed, &platform.to_string());
                let out = hooks::run(root, &seed, hooks::SEED_TIMEOUT).await?;
                if !self.without_devices.contains(&platform) {
                    let starts_on = spec.front.starts_on.clone();
                    let hardware = spec.front.keyboard == crate::suite::spec::Keyboard::Hardware;
                    self.on_device(platform, async |d| {
                        d.device.hardware_keyboard(hardware).await?;
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
            // A refused dispatch quotes the latest reading, not the one that started the hold.
            self.hold_for(platform, &format!("the Mac is busy ({reason})"));
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
            .on_device(platform, async |d| {
                // An attempt that ended offline or with a fault mustn't leave the app cut off between specs.
                if let Some(network) = &d.network {
                    network.reset().await?;
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
        let suite = self.suite();
        let dir = suite.config.as_ref().and_then(|c| c.fixtures.clone());
        suite.root.join(dir.unwrap_or_else(|| "fixtures".into()))
    }

    /// Runs the project's check or act hook: `<hook> <name> <args...>`, each quoted for the shell. A failing call
    /// is still evidence, so its exit and output come back rather than an error.
    pub async fn project_hook(&self, kind: &str, name: &str, args: &[String]) -> Result<String> {
        let suite = self.suite();
        let config = suite
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
        match hooks::run(&suite.root, &command, hooks::SEED_TIMEOUT).await {
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
        let command = hooks::fill(&config.hooks.build_identity, "", &platform.to_string());
        let out = hooks::run(&suite.root, &command, hooks::GUARD_TIMEOUT).await?;
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
        let Some(install) = &config.hooks.install else {
            anyhow::bail!("suite.yaml has no install hook, so builds are installed by hand");
        };
        let command = hooks::fill(install, "", &platform.to_string());
        hooks::run(&suite.root, &command, hooks::INSTALL_TIMEOUT)
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
        .route("/download", get(download))
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
        .with_run(|run, _| Ok(report::document(&hub.page(&hub.suite(), run, None))))
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

async fn fragment(State(hub): State<Arc<Hub>>) -> Response {
    let html = hub
        .with_run(|run, _| Ok(report::content(&hub.page(&hub.suite(), run, None))))
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
