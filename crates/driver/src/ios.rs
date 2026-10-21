//! The iOS simulator: reads through the reader (`readers/ios`, one long-lived accessibility session serving
//! `axe describe-ui` JSON), with `axe describe-ui` as the fallback; input through AXe. Points everywhere but
//! screenshots, which are in pixels.

use crate::Platform;
use crate::device::{Device, DriverError, Health, Result};
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
fn has_content(json: &str) -> bool {
    parse(json, true).is_ok_and(|e| !e.is_empty())
}

/// HID keyboard usage codes, which `axe key` takes.
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
}

/// Health limits for the Mac that runs the simulator, and the app's launch.
const MIN_FREE_MEMORY_PERCENT: f64 = 10.0;
const MAX_LOAD_PER_CORE: f64 = 2.0;
const MAX_LAUNCH_MS: u64 = 20_000;

impl Ios {
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
        };
        ios.no_hardware_keyboard().await?;
        run("xcrun", &["simctl", "bootstatus", &ios.udid, "-b"]).await?;
        Ok(ios)
    }

    /// With AXe's keyboard attached, iOS counts a hardware keyboard and keeps the on-screen one out of sight; most
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

    async fn axe(&self, args: &[&str]) -> Result<String> {
        let mut full = args.to_vec();
        full.extend(["--udid", self.udid.as_str()]);
        run("axe", &full).await
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
                "reader: {}",
                head.lines().next().unwrap_or("")
            )));
        }
        Ok(body.to_string())
    }

    async fn reader_alive(&self) -> bool {
        self.reader_get("/ping")
            .await
            .is_ok_and(|b| b == format!("ok {}", self.udid))
    }

    fn stop_reader(&self) {
        if let Some(binary) = &self.reader {
            let _ = std::process::Command::new("pkill")
                .args(["-f", &binary.display().to_string()])
                .status();
        }
    }

    /// Starts the reader if it isn't answering for this simulator; one attached to another simulator is no use.
    async fn ensure_reader(&self) -> Result<()> {
        if self.reader_alive().await {
            return Ok(());
        }
        let Some(binary) = &self.reader else {
            return Err(device("no iOS reader built (readers/ios/build.sh)"));
        };
        self.stop_reader();
        std::process::Command::new(binary)
            .args(["--udid", &self.udid, "--port", &READER_PORT.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(device)?;
        for _ in 0..40 {
            if self.reader_alive().await {
                return Ok(());
            }
            sleep(Duration::from_millis(250)).await;
        }
        Err(device("the iOS reader didn't start"))
    }

    async fn screen_json(&self, all: bool) -> Result<String> {
        if std::env::var("SOUNDCHECK_READER").as_deref() != Ok("off") && self.reader.is_some() {
            if self.ensure_reader().await.is_ok() {
                for _ in 0..3 {
                    match self
                        .reader_get(if all { "/describe" } else { "/describe?lean" })
                        .await
                    {
                        Ok(json) if has_content(&json) => return Ok(json),
                        Ok(_) => sleep(Duration::from_millis(300)).await,
                        Err(_) => break,
                    }
                }
            }
            // A reader from before a simulator reboot answers pings but can't read; the next read starts a fresh one.
            self.stop_reader();
        }
        let mut last = device("not read");
        for _ in 0..5 {
            match self.axe(&["describe-ui"]).await {
                Ok(json) if has_content(&json) => return Ok(json),
                Ok(json) => last = device(format!("an empty screen: {}", json.trim())),
                Err(e) => last = e,
            }
            sleep(Duration::from_millis(500)).await;
        }
        Err(last)
    }

    /// The screen as `axe describe-ui` reads it, for parity checks against the reader.
    pub async fn read_axe(&self) -> Result<Vec<Element>> {
        parse(&self.axe(&["describe-ui"]).await?, true)
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

    pub fn udid(&self) -> &str {
        &self.udid
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// Under heavy load AXe's session can stall for minutes, so every call has a limit.
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
fn path(from: (f64, f64), to: (f64, f64)) -> Vec<(f64, f64)> {
    let steps = (((to.0 - from.0).abs().max((to.1 - from.1).abs())) / 10.0)
        .ceil()
        .max(1.0) as u32;
    (1..=steps)
        .map(|i| {
            let t = f64::from(i) / f64::from(steps);
            (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
        })
        .collect()
}

impl Device for Ios {
    fn platform(&self) -> Platform {
        Platform::Ios
    }

    async fn read(&self, all: bool) -> Result<Vec<Element>> {
        parse(&self.screen_json(all).await?, all)
    }

    async fn tap_at(&self, x: f64, y: f64) -> Result<()> {
        // Both `axe tap` styles drop taps (one missed 10 of 20); a touch held for 80 ms missed none.
        self.hold_at(x, y, 80).await
    }

    async fn hold_at(&self, x: f64, y: f64, ms: u64) -> Result<()> {
        let (x, y, delay) = (
            format!("{x:.0}"),
            format!("{y:.0}"),
            format!("{}", ms as f64 / 1000.0),
        );
        self.axe(&[
            "touch", "-x", &x, "-y", &y, "--down", "--up", "--delay", &delay,
        ])
        .await
        .map(|_| ())
    }

    async fn drag(&self, from: (f64, f64), to: (f64, f64), hold_ms: u64) -> Result<()> {
        // `axe drag` moves at once, so a long-press drag never starts; another `--down` moves the held touch.
        let mut steps = vec![
            format!("touch -x {:.0} -y {:.0} --down", from.0, from.1),
            format!("sleep {}", hold_ms as f64 / 1000.0),
        ];
        for (x, y) in path(from, to) {
            steps.push(format!("touch -x {x:.0} -y {y:.0} --down"));
            steps.push("sleep 0.03".into());
        }
        steps.push("sleep 0.3".into());
        steps.push(format!("touch -x {:.0} -y {:.0} --up", to.0, to.1));
        let mut args = vec!["batch"];
        for s in &steps {
            args.extend(["--step", s.as_str()]);
        }
        self.axe(&args).await.map(|_| ())
    }

    async fn swipe(&self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        let n = |v: f64| format!("{v:.0}");
        let (x1, y1, x2, y2, d) = (
            n(from.0),
            n(from.1),
            n(to.0),
            n(to.1),
            format!("{}", ms as f64 / 1000.0),
        );
        self.axe(&[
            "swipe",
            "--start-x",
            &x1,
            "--start-y",
            &y1,
            "--end-x",
            &x2,
            "--end-y",
            &y2,
            "--duration",
            &d,
        ])
        .await
        .map(|_| ())
    }

    async fn type_text(&self, text: &str) -> Result<()> {
        self.axe(&["type", text]).await?;
        sleep(Duration::from_millis(300)).await;
        Ok(())
    }

    async fn clear(&self, chars: usize) -> Result<()> {
        // Select-all is ignored with the hardware keyboard off and a tap leaves the cursor mid-text, so delete
        // forward and back from wherever it is.
        let keys: Vec<String> = std::iter::repeat_n("76", chars)
            .chain(std::iter::repeat_n("42", chars))
            .map(str::to_string)
            .collect();
        // AXe takes at most 100 keys per sequence.
        for chunk in keys.chunks(100) {
            self.axe(&[
                "key-sequence",
                "--keycodes",
                &chunk.join(","),
                "--delay",
                "0.02",
            ])
            .await?;
        }
        Ok(())
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
            let mut json = String::new();
            while started.elapsed() < Duration::from_secs(60) {
                json = self.screen_json(true).await.unwrap_or_default();
                if has_content(&json) {
                    break;
                }
                sleep(Duration::from_millis(300)).await;
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

    async fn press(&self, key: &str) -> Result<()> {
        let code = keycode(key).ok_or_else(|| {
            DriverError::Invalid(format!(
                "{key} isn't a key soundcheck knows; use names such as ArrowRight or Enter"
            ))
        })?;
        self.axe(&["key", &code.to_string()]).await.map(|_| ())
    }

    // Separate `axe touch` calls keep one touch held between them; another `--down` moves it.
    async fn touch_down(&self, x: f64, y: f64) -> Result<()> {
        let (x, y) = (format!("{x:.0}"), format!("{y:.0}"));
        self.axe(&["touch", "-x", &x, "-y", &y, "--down"])
            .await
            .map(|_| ())
    }

    async fn touch_move(&self, x: f64, y: f64) -> Result<()> {
        self.touch_down(x, y).await
    }

    async fn touch_up(&self, x: f64, y: f64) -> Result<()> {
        let (x, y) = (format!("{x:.0}"), format!("{y:.0}"));
        self.axe(&["touch", "-x", &x, "-y", &y, "--up"])
            .await
            .map(|_| ())
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
        Ok(health)
    }

    async fn restart(&self) -> Result<()> {
        // A reader from before the reboot answers pings but can't read.
        self.stop_reader();
        let _ = run("xcrun", &["simctl", "shutdown", &self.udid]).await;
        run("xcrun", &["simctl", "boot", &self.udid]).await?;
        run("xcrun", &["simctl", "bootstatus", &self.udid, "-b"]).await?;
        Ok(())
    }

    async fn tap_burst(&self, x: f64, y: f64, count: u32, interval_ms: u64) -> Result<()> {
        // One AXe process, so the gaps are timed there rather than across process start-ups.
        let tap = format!("touch -x {x:.0} -y {y:.0} --down --up --delay 0.08");
        let gap = format!("sleep {}", interval_ms as f64 / 1000.0);
        let mut args = vec!["batch"];
        for i in 0..count {
            if i > 0 {
                args.extend(["--step", gap.as_str()]);
            }
            args.extend(["--step", tap.as_str()]);
        }
        self.axe(&args).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_drag_path_ends_where_it_was_aimed() {
        let p = path((0.0, 0.0), (25.0, 5.0));
        assert_eq!(p.len(), 3);
        assert_eq!(p.last(), Some(&(25.0, 5.0)));
    }
}
