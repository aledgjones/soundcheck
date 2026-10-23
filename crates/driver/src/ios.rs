//! The iOS simulator: reads through the reader (`readers/ios`, one long-lived accessibility session serving
//! `axe describe-ui` JSON) and input through its held HID session. There's no fallback: a reader gap is a bug to fix.
//! Points everywhere but screenshots, which are in pixels.

use crate::Platform;
use crate::device::{
    Device, DriverError, Health, Result, Stop, double_tap_script, swipe_script, touch_script,
};
use crate::element::{Element, Rect};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout};

const READER_PORT: u16 = 9019;

fn device(e: impl std::fmt::Display) -> DriverError {
    DriverError::Device(e.to_string())
}

fn text(node: &Value, key: &str) -> Option<String> {
    node.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parses `axe describe-ui` JSON into elements, in tree order. React Native nests the same text twice, so repeats at
/// the same point are dropped, as are elements off the screen. The default view leaves out labels that are only an
/// accessibility id, which VoiceOver never speaks; `all` keeps them.
pub fn parse(json: &str, all: bool) -> Result<Vec<Element>> {
    let tree: Value = serde_json::from_str(json).map_err(device)?;
    let roots = tree.as_array().cloned().unwrap_or_else(|| vec![tree]);
    let Some(screen) = roots.first().map(frame) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack: Vec<&Value> = roots.iter().rev().collect();
    while let Some(node) = stack.pop() {
        if let Some(children) = node.get("children").and_then(Value::as_array) {
            stack.extend(children.iter().rev());
        }
        let role = text(node, "type");
        if role.as_deref() == Some("Application") {
            continue;
        }
        let (label, value, id) = (
            text(node, "AXLabel"),
            text(node, "AXValue"),
            text(node, "AXUniqueId"),
        );
        let shown = match (label.clone().or(value.clone()), all) {
            (Some(s), _) => s,
            (None, true) if id.is_some() => id.clone().unwrap_or_default(),
            _ => continue,
        };
        let f = frame(node);
        let (cx, cy) = f.centre();
        if f.width <= 0.0
            || f.height <= 0.0
            || cx < 0.0
            || cy < 0.0
            || cx > screen.width
            || cy > screen.height
        {
            continue;
        }
        if !seen.insert(format!("{cx:.0},{cy:.0}|{label:?}|{value:?}|{id:?}")) {
            continue;
        }
        let switch = role.as_deref() == Some("Switch");
        let traits: Vec<&str> = node
            .get("traits")
            .and_then(Value::as_array)
            .map(|t| t.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        out.push(Element {
            value: value.filter(|v| !switch && *v != shown),
            checked: switch.then(|| text(node, "AXValue").as_deref() == Some("1")),
            selected: traits.contains(&"Selected").then_some(true),
            label: shown,
            id,
            role,
            frame: f,
            enabled: node.get("enabled").and_then(Value::as_bool) != Some(false),
            expanded: None,
            focused: false,
        });
    }
    Ok(out)
}

fn frame(node: &Value) -> Rect {
    let n = |k: &str| {
        node.get("frame")
            .and_then(|f| f.get(k))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    };
    Rect {
        x: n("x"),
        y: n("y"),
        width: n("width"),
        height: n("height"),
    }
}

/// Whether a tree has anything in it: just after boot or a launch the app answers with no children.
/// Seconds since the newest process matching `pattern` started, from `ps`'s `[[dd-]hh:]mm:ss`.
async fn started_seconds_ago(pattern: &str) -> Option<u64> {
    let pid = run("pgrep", &["-n", "-f", pattern]).await.ok()?;
    let elapsed = run("ps", &["-o", "etime=", "-p", pid.trim()]).await.ok()?;
    parse_etime(elapsed.trim())
}

fn parse_etime(etime: &str) -> Option<u64> {
    let (days, clock) = match etime.split_once('-') {
        Some((d, rest)) => (d.parse::<u64>().ok()?, rest),
        None => (0, etime),
    };
    let seconds = clock.split(':').try_fold(0u64, |total, part| {
        Some(total * 60 + part.parse::<u64>().ok()?)
    })?;
    Some(days * 86_400 + seconds)
}

fn has_content(json: &str) -> bool {
    parse(json, true).is_ok_and(|e| !e.is_empty())
}

/// HID keyboard usage codes, which the reader's `k` step takes.
fn keycode(key: &str) -> Option<u32> {
    Some(match key {
        "ArrowRight" => 79,
        "ArrowLeft" => 80,
        "ArrowDown" => 81,
        "ArrowUp" => 82,
        "Enter" => 40,
        "Escape" => 41,
        "Backspace" => 42,
        "Tab" => 43,
        "Space" | " " => 44,
        "Delete" => 76,
        "PageUp" => 75,
        "PageDown" => 78,
        _ => return None,
    })
}

pub struct Ios {
    udid: String,
    bundle_id: String,
    reader: Option<PathBuf>,
    /// When the app was last launched: crash reports from before then are someone else's problem.
    launched: std::sync::Mutex<SystemTime>,
    /// How long the last launch took to show a screen, in milliseconds.
    last_launch_ms: std::sync::Mutex<Option<u64>>,
    /// Whether the hardware keyboard is known to be connected, so typing needn't look for the on-screen one.
    hardware_keyboard: std::sync::atomic::AtomicBool,
    /// Whether Simulator.app shows the device; the simulator itself runs either way.
    headed: bool,
    /// When the reader was last checked against the simulator's boot, so the check costs a `ps` every few seconds.
    boot_checked: std::sync::Mutex<Option<std::time::Instant>>,
    /// What soundcheck restarted on its own and why, until health reports it.
    restarts: std::sync::Mutex<Vec<String>>,
    /// How many times a lost reader channel rebooted the simulator, and when it last did.
    recoveries: std::sync::atomic::AtomicU32,
    recovered_at: std::sync::Mutex<Option<std::time::Instant>>,
}

/// What a reader's `/ping` says: `ok <udid> <build id> <pid> <started, unix seconds>`. A reader built before it
/// carried them says only `ok <udid>`.
#[derive(Debug, PartialEq)]
struct Ping {
    udid: String,
    build: Option<String>,
    pid: Option<u32>,
    started: Option<u64>,
}

fn parse_ping(body: &str) -> Option<Ping> {
    let mut parts = body.split_whitespace();
    if parts.next()? != "ok" {
        return None;
    }
    Some(Ping {
        udid: parts.next()?.to_string(),
        build: parts.next().map(str::to_string),
        pid: parts.next().and_then(|p| p.parse().ok()),
        started: parts.next().and_then(|s| s.parse().ok()),
    })
}

/// Why a reader answering for this simulator must be replaced: a build other than the one on disk, or a start
/// before the simulator's current boot, whose input session answers but lands nowhere.
fn stale_reader(ping: &Ping, built: Option<&str>, booted_at: Option<u64>) -> Option<&'static str> {
    if let Some(built) = built
        && ping.build.as_deref() != Some(built)
    {
        return Some("it was another build than the one on disk");
    }
    // `ps` and the reader both count whole seconds, so a couple of seconds' slack.
    if let (Some(started), Some(boot)) = (ping.started, booted_at)
        && started + 2 < boot
    {
        return Some("the simulator had rebooted under it");
    }
    None
}

/// The reader's link to the simulator broke; only a simulator reboot mends it (2026-10-23: five times in one run).
fn lost_channel(e: &DriverError) -> bool {
    e.to_string().contains("Channel disconnected")
}

/// At most one simulator reboot for a lost channel in this window, so a reader that keeps failing isn't rebooted
/// in a loop.
const RECOVERY_COOLDOWN: Duration = Duration::from_secs(120);

/// Health limits for the Mac that runs the simulator, and the app's launch.
const MIN_FREE_MEMORY_PERCENT: f64 = 10.0;
const MAX_LOAD_PER_CORE: f64 = 2.0;
const MAX_LAUNCH_MS: u64 = 20_000;

/// The on-screen key that types `c`, and whether it needs shift. Letter keys show their case (they change with
/// shift), so a letter carries no shift here: [`Ios::type_key`] reads which case is showing.
fn key_for(c: char) -> Option<(String, bool)> {
    const SHIFTED: &str = "!@#$%^&*()_+{}|:\"~<>?";
    const BASE: &str = "1234567890-=[]\\;'`,./";
    Some(match c {
        ' ' => ("space".into(), false),
        '\n' => ("return".into(), false),
        '\t' => ("Tab".into(), false),
        c if c.is_ascii_alphabetic() => (c.to_string(), false),
        c if BASE.contains(c) => (c.to_string(), false),
        c => {
            let i = SHIFTED.find(c)?;
            (BASE[i..].chars().next()?.to_string(), true)
        }
    })
}

fn key_centre(keys: &[Element], label: &str) -> Option<(f64, f64)> {
    keys.iter()
        .find(|e| e.label == label)
        .map(Element::tap_point)
}

impl Ios {
    /// Sends a script of input events through the reader's held HID session: about 0.1 s a tap, where starting
    /// a process for each took about 1.3 s. The script's steps are the reader's (`d`, `u`, `w`, `k`, `kd`, `ku`).
    async fn hid(&self, script: &str) -> Result<()> {
        self.ensure_reader().await?;
        let path = format!("/hid?{}", script.replace(' ', "%20"));
        match self.reader_get(&path).await {
            Ok(_) => return Ok(()),
            Err(e) if lost_channel(&e) => return Err(self.recover(e).await),
            Err(_) => {}
        }
        // A reader from before a simulator reboot can have a dead session; a fresh one doesn't.
        self.stop_reader().await;
        self.ensure_reader().await?;
        match self.reader_get(&path).await {
            Ok(_) => Ok(()),
            Err(e) if lost_channel(&e) => Err(self.recover(e).await),
            Err(e) => Err(e),
        }
    }

    /// The screen's elements when the on-screen keyboard is up (its `space` and `shift` keys show), else none.
    async fn on_screen_keyboard(&self) -> Result<Option<Vec<Element>>> {
        if self
            .hardware_keyboard
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(None);
        }
        let elements = self.read(true).await?;
        let up = ["space", "shift"]
            .iter()
            .all(|k| elements.iter().any(|e| e.label == *k));
        Ok(up.then_some(elements))
    }

    /// Types one character by tapping the on-screen keyboard.
    async fn type_key(&self, c: char) -> Result<()> {
        let (key, shifted) = key_for(c).ok_or_else(|| {
            DriverError::Invalid(format!("{c:?} isn't on the on-screen keyboard"))
        })?;
        let keys = self.read(true).await?;
        let missing = || DriverError::NotFound(format!("the on-screen keyboard's {key:?} key"));
        let shift =
            || key_centre(&keys, "shift").ok_or_else(|| DriverError::NotFound("shift".into()));
        let at = if c.is_ascii_alphabetic() {
            // The keys show the case they'll type; if it's the other one, shift first.
            match key_centre(&keys, &key) {
                Some(at) => at,
                None => {
                    let other: String = key
                        .chars()
                        .flat_map(|k| {
                            if k.is_ascii_uppercase() {
                                k.to_lowercase().collect::<Vec<_>>()
                            } else {
                                k.to_uppercase().collect()
                            }
                        })
                        .collect();
                    let at = key_centre(&keys, &other).ok_or_else(missing)?;
                    let (x, y) = shift()?;
                    self.tap_at(x, y).await?;
                    at
                }
            }
        } else {
            let at = key_centre(&keys, &key).ok_or_else(missing)?;
            if shifted {
                let (x, y) = shift()?;
                self.tap_at(x, y).await?;
            }
            at
        };
        self.tap_at(at.0, at.1).await
    }
    /// Connects to the booted simulator (or `SOUNDCHECK_IOS_UDID`) to drive `bundle_id`. Simulators only:
    /// `simctl` never lists a physical device.
    pub async fn connect(bundle_id: &str, reader: Option<PathBuf>) -> Result<Ios> {
        let udid = match std::env::var("SOUNDCHECK_IOS_UDID") {
            Ok(u) => u,
            Err(_) => {
                let list = run("xcrun", &["simctl", "list", "devices", "booted", "--json"]).await?;
                let parsed: Value = serde_json::from_str(&list).map_err(device)?;
                let booted: Vec<String> = parsed["devices"]
                    .as_object()
                    .into_iter()
                    .flat_map(|runtimes| runtimes.values())
                    .filter_map(Value::as_array)
                    .flatten()
                    .filter(|d| d["state"] == "Booted")
                    .filter_map(|d| d["udid"].as_str().map(str::to_string))
                    .collect();
                match booted.as_slice() {
                    [one] => one.clone(),
                    [] => return Err(device("no booted simulator")),
                    _ => {
                        return Err(device(
                            "several simulators are booted; set SOUNDCHECK_IOS_UDID to the one to drive",
                        ));
                    }
                }
            }
        };
        let ios = Ios {
            udid,
            bundle_id: bundle_id.into(),
            reader,
            launched: std::sync::Mutex::new(SystemTime::now()),
            last_launch_ms: std::sync::Mutex::new(None),
            hardware_keyboard: std::sync::atomic::AtomicBool::new(false),
            headed: false,
            boot_checked: std::sync::Mutex::new(None),
            restarts: std::sync::Mutex::new(Vec::new()),
            recoveries: std::sync::atomic::AtomicU32::new(0),
            recovered_at: std::sync::Mutex::new(None),
        };
        ios.no_hardware_keyboard().await?;
        run("xcrun", &["simctl", "bootstatus", &ios.udid, "-b"]).await?;
        Ok(ios)
    }

    /// With the reader's keyboard attached, iOS counts a hardware keyboard and keeps the on-screen one out of sight; most
    /// users have none. The setting applies at boot, so changing it means a reboot.
    async fn no_hardware_keyboard(&self) -> Result<()> {
        let prefs = home().join("Library/Preferences/com.apple.iphonesimulator.plist");
        let prefs = prefs.display().to_string();
        let key = format!(":DevicePreferences:{}:ConnectHardwareKeyboard", self.udid);
        let buddy = |command: String| {
            let prefs = prefs.clone();
            async move { run("/usr/libexec/PlistBuddy", &["-c", &command, &prefs]).await }
        };
        if buddy(format!("Print {key}"))
            .await
            .is_ok_and(|v| v.trim() == "false")
        {
            return Ok(());
        }
        if buddy(format!("Set {key} false")).await.is_err() {
            let _ = buddy(format!("Add :DevicePreferences:{} dict", self.udid)).await;
            buddy(format!("Add {key} bool false")).await?;
        }
        let _ = run("killall", &["cfprefsd"]).await;
        let _ = run("xcrun", &["simctl", "shutdown", &self.udid]).await;
        Ok(())
    }

    async fn reader_get(&self, path: &str) -> Result<String> {
        let get = async {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", READER_PORT)).await?;
            stream
                .write_all(format!("GET {path} HTTP/1.0\r\n\r\n").as_bytes())
                .await?;
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await?;
            std::io::Result::Ok(String::from_utf8_lossy(&buf).into_owned())
        };
        let response = timeout(Duration::from_secs(15), get)
            .await
            .map_err(|_| device("the reader didn't answer within 15 s"))?
            .map_err(device)?;
        let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
        if !head.starts_with("HTTP/1.0 200") {
            return Err(device(format!(
                "reader: {} {}",
                head.lines().next().unwrap_or(""),
                body.trim()
            )));
        }
        Ok(body.to_string())
    }

    /// Whatever reader answers on the port, whichever simulator it's for.
    async fn ping(&self) -> Option<Ping> {
        // Short: stopping a wedged reader asks it first.
        let body = timeout(Duration::from_secs(2), self.reader_get("/ping"))
            .await
            .ok()?
            .ok()?;
        parse_ping(&body)
    }

    async fn reader_alive(&self) -> bool {
        self.ping().await.is_some_and(|p| p.udid == self.udid)
    }

    /// Stops the reader by the pid it reports, or the one listening on its port, so it goes whatever path started
    /// it (a `cargo test` reader once outlived its tests and took the server's taps); the path is the last resort.
    async fn stop_reader(&self) {
        let pid = match self.ping().await.and_then(|p| p.pid) {
            Some(pid) => Some(pid.to_string()),
            None => run(
                "lsof",
                &["-t", &format!("-iTCP:{READER_PORT}"), "-sTCP:LISTEN"],
            )
            .await
            .ok()
            .and_then(|out| out.lines().next().map(str::to_string)),
        };
        let Some(pid) = pid else {
            if let Some(binary) = &self.reader {
                let _ = run("pkill", &["-f", &binary.display().to_string()]).await;
            }
            return;
        };
        let _ = run("kill", &[&pid]).await;
        // The next reader binds the same port, so wait for this one to go.
        for _ in 0..30 {
            if run("kill", &["-0", &pid]).await.is_err() {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
        let _ = run("kill", &["-9", &pid]).await;
    }

    /// Why the running reader must be replaced, checked every few seconds at most: the build id on disk is a file
    /// read and the simulator's boot a `ps`.
    async fn reader_stale(&self, ping: &Ping) -> Option<&'static str> {
        {
            let mut checked = self.boot_checked.lock().unwrap_or_else(|e| e.into_inner());
            if checked.is_some_and(|at| at.elapsed() < Duration::from_secs(5)) {
                return None;
            }
            *checked = Some(std::time::Instant::now());
        }
        let built = self
            .reader
            .as_ref()
            .and_then(|b| std::fs::read_to_string(b.with_file_name("reader.build-id")).ok());
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let booted_at = started_seconds_ago(&format!("launchd_sim .*{}", self.udid))
            .await
            .map(|ago| now.saturating_sub(ago));
        stale_reader(ping, built.as_deref().map(str::trim), booted_at)
    }

    fn note_restart(&self, what: String) {
        self.restarts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(what);
    }

    /// Starts the reader if none answers for this simulator, or replaces a stale one; one attached to another
    /// simulator is no use. True when it started a fresh one.
    async fn ensure_reader(&self) -> Result<bool> {
        if let Some(ping) = self.ping().await.filter(|p| p.udid == self.udid) {
            match self.reader_stale(&ping).await {
                None => return Ok(false),
                Some(why) => self.note_restart(format!("restarted the reader: {why}")),
            }
        }
        let Some(binary) = &self.reader else {
            return Err(device("no iOS reader built (readers/ios/build.sh)"));
        };
        self.stop_reader().await;
        let spawn = || {
            std::process::Command::new(binary)
                .args(["--udid", &self.udid, "--port", &READER_PORT.to_string()])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(device)
        };
        let mut child = spawn()?;
        for _ in 0..40 {
            if self.reader_alive().await {
                return Ok(true);
            }
            // One that can't bind the port yet (still in use as the last one closes) exits at once.
            if matches!(child.try_wait(), Ok(Some(_))) {
                child = spawn()?;
            }
            sleep(Duration::from_millis(250)).await;
        }
        Err(device("the iOS reader didn't start"))
    }

    /// A fresh reader's first read, given a minute: just after a boot the simulator's automation session takes a
    /// while to come up ("Timed out creating the simulator remote automation session").
    async fn first_read(&self, path: &str) -> Result<String> {
        let started = std::time::Instant::now();
        loop {
            match self.reader_get(path).await {
                Ok(json) => return Ok(json),
                Err(e) if lost_channel(&e) || started.elapsed() > Duration::from_secs(60) => {
                    return Err(e);
                }
                Err(_) => sleep(Duration::from_secs(1)).await,
            }
        }
    }

    async fn screen_json(&self, all: bool) -> Result<String> {
        let path = if all { "/describe" } else { "/describe?lean" };
        // A fresh reader that never reads has lost its channel as surely as one that says so.
        if self.ensure_reader().await?
            && let Err(e) = self.first_read(path).await
        {
            return Err(self.recover(e).await);
        }
        let mut last = device("not read");
        for _ in 0..3 {
            match self.reader_get(path).await {
                Ok(json) if has_content(&json) => return Ok(json),
                Ok(json) => last = device(format!("an empty screen: {}", json.trim())),
                Err(e) if lost_channel(&e) => return Err(self.recover(e).await),
                Err(e) => {
                    last = e;
                    break;
                }
            }
            sleep(Duration::from_millis(300)).await;
        }
        // A reader from before a simulator reboot answers pings but can't read; the next read starts a fresh one.
        self.stop_reader().await;
        Err(last)
    }

    /// Mends a reader that lost its channel to the simulator the way only a reboot does: stops the reader, reboots
    /// the simulator, starts a fresh reader and checks it reads. The app's state is gone with the reboot, so the
    /// call that hit it fails with a harness error saying so; the next spec starts on the working reader.
    async fn recover(&self, cause: DriverError) -> DriverError {
        let lately = {
            let mut at = self.recovered_at.lock().unwrap_or_else(|e| e.into_inner());
            let lately = at.is_some_and(|t| t.elapsed() < RECOVERY_COOLDOWN);
            if !lately {
                *at = Some(std::time::Instant::now());
            }
            lately
        };
        if lately {
            self.stop_reader().await;
            return cause;
        }
        self.recoveries
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mended = async {
            self.restart().await?;
            self.ensure_reader().await?;
            self.first_read("/describe?lean").await
        }
        .await;
        match mended {
            Ok(_) => {
                self.note_restart(format!(
                    "restarted the simulator: the reader lost its channel ({cause})"
                ));
                device(format!(
                    "harness: the reader lost its channel to the simulator ({cause}), so soundcheck restarted the \
                     simulator and its reader; the app's state went with it, so this spec needs a rerun"
                ))
            }
            Err(e) => {
                self.note_restart(format!(
                    "restarted the simulator: the reader lost its channel ({cause}), and a fresh reader still can't \
                     read ({e})"
                ));
                device(format!(
                    "harness: the reader lost its channel to the simulator ({cause}); restarting the simulator didn't \
                     mend it ({e})"
                ))
            }
        }
    }

    /// "On My iPad" in Files: the LocalStorage app group's `File Provider Storage`, whose folder differs per simulator.
    fn files_storage(&self) -> Result<PathBuf> {
        let groups = home()
            .join("Library/Developer/CoreSimulator/Devices")
            .join(&self.udid)
            .join("data/Containers/Shared/AppGroup");
        for entry in std::fs::read_dir(&groups).map_err(device)?.flatten() {
            let metadata = entry
                .path()
                .join(".com.apple.mobile_container_manager.metadata.plist");
            let id = std::process::Command::new("plutil")
                .args(["-extract", "MCMMetadataIdentifier", "raw"])
                .arg(&metadata)
                .output();
            if id.is_ok_and(|o| {
                String::from_utf8_lossy(&o.stdout).trim()
                    == "group.com.apple.FileProvider.LocalStorage"
            }) {
                return Ok(entry.path().join("File Provider Storage"));
            }
        }
        Err(device(
            "the Files app's local storage isn't there yet; open Files once",
        ))
    }

    /// Shows the simulator in Simulator.app now and after a restart; without it the simulator runs with no window.
    pub fn headed(self, headed: bool) -> Ios {
        if headed {
            self.show();
        }
        Ios { headed, ..self }
    }

    fn show(&self) {
        let _ = std::process::Command::new("open")
            .args([
                "-a",
                "Simulator",
                "--args",
                "-CurrentDeviceUDID",
                &self.udid,
            ])
            .status();
    }

    pub fn udid(&self) -> &str {
        &self.udid
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// Under heavy load a simulator tool can stall for minutes, so every call has a limit.
async fn run(program: &str, args: &[&str]) -> Result<String> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .kill_on_drop(true)
        .output();
    let out = timeout(Duration::from_secs(60), child)
        .await
        .map_err(|_| {
            device(format!(
                "{program} {} took longer than 60 s",
                args.join(" ")
            ))
        })?
        .map_err(|e| device(format!("{program}: {e}")))?;
    if !out.status.success() {
        return Err(device(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Points along a drag, about 10 apart, ending at `to`.
/// The USB HID usage code that types `c` on a US keyboard, and whether it needs shift (as AXe's table).
fn char_key(c: char) -> Option<(u32, bool)> {
    const BASE: &str = "1234567890-=[]\\;'`,./";
    const SHIFTED: &str = "!@#$%^&*()_+{}|:\"~<>?";
    const CODES: [u32; 21] = [
        30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 45, 46, 47, 48, 49, 51, 52, 53, 54, 55, 56,
    ];
    Some(match c {
        'a'..='z' => (4 + (c as u32 - 'a' as u32), false),
        'A'..='Z' => (4 + (c as u32 - 'A' as u32), true),
        ' ' => (44, false),
        '\n' => (40, false),
        '\t' => (43, false),
        c => match (BASE.find(c), SHIFTED.find(c)) {
            (Some(i), _) => (CODES[i], false),
            (_, Some(i)) => (CODES[i], true),
            _ => return None,
        },
    })
}

/// A HID script that types `text`, or none if a character has no key. Keys sent back to back were dropped by a field
/// that re-renders as it changes, so each waits a moment, still far quicker than a person.
fn type_script(text: &str) -> Option<String> {
    let mut steps = Vec::new();
    for (i, c) in text.chars().enumerate() {
        if i > 0 {
            steps.push("w 30".to_string());
        }
        let (code, shift) = char_key(c)?;
        if shift {
            steps.push("kd 225".to_string());
        }
        steps.push(format!("k {code}"));
        if shift {
            steps.push("ku 225".to_string());
        }
    }
    Some(steps.join(";"))
}

/// How long a moving touch takes per step: iOS reads quicker moves as a flick.
const STEP_MS: u64 = 30;

impl Device for Ios {
    fn platform(&self) -> Platform {
        Platform::Ios
    }

    async fn read(&self, all: bool) -> Result<Vec<Element>> {
        parse(&self.screen_json(all).await?, all)
    }

    async fn tap_at(&self, x: f64, y: f64) -> Result<()> {
        // A touch held for 80 ms registers as a tap every time; an instant down and up could be missed.
        self.hold_at(x, y, 80).await
    }

    async fn hold_at(&self, x: f64, y: f64, ms: u64) -> Result<()> {
        let stop = Stop { x, y, pause_ms: ms };
        self.hid(&touch_script(&[stop], STEP_MS, false)).await
    }

    async fn touch(&self, stops: &[Stop], keep: bool) -> Result<()> {
        self.hid(&touch_script(stops, STEP_MS, keep)).await
    }

    async fn release(&self, back: bool) -> Result<()> {
        self.ensure_reader().await?;
        self.reader_get(if back { "/release?back" } else { "/release" })
            .await
            .map(|_| ())
    }

    async fn swipe(&self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        self.hid(&swipe_script(from, to, ms)).await
    }

    async fn type_text(&self, text: &str) -> Result<()> {
        // Key events arrive as a hardware keyboard, which iOS ignores while the simulator's is disconnected (a spec
        // with `keyboard: software`); then only the on-screen keys reach the field.
        if self.on_screen_keyboard().await?.is_some() {
            for c in text.chars() {
                self.type_key(c).await?;
            }
            return Ok(());
        }
        let script = type_script(text).ok_or_else(|| {
            DriverError::Invalid(format!("{text:?} has a character with no US key"))
        })?;
        self.hid(&script).await?;
        sleep(Duration::from_millis(100)).await;
        Ok(())
    }

    async fn clear(&self, chars: usize) -> Result<()> {
        if let Some(keys) = self.on_screen_keyboard().await? {
            let delete = key_centre(&keys, "delete").ok_or_else(|| {
                DriverError::NotFound("the on-screen keyboard's delete key".into())
            })?;
            // Backwards from the cursor, which `fill` puts at the end of the field.
            let tap = format!(
                "d {:.0} {:.0};w 60;u {:.0} {:.0}",
                delete.0, delete.1, delete.0, delete.1
            );
            let script = vec![tap; chars].join(";w 40;");
            return self.hid(&script).await;
        }
        // A tap can leave the cursor mid-text, so delete forward and back from wherever it is.
        let script = std::iter::repeat_n("k 76", chars)
            .chain(std::iter::repeat_n("k 42", chars))
            .collect::<Vec<_>>()
            .join(";");
        self.hid(&script).await
    }

    async fn launch(&self) -> Result<()> {
        // On iPadOS 26 every other relaunch left the tree reading DockFolderViewService for good; launching again
        // clears it.
        for attempt in 0..3 {
            let _ = run(
                "xcrun",
                &["simctl", "terminate", &self.udid, &self.bundle_id],
            )
            .await;
            *self.launched.lock().unwrap_or_else(|e| e.into_inner()) = SystemTime::now();
            let started = std::time::Instant::now();
            run("xcrun", &["simctl", "launch", &self.udid, &self.bundle_id]).await?;
            let recoveries = self.recoveries.load(std::sync::atomic::Ordering::Relaxed);
            let mut json = String::new();
            while started.elapsed() < Duration::from_secs(60) {
                json = self.screen_json(true).await.unwrap_or_default();
                if has_content(&json) {
                    break;
                }
                sleep(Duration::from_millis(300)).await;
            }
            // A lost reader channel rebooted the simulator mid-launch, taking the app with it.
            if attempt < 2
                && self.recoveries.load(std::sync::atomic::Ordering::Relaxed) != recoveries
            {
                continue;
            }
            *self
                .last_launch_ms
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(started.elapsed().as_millis() as u64);
            // The first screen can still be settling.
            sleep(Duration::from_secs(1)).await;
            if attempt == 2 || !json.contains("DockFolderViewService") {
                break;
            }
        }
        Ok(())
    }

    async fn screenshot(&self, path: &Path) -> Result<()> {
        let absolute = std::path::absolute(path).map_err(device)?;
        run(
            "xcrun",
            &[
                "simctl",
                "io",
                &self.udid,
                "screenshot",
                &absolute.display().to_string(),
            ],
        )
        .await
        .map(|_| ())
    }

    /// Crash reports for the app written since its launch.
    async fn errors(&self) -> Result<Vec<String>> {
        let since = *self.launched.lock().unwrap_or_else(|e| e.into_inner());
        let dir = home().join("Library/Logs/DiagnosticReports");
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(device)?.flatten() {
            let p = entry.path();
            let recent = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t > since);
            if !recent || p.extension().is_none_or(|e| e != "ips") {
                continue;
            }
            // The first line is a JSON header naming the app.
            let head = std::fs::read_to_string(&p)
                .ok()
                .and_then(|s| s.lines().next().map(str::to_string))
                .unwrap_or_default();
            let header: Value = serde_json::from_str(&head).unwrap_or_default();
            if header["bundleID"] == self.bundle_id.as_str() {
                found.push(format!("crash report {}", p.display()));
            }
        }
        Ok(found)
    }

    async fn push_file(&self, source: &Path, name: &str) -> Result<()> {
        std::fs::copy(source, self.files_storage()?.join(name))
            .map(|_| ())
            .map_err(device)
    }

    async fn hardware_keyboard(&self, on: bool) -> Result<()> {
        let path = format!("/keyboard?hardware={}", if on { "on" } else { "off" });
        self.ensure_reader().await?;
        if self.reader_get(&path).await.is_err() {
            // A reader from an older build answers pings but has no keyboard switch; a fresh one does.
            self.stop_reader().await;
            self.ensure_reader().await?;
            self.reader_get(&path).await?;
        }
        self.hardware_keyboard
            .store(on, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    async fn press(&self, key: &str) -> Result<()> {
        if key == "Home" {
            return self.hid("b home").await;
        }
        let on_screen = match key {
            "Enter" | "Return" => Some("return"),
            "Backspace" => Some("delete"),
            "Tab" => Some("Tab"),
            _ => None,
        };
        if let (Some(label), Some(keys)) = (on_screen, self.on_screen_keyboard().await?) {
            let (x, y) = key_centre(&keys, label)
                .ok_or_else(|| DriverError::NotFound(format!("the on-screen {label} key")))?;
            return self.tap_at(x, y).await;
        }
        let code = keycode(key).ok_or_else(|| {
            DriverError::Invalid(format!(
                "{key} isn't a key soundcheck knows; use names such as ArrowRight or Enter"
            ))
        })?;
        self.hid(&format!("k {code}")).await
    }

    async fn screen_size(&self) -> Result<(f64, f64)> {
        let tree: Value = serde_json::from_str(&self.screen_json(true).await?).map_err(device)?;
        let root = tree.as_array().and_then(|r| r.first()).unwrap_or(&tree);
        let f = frame(root);
        Ok((f.width, f.height))
    }

    /// The simulator shares the Mac, so its health is the Mac's, plus the app's launch time.
    async fn health(&self) -> Result<Health> {
        let mut health = Health::default();
        let pressure = run("memory_pressure", &["-Q"]).await?;
        if let Some(free) = pressure.lines().find_map(|l| {
            l.strip_prefix("System-wide memory free percentage:")?
                .trim()
                .trim_end_matches('%')
                .parse::<f64>()
                .ok()
        }) {
            health.host(
                format!("Mac memory {free:.0}% free"),
                free < MIN_FREE_MEMORY_PERCENT,
            );
        }
        let load = run("sysctl", &["-n", "vm.loadavg", "hw.ncpu"]).await?;
        // `{ 1m 5m 15m }` then the core count. The one-minute average: the five-minute one still holds a simulator's
        // boot minutes after it has settled.
        let one_minute = load
            .split_whitespace()
            .nth(1)
            .and_then(|n| n.parse::<f64>().ok());
        let cores = load
            .lines()
            .nth(1)
            .and_then(|n| n.trim().parse::<f64>().ok());
        if let (Some(one_minute), Some(cores)) = (one_minute, cores) {
            health.host(
                format!("Mac load {one_minute:.1} on {cores:.0} cores"),
                one_minute > cores * MAX_LOAD_PER_CORE,
            );
        }
        if let Some(ms) = *self
            .last_launch_ms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            health.measure(
                format!("launch {:.1} s", ms as f64 / 1000.0),
                ms > MAX_LAUNCH_MS,
            );
        }
        health
            .facts
            .append(&mut self.restarts.lock().unwrap_or_else(|e| e.into_inner()));
        Ok(health)
    }

    async fn restart(&self) -> Result<()> {
        // A reader from before the reboot answers pings but can't read.
        self.stop_reader().await;
        let _ = run("xcrun", &["simctl", "shutdown", &self.udid]).await;
        run("xcrun", &["simctl", "boot", &self.udid]).await?;
        run("xcrun", &["simctl", "bootstatus", &self.udid, "-b"]).await?;
        // The keyboard setting applies at boot, and is off.
        self.hardware_keyboard
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if self.headed {
            self.show();
        }
        Ok(())
    }

    async fn double_tap(&self, x: f64, y: f64) -> Result<()> {
        self.hid(&double_tap_script(x, y)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_elapsed_times_parse_to_seconds() {
        assert_eq!(parse_etime("42:08"), Some(2528));
        assert_eq!(parse_etime("01:02:03"), Some(3723));
        assert_eq!(parse_etime("2-00:00:05"), Some(172_805));
        assert_eq!(parse_etime("soon"), None);
    }

    #[test]
    fn a_ping_names_the_reader() {
        let udid = "68A7F6A7-4339-4BC4-8A04-072296D06525";
        assert_eq!(
            parse_ping(&format!("ok {udid} 8ccc022d1070 63127 1792710420")),
            Some(Ping {
                udid: udid.into(),
                build: Some("8ccc022d1070".into()),
                pid: Some(63127),
                started: Some(1_792_710_420),
            })
        );
        let old = parse_ping(&format!("ok {udid}")).unwrap();
        assert_eq!((old.udid.as_str(), old.build, old.pid), (udid, None, None));
        assert_eq!(parse_ping("404"), None);
    }

    #[test]
    fn a_reader_is_replaced_when_its_build_or_boot_is_stale() {
        let ping = parse_ping("ok U abc 1 1000").unwrap();
        assert_eq!(stale_reader(&ping, Some("abc"), Some(990)), None);
        assert_eq!(stale_reader(&ping, None, None), None);
        assert!(stale_reader(&ping, Some("def"), Some(990)).is_some());
        assert_eq!(
            stale_reader(&ping, Some("abc"), Some(1010)),
            Some("the simulator had rebooted under it")
        );
        // Within the seconds' rounding of `ps` and the reader's clock.
        assert_eq!(stale_reader(&ping, Some("abc"), Some(1002)), None);
        let old = parse_ping("ok U").unwrap();
        assert!(stale_reader(&old, Some("abc"), None).is_some());
    }

    #[test]
    fn only_a_dropped_channel_counts_as_lost() {
        assert!(lost_channel(&device(
            "reader: HTTP/1.0 500 Error Channel disconnected"
        )));
        assert!(!lost_channel(&device(
            "the reader didn't answer within 15 s"
        )));
    }

    #[test]
    fn each_character_maps_to_its_on_screen_key() {
        let key = key_for;
        assert_eq!(key(' '), Some(("space".into(), false)));
        assert_eq!(key('E'), Some(("E".into(), false)));
        assert_eq!(key('2'), Some(("2".into(), false)));
        assert_eq!(key('!'), Some(("1".into(), true)));
        assert_eq!(key('?'), Some(("/".into(), true)));
        assert_eq!(key('\n'), Some(("return".into(), false)));
        assert_eq!(key('£'), None);
    }

    const TREE: &str = r#"[{"type":"Application","AXLabel":"Solo Scores","frame":{"x":0,"y":0,"width":1032,"height":1376},
      "children":[{"type":"Group","AXLabel":null,"frame":{"x":0,"y":0,"width":1032,"height":1376},"children":[
        {"type":"Button","AXLabel":"Open the app drawer","AXUniqueId":"","enabled":true,"frame":{"x":8,"y":40,"width":48,"height":48}},
        {"type":"StaticText","AXLabel":"Collection deleted","frame":{"x":400,"y":746,"width":231,"height":33}},
        {"type":"StaticText","AXLabel":"Collection deleted","frame":{"x":400,"y":746,"width":231,"height":33}},
        {"type":"Switch","AXLabel":"Dark mode","AXValue":"1","frame":{"x":10,"y":200,"width":50,"height":30}},
        {"type":"Button","AXLabel":"Save","enabled":false,"frame":{"x":10,"y":300,"width":50,"height":30}},
        {"type":"Other","AXUniqueId":"score-row-icon","frame":{"x":10,"y":400,"width":20,"height":20}},
        {"type":"Button","AXLabel":"Below","frame":{"x":10,"y":1500,"width":50,"height":30}}
      ]}]}]"#;

    #[test]
    fn the_default_view_is_what_voiceover_speaks() {
        let seen: Vec<String> = parse(TREE, false)
            .unwrap()
            .iter()
            .map(Element::line)
            .collect();
        assert_eq!(
            seen,
            [
                "Open the app drawer",
                "Collection deleted",
                "Dark mode (checked)",
                "Save (disabled)"
            ]
        );
    }

    #[test]
    fn all_includes_id_only_labels() {
        let seen: Vec<String> = parse(TREE, true)
            .unwrap()
            .iter()
            .map(|e| e.label.clone())
            .collect();
        assert!(seen.contains(&"score-row-icon".to_string()));
    }
}
