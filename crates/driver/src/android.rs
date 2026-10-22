//! The Android driver: an emulator over adb, read through a long-lived reader on the device.

use crate::Platform;
use crate::device::{Device, DriverError, Health, Result, double_tap_script};
use crate::element::{Element, Rect};
use regex::Regex;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout};

const READER_PACKAGE: &str = "dev.soundcheck.reader";
const READER_PORT: u16 = 9018;
/// What a crash or hang looks like in logcat, for `errors`.
const FAILURES: &str = r"FATAL EXCEPTION|ANR in|handleHostException|already has a parent";

static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<node ([^>]*?)/?>").unwrap());
// uiautomator single-quotes a value that holds a double quote.
static ATTR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"([\w-]+)=(?:"([^"]*)"|'([^']*)')"#).unwrap());
static BOUNDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(-?\d+),(-?\d+)\]\[(-?\d+),(-?\d+)\]").unwrap());

pub struct Android {
    adb: PathBuf,
    serial: String,
    package: String,
    reader_apk: Option<PathBuf>,
    /// The `adb reverse` forwards at connect, as (device port, host port): what a reboot drops and health restores.
    forwards_at_connect: Vec<(u16, u16)>,
    /// How long the last launch took, in milliseconds, as `am start -W` reports it.
    last_launch_ms: std::sync::Mutex<Option<u64>>,
    /// Whether an emulator soundcheck boots shows its window.
    headed: bool,
}

/// Health limits. A degraded emulator on 2026-10-20 had 0.4 of 4 GB free, the system busy while idle, ANRs in
/// Google's own apps and a 59 s cold launch (normally about 1 s).
const MIN_FREE_MEMORY: f64 = 0.10;
const MAX_CPU_BUSY: f64 = 0.80;
const SETTLED_CPU_BUSY: f64 = 0.50;
const MAX_LAUNCH_MS: u64 = 15_000;

/// Where the per-spec proxies of earlier versions listened (the host port plus this): a forward left aimed at one is restored.
const PROXY_OFFSET: u16 = 20_000;

fn device(e: impl std::fmt::Display) -> DriverError {
    DriverError::Device(e.to_string())
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#10;", "\n")
        .replace("&#13;", "\r")
        .replace("&amp;", "&")
}

/// Parses `uiautomator dump` XML into elements. The reader's default tree is already what a screen reader is given
/// (Android leaves out views it deems unimportant), so the default view only drops labels that are just a resource or
/// test id (`phosphor-react-native-list-regular`), which a screen reader never speaks. `all` keeps them.
pub fn parse(xml: &str, all: bool) -> Vec<Element> {
    let mut out = Vec::new();
    for node in TOKEN.captures_iter(xml) {
        let Some(body) = node.get(1) else {
            continue;
        };
        let attrs: std::collections::HashMap<&str, String> = ATTR
            .captures_iter(body.as_str())
            .map(|c| {
                (
                    c.get(1).unwrap().as_str(),
                    unescape(c.get(2).or(c.get(3)).unwrap().as_str()),
                )
            })
            .collect();
        let get = |k: &str| attrs.get(k).map(String::as_str).unwrap_or("");
        let flag = |k: &str| get(k) == "true";
        let (desc, text, id) = (get("content-desc"), get("text"), get("resource-id"));
        let spoken = [desc, text].into_iter().find(|s| !s.trim().is_empty());
        let label = match (spoken, all) {
            (Some(s), _) => s,
            (None, true) if !id.is_empty() => id,
            _ => continue,
        };
        let Some(b) = BOUNDS.captures(get("bounds")) else {
            continue;
        };
        let n = |i: usize| b.get(i).unwrap().as_str().parse::<f64>().unwrap_or(0.0);
        let frame = Rect {
            x: n(1),
            y: n(2),
            width: n(3) - n(1),
            height: n(4) - n(2),
        };
        if frame.width <= 0.0 || frame.height <= 0.0 {
            continue;
        }
        out.push(Element {
            label: label.to_string(),
            value: (!text.is_empty() && text != label).then(|| text.to_string()),
            id: (!id.is_empty()).then(|| id.to_string()),
            role: get("class")
                .rsplit('.')
                .next()
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            frame,
            enabled: get("enabled") != "false",
            checked: flag("checkable").then(|| flag("checked")),
            selected: flag("selected").then_some(true),
            expanded: None,
            focused: flag("focused"),
        });
    }
    out
}

/// The app an "ANR in" log line names: ActivityManager writes `ANR in com.app (…)`, WindowManager
/// `ANR in Window{b40aef5 u0 com.app/com.app.Activity}`.
fn anr_app(line: &str) -> Option<String> {
    let rest = line.split("ANR in ").nth(1)?;
    let name = match rest.strip_prefix("Window{") {
        Some(window) => window.split_whitespace().nth(2)?.split('/').next()?,
        None => rest.split_whitespace().next()?,
    };
    Some(name.trim_end_matches(['}', '.', ',']).to_string())
}

/// `input text` takes one word: spaces become %s, and the shell's own characters are escaped.
fn current_size(dumpsys: &str) -> Option<(f64, f64)> {
    static CUR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bcur=(\d+)x(\d+)").unwrap());
    let c = CUR.captures(dumpsys)?;
    Some((c[1].parse().ok()?, c[2].parse().ok()?))
}

fn touchable_region(dumpsys: &str) -> Option<Rect> {
    static REGION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"touchable region=SkRegion\(\((\d+),(\d+),(\d+),(\d+)\)\)").unwrap()
    });
    let c = REGION.captures(dumpsys)?;
    let n = |i: usize| c[i].parse::<f64>().ok();
    Some(Rect {
        x: n(1)?,
        y: n(2)?,
        width: n(3)? - n(1)?,
        height: n(4)? - n(2)?,
    })
}

/// Text for a reader query string, as `URLDecoder` reads it back.
fn url_encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A reader input script for one touch through `points`: down at the first, held `hold_ms`, moving through the rest
/// `step_ms` apart, up at the last.
fn touch_script(points: &[(f64, f64)], hold_ms: u64, step_ms: u64) -> String {
    let mut steps = Vec::new();
    if let Some((x, y)) = points.first() {
        steps.push(format!("d {x:.0} {y:.0}"));
        steps.push(format!("w {hold_ms}"));
    }
    for (x, y) in points.iter().skip(1) {
        steps.push(format!("d {x:.0} {y:.0}"));
        steps.push(format!("w {step_ms}"));
    }
    if let Some((x, y)) = points.last() {
        steps.push(format!("u {x:.0} {y:.0}"));
    }
    steps.join(";")
}

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

fn keycode(key: &str) -> Option<u32> {
    Some(match key {
        "ArrowLeft" => 21,
        "ArrowRight" => 22,
        "ArrowUp" => 19,
        "ArrowDown" => 20,
        "Enter" => 66,
        "Escape" => 111,
        "Back" => 4,
        "Home" => 3,
        "Tab" => 61,
        "Backspace" => 67,
        "Space" | " " => 62,
        "PageUp" => 92,
        "PageDown" => 93,
        _ => return None,
    })
}

impl Android {
    /// Connects to the running emulator (or `ANDROID_SERIAL`, which must be an emulator) to drive `package`.
    pub async fn connect(package: &str, reader_apk: Option<PathBuf>) -> Result<Android> {
        let sdk = std::env::var_os("ANDROID_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                    .join("Library/Android/sdk")
            });
        let adb = sdk.join("platform-tools/adb");
        let serial = match std::env::var("ANDROID_SERIAL") {
            Ok(s) => s,
            Err(_) => {
                let out = run(&adb, &["devices"], Duration::from_secs(10)).await?;
                out.lines()
                    .filter_map(|l| l.strip_suffix("\tdevice"))
                    .find(|s| s.starts_with("emulator-"))
                    .ok_or_else(|| DriverError::Device("no running emulator".into()))?
                    .to_string()
            }
        };
        // A physical device may run a build that points at production.
        if !serial.starts_with("emulator-") {
            return Err(DriverError::Device(format!(
                "{serial} isn't an emulator; soundcheck drives emulators only"
            )));
        }
        let android = Android {
            adb,
            serial,
            package: package.into(),
            reader_apk,
            forwards_at_connect: Vec::new(),
            last_launch_ms: std::sync::Mutex::new(None),
            headed: false,
        };
        // A forward left pointing at a proxy (a driver that stopped mid-drop) would leave the app offline.
        android.restore_forwards().await?;
        let forwards_at_connect = android.forwards().await?;
        android.replace_stale_reader().await?;
        Ok(Android {
            forwards_at_connect,
            ..android
        })
    }

    async fn adb(&self, args: &[&str]) -> Result<String> {
        let mut full = vec!["-s", self.serial.as_str()];
        full.extend_from_slice(args);
        run(&self.adb, &full, Duration::from_secs(60)).await
    }

    /// A reader signed with another key (a build from before the committed keystore) refuses an update, so it's
    /// uninstalled first.
    async fn install_reader(&self, apk: &Path) -> Result<()> {
        let apk = apk.display().to_string();
        match self.adb(&["install", "-r", &apk]).await {
            Err(e) if e.to_string().contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE") => {
                self.adb(&["uninstall", READER_PACKAGE]).await?;
                self.adb(&["install", &apk]).await.map(|_| ())
            }
            other => other.map(|_| ()),
        }
    }

    async fn shell(&self, command: &str) -> Result<String> {
        self.adb(&["shell", command]).await
    }

    /// The share of CPU time spent busy over two seconds, leaving out waiting for the disk.
    async fn cpu_busy(&self) -> Option<f64> {
        let out = self
            .shell("head -1 /proc/stat; sleep 2; head -1 /proc/stat")
            .await
            .ok()?;
        let sample = |line: &str| -> Option<(f64, f64)> {
            let n: Vec<f64> = line
                .split_whitespace()
                .skip(1)
                .filter_map(|v| v.parse().ok())
                .collect();
            // user nice system idle iowait irq softirq steal
            let idle = n.get(3)? + n.get(4)?;
            let total: f64 = n.iter().take(8).sum();
            Some((total - idle, total - n.get(4)?))
        };
        let mut lines = out.lines().filter(|l| l.starts_with("cpu "));
        let (busy1, total1) = sample(lines.next()?)?;
        let (busy2, total2) = sample(lines.next()?)?;
        let total = total2 - total1;
        (total > 0.0).then(|| (busy2 - busy1) / total)
    }

    /// `adb reverse` forwards as (device port, host port).
    async fn forwards(&self) -> Result<Vec<(u16, u16)>> {
        let list = self.adb(&["reverse", "--list"]).await?;
        Ok(list
            .lines()
            .filter_map(|l| {
                let mut parts = l.split_whitespace().skip(1);
                let device = parts.next()?.strip_prefix("tcp:")?.parse().ok()?;
                let host = parts.next()?.strip_prefix("tcp:")?.parse().ok()?;
                Some((device, host))
            })
            .collect())
    }

    /// Points any forward still aimed at a proxy back at its own host port.
    async fn restore_forwards(&self) -> Result<()> {
        for (device, host) in self.forwards().await? {
            if host >= PROXY_OFFSET && host - PROXY_OFFSET == device {
                self.adb(&[
                    "reverse",
                    &format!("tcp:{device}"),
                    &format!("tcp:{device}"),
                ])
                .await?;
            }
        }
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
                "reader: {}",
                head.lines().next().unwrap_or("")
            )));
        }
        Ok(body.to_string())
    }

    /// Runs a touch and key script through the reader's automation connection: one injected gesture with the
    /// device keeping the timing, where `adb shell input` costs a process start per action.
    async fn input(&self, script: &str) -> Result<()> {
        self.reader_call(&format!("/input?{}", url_encode(script)))
            .await
    }

    async fn reader_call(&self, path: &str) -> Result<()> {
        self.ensure_reader().await?;
        if self.reader_get(path).await.is_ok() {
            return Ok(());
        }
        // A reader installed from an older build has no input endpoints: install this one and start it afresh.
        let _ = self.shell(&format!("am force-stop {READER_PACKAGE}")).await;
        if let Some(apk) = &self.reader_apk {
            self.install_reader(apk).await?;
        }
        self.ensure_reader().await?;
        self.reader_get(path).await.map(|_| ())
    }

    /// A reader from another soundcheck build keeps answering with its own tree, so replace it unless it's this APK.
    async fn replace_stale_reader(&self) -> Result<()> {
        let Some(apk) = &self.reader_apk else {
            return Ok(());
        };
        let installed = self.shell(&format!("pm path {READER_PACKAGE}")).await?;
        let Some(path) = installed.trim().strip_prefix("package:") else {
            return Ok(());
        };
        let hash = |out: String| out.split_whitespace().next().map(str::to_string);
        let on_device = hash(self.shell(&format!("sha256sum {path}")).await?);
        let apk_path = apk.display().to_string();
        let built = hash(
            run(
                Path::new("/usr/bin/shasum"),
                &["-a", "256", &apk_path],
                Duration::from_secs(10),
            )
            .await?,
        );
        if on_device.is_some() && on_device == built {
            return Ok(());
        }
        let _ = self.shell(&format!("am force-stop {READER_PACKAGE}")).await;
        self.install_reader(apk).await
    }

    /// Starts the reader if it isn't answering. Only one UI automation client can connect at a time.
    async fn ensure_reader(&self) -> Result<()> {
        let port = format!("tcp:{READER_PORT}");
        self.adb(&["forward", &port, &port]).await?;
        if self
            .reader_get("/ping")
            .await
            .is_ok_and(|b| b == "ok soundcheck")
        {
            return Ok(());
        }
        let Some(apk) = &self.reader_apk else {
            return Err(device("no reader APK built (readers/android/build.sh)"));
        };
        let installed = self
            .shell(&format!("pm list packages {READER_PACKAGE}"))
            .await?;
        if !installed.contains(READER_PACKAGE) {
            self.install_reader(apk).await?;
        }
        // `-w` keeps `am` alive, and the automation connection lives in it; setsid detaches it from this shell.
        self.shell(&format!(
            "setsid nohup am instrument -w {READER_PACKAGE}/.Reader </dev/null >/dev/null 2>&1 &"
        ))
        .await?;
        for _ in 0..30 {
            if self
                .reader_get("/ping")
                .await
                .is_ok_and(|b| b == "ok soundcheck")
            {
                return Ok(());
            }
            sleep(Duration::from_millis(200)).await;
        }
        let others = self
            .shell("ps -A -o ARGS | grep '[a]m instrument'")
            .await
            .unwrap_or_default();
        Err(device(format!(
            "the reader didn't start; another UI automation client may hold the screen{}",
            if others.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", others.trim())
            }
        )))
    }

    async fn uiautomator_dump(&self, compressed: bool) -> Result<String> {
        let _ = self.shell(&format!("am force-stop {READER_PACKAGE}")).await;
        let file = "/sdcard/soundcheck-ui.xml";
        self.shell(&format!("rm -f {file}")).await?;
        let flag = if compressed { " --compressed" } else { "" };
        let out = self
            .shell(&format!("uiautomator dump{flag} {file}"))
            .await?;
        if out.contains("ERROR") {
            return Err(device(out.trim()));
        }
        self.shell(&format!("cat {file}")).await
    }

    async fn screen_xml(&self, all: bool) -> Result<String> {
        if std::env::var("SOUNDCHECK_READER").as_deref() != Ok("off") {
            // An empty tree is a failed read too (no active window yet): fall back, and restart the reader next time.
            if self.ensure_reader().await.is_ok()
                && let Ok(xml) = self
                    .reader_get(if all { "/dump?all" } else { "/dump" })
                    .await
                && xml.contains("<node")
            {
                return Ok(xml);
            }
            let _ = self.shell(&format!("am force-stop {READER_PACKAGE}")).await;
        }
        self.uiautomator_dump(false).await
    }

    /// The screen as `uiautomator dump` reads it, for parity checks against the reader. `compressed` is the tree a
    /// screen reader is given, which the reader's default view matches.
    pub async fn read_uiautomator(&self, compressed: bool) -> Result<Vec<Element>> {
        Ok(parse(
            &self.uiautomator_dump(compressed).await?,
            !compressed,
        ))
    }

    /// Where the on-screen keyboard takes touches, if it's up: the reader leaves out what it covers, uiautomator doesn't.
    pub async fn keyboard_region(&self) -> Result<Option<Rect>> {
        if !self.keyboard_shown().await? {
            return Ok(None);
        }
        Ok(touchable_region(
            &self.shell("dumpsys window InputMethod").await?,
        ))
    }

    /// Whether an emulator this driver boots (a restart) shows its window; headless by default.
    pub fn headed(self, headed: bool) -> Android {
        Android { headed, ..self }
    }

    /// Forwards each port to the same port on this machine, where the network proxy listens; health keeps them.
    pub async fn forward(mut self, ports: &[u16]) -> Result<Android> {
        for &port in ports {
            self.adb(&["reverse", &format!("tcp:{port}"), &format!("tcp:{port}")])
                .await?;
            self.forwards_at_connect
                .retain(|&(device, _)| device != port);
            self.forwards_at_connect.push((port, port));
        }
        Ok(self)
    }

    /// The emulator, for things outside the app (its serial and adb).
    pub fn serial(&self) -> &str {
        &self.serial
    }
}

async fn run(program: &Path, args: &[&str], limit: Duration) -> Result<String> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .kill_on_drop(true)
        .output();
    let out = timeout(limit, child)
        .await
        .map_err(|_| {
            device(format!(
                "adb {} took longer than {} s",
                args.join(" "),
                limit.as_secs()
            ))
        })?
        .map_err(device)?;
    if !out.status.success() {
        return Err(device(format!(
            "adb {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

impl Device for Android {
    fn platform(&self) -> Platform {
        Platform::Android
    }

    async fn read(&self, all: bool) -> Result<Vec<Element>> {
        Ok(parse(&self.screen_xml(all).await?, all))
    }

    async fn tap_at(&self, x: f64, y: f64) -> Result<()> {
        self.hold_at(x, y, 80).await
    }

    async fn hold_at(&self, x: f64, y: f64, ms: u64) -> Result<()> {
        self.input(&touch_script(&[(x, y)], ms, 0)).await
    }

    async fn drag(&self, from: (f64, f64), to: (f64, f64), hold_ms: u64) -> Result<()> {
        let mut points = vec![from];
        points.extend(path(from, to));
        // A pause at the end, so the drop isn't read as a fling.
        let mut script = touch_script(&points, hold_ms, 16);
        if let Some(up) = script.rfind(";u ") {
            script.insert_str(up, ";w 300");
        }
        self.input(&script).await
    }

    async fn swipe(&self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        let mut points = vec![from];
        points.extend(path(from, to));
        let step = ms / points.len() as u64;
        self.input(&touch_script(&points, step, step)).await
    }

    async fn type_text(&self, text: &str) -> Result<()> {
        self.reader_call(&format!("/type?{}", url_encode(text)))
            .await?;
        sleep(Duration::from_millis(300)).await;
        Ok(())
    }

    async fn clear(&self, _chars: usize) -> Result<()> {
        // Select all (ctrl+A), then delete.
        self.input("kd 113;k 29;ku 113;k 67").await
    }

    async fn launch(&self) -> Result<()> {
        self.shell(&format!("am force-stop {}", self.package))
            .await?;
        let resolved = self
            .shell(&format!(
                "cmd package resolve-activity --brief -c android.intent.category.LAUNCHER {}",
                self.package
            ))
            .await?;
        let component = resolved
            .lines()
            .rev()
            .find(|l| l.contains('/'))
            .ok_or_else(|| {
                device(format!(
                    "{} has no launcher activity (is it installed?)",
                    self.package
                ))
            })?
            .trim()
            .to_string();
        self.adb(&["logcat", "-c"]).await?;
        // -W waits until the activity has drawn and reports how long that took.
        // A first launch after a reboot can take a minute: that's a slow launch for health to record, not a failure.
        let limit = Duration::from_secs(120);
        let command = format!("am start -W -n {component}");
        let started = run(&self.adb, &["-s", &self.serial, "shell", &command], limit)
            .await
            .unwrap_or_else(|_| format!("TotalTime: {}", limit.as_millis()));
        let total = started.lines().find_map(|l| {
            l.trim()
                .strip_prefix("TotalTime:")?
                .trim()
                .parse::<u64>()
                .ok()
        });
        *self
            .last_launch_ms
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = total;
        sleep(Duration::from_secs(1)).await;
        Ok(())
    }

    async fn screenshot(&self, path: &Path) -> Result<()> {
        // The emulator captures from the host side, so screens marked secure (a score viewer) still show.
        let absolute = std::path::absolute(path).map_err(device)?;
        self.adb(&[
            "emu",
            "screenrecord",
            "screenshot",
            &absolute.display().to_string(),
        ])
        .await
        .map(|_| ())
    }

    async fn errors(&self) -> Result<Vec<String>> {
        let log = self.adb(&["logcat", "-d", "-v", "brief"]).await?;
        let failures = Regex::new(FAILURES).unwrap();
        Ok(log
            .lines()
            .filter(|l| failures.is_match(l))
            .map(str::to_string)
            .collect())
    }

    async fn keyboard_shown(&self) -> Result<bool> {
        Ok(self
            .shell("dumpsys input_method")
            .await?
            .contains("mInputShown=true"))
    }

    async fn push_file(&self, source: &Path, name: &str) -> Result<()> {
        self.adb(&[
            "push",
            &source.display().to_string(),
            &format!("/sdcard/Download/{name}"),
        ])
        .await
        .map(|_| ())
    }

    async fn press(&self, key: &str) -> Result<()> {
        let code = keycode(key).ok_or_else(|| {
            DriverError::Invalid(format!(
                "{key} isn't a key soundcheck knows; use names such as ArrowRight or Enter"
            ))
        })?;
        self.input(&format!("k {code}")).await
    }

    // The reader keeps the touch held between calls: a second `d` moves it.
    async fn touch_down(&self, x: f64, y: f64) -> Result<()> {
        self.input(&format!("d {x:.0} {y:.0}")).await
    }

    async fn touch_move(&self, x: f64, y: f64) -> Result<()> {
        self.input(&format!("d {x:.0} {y:.0}")).await
    }

    async fn touch_up(&self, x: f64, y: f64) -> Result<()> {
        self.input(&format!("u {x:.0} {y:.0}")).await
    }

    async fn screen_size(&self) -> Result<(f64, f64)> {
        // `wm size` is the panel's natural size, landscape on a tablet even while it shows portrait; the display's
        // `cur=` size is what's on screen now, with rotation and any override applied.
        let out = self.shell("dumpsys window displays").await?;
        current_size(&out).ok_or_else(|| device("dumpsys window displays has no cur= size"))
    }

    async fn density(&self) -> f64 {
        let out = self.shell("wm density").await.unwrap_or_default();
        out.lines()
            .rev()
            .find_map(|l| l.rsplit(':').next()?.trim().parse::<f64>().ok())
            .map_or(1.0, |dpi| dpi / 160.0)
    }

    async fn double_tap(&self, x: f64, y: f64) -> Result<()> {
        // One script, so the device keeps the timing.
        self.input(&double_tap_script(x, y)).await
    }

    async fn health(&self) -> Result<Health> {
        let mut health = Health::default();
        // A reboot drops the forwards, and the app then silently shows stale data: put them back at once.
        let present: Vec<u16> = self.forwards().await?.into_iter().map(|(d, _)| d).collect();
        for (device_port, host) in &self.forwards_at_connect {
            if !present.contains(device_port) {
                self.adb(&[
                    "reverse",
                    &format!("tcp:{device_port}"),
                    &format!("tcp:{host}"),
                ])
                .await?;
                health
                    .facts
                    .push(format!("restored the lost forward for port {device_port}"));
            }
        }
        let meminfo = self.shell("cat /proc/meminfo").await?;
        let kb = |key: &str| {
            meminfo.lines().find_map(|l| {
                l.strip_prefix(key)?
                    .split_whitespace()
                    .next()?
                    .parse::<f64>()
                    .ok()
            })
        };
        if let (Some(total), Some(available)) = (kb("MemTotal:"), kb("MemAvailable:")) {
            let free = available / total;
            health.measure(
                format!("memory {:.0}% free", free * 100.0),
                free < MIN_FREE_MEMORY,
            );
        }
        // Busy CPU, not the load average: post-boot disk work keeps the load near 20 with the CPU idle.
        if let Some(busy) = self.cpu_busy().await {
            health.measure(
                format!("CPU {:.0}% busy", busy * 100.0),
                busy > MAX_CPU_BUSY,
            );
        }
        // The log is cleared at each launch, so these are since this spec's launch.
        let log = self.adb(&["logcat", "-d", "-v", "brief"]).await?;
        let mut seen = Vec::new();
        for app in log.lines().filter_map(anr_app) {
            if app != self.package && !seen.contains(&app) {
                health.measure(format!("ANR in another app: {app}"), true);
                seen.push(app);
            }
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
        let avd = self.adb(&["emu", "avd", "name"]).await?;
        let avd = avd.lines().next().unwrap_or("").trim().to_string();
        if avd.is_empty() {
            return Err(device("the emulator didn't say which AVD it runs"));
        }
        let _ = self.adb(&["emu", "kill"]).await;
        for _ in 0..60 {
            let devices = run(&self.adb, &["devices"], Duration::from_secs(10)).await?;
            if !devices.contains(&self.serial) {
                break;
            }
            sleep(Duration::from_secs(1)).await;
        }
        let emulator = self
            .adb
            .parent()
            .and_then(Path::parent)
            .map(|sdk| sdk.join("emulator/emulator"))
            .ok_or_else(|| device("no emulator binary beside adb"))?;
        let port = self.serial.strip_prefix("emulator-").unwrap_or("5554");
        // Its own process group, so it outlives this server; SwiftShader drops taps, so the host GPU.
        std::process::Command::new(emulator)
            .args(["-avd", &avd, "-port", port, "-gpu", "host", "-no-snapshot"])
            .args(if self.headed {
                None
            } else {
                Some("-no-window")
            })
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(device)?;
        let mut booted = false;
        for _ in 0..180 {
            if self
                .shell("getprop sys.boot_completed")
                .await
                .is_ok_and(|v| v.trim() == "1")
            {
                booted = true;
                break;
            }
            sleep(Duration::from_secs(1)).await;
        }
        if !booted {
            return Err(device(format!(
                "{avd} didn't finish booting within 3 minutes"
            )));
        }
        // A fresh boot keeps the system busy for minutes: let it settle first.
        for _ in 0..60 {
            if self
                .cpu_busy()
                .await
                .is_some_and(|busy| busy < SETTLED_CPU_BUSY)
            {
                break;
            }
            sleep(Duration::from_secs(1)).await;
        }
        // Wake and dismiss a keyguard without a PIN; a PIN needs a person.
        let _ = self.shell("input keyevent 224; wm dismiss-keyguard").await;
        for (device_port, host) in &self.forwards_at_connect {
            self.adb(&[
                "reverse",
                &format!("tcp:{device_port}"),
                &format!("tcp:{host}"),
            ])
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0"><node index="0" text="" resource-id="" class="android.widget.FrameLayout" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focused="false" selected="false" bounds="[0,0][1600,2560]"><node index="1" text="" resource-id="com.example:id/switch" class="android.widget.Switch" content-desc="Send diagnostic info" checkable="true" checked="true" clickable="true" enabled="true" focused="false" selected="false" bounds="[400,10][500,40]" /><node index="2" text="Opener" resource-id="" class="android.widget.EditText" content-desc="Title" checkable="false" checked="false" clickable="true" enabled="false" focused="true" selected="false" bounds="[0,100][800,160]" /><node index="3" text="" resource-id="phosphor-react-native-list-regular" class="android.view.View" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focused="false" selected="false" bounds="[0,200][80,260]" /><node index="4" text="Off screen" resource-id="" class="android.widget.TextView" content-desc="" bounds="[0,0][0,0]" /></node></hierarchy>"#;

    #[test]
    fn the_default_view_is_what_a_screen_reader_speaks() {
        let seen = parse(XML, false);
        let labels: Vec<_> = seen.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Send diagnostic info", "Title"],
            "id-only and off-screen nodes are left out"
        );
        assert_eq!(seen[0].checked, Some(true));
        assert_eq!(seen[0].role.as_deref(), Some("Switch"));
        assert_eq!(seen[1].value.as_deref(), Some("Opener"));
        assert!(!seen[1].enabled && seen[1].focused);
        assert_eq!(seen[1].tap_point(), (400.0, 130.0));
    }

    #[test]
    fn a_single_quoted_value_keeps_its_double_quotes() {
        let xml = r#"<node index="0" text='Delete "Strings"?' resource-id="android:id/message" content-desc="" bounds="[0,0][10,10]" />"#;
        assert_eq!(parse(xml, false)[0].label, r#"Delete "Strings"?"#);
    }

    #[test]
    fn all_includes_id_only_labels() {
        let labels: Vec<_> = parse(XML, true).into_iter().map(|e| e.label).collect();
        assert!(labels.contains(&"phosphor-react-native-list-regular".to_string()));
    }

    #[test]
    fn an_anr_names_its_app_in_either_log_form() {
        assert_eq!(
            anr_app(
                "I/WindowManager(566): ANR in Window{b40aef5 u0 com.android.settings/com.android.settings.Settings}. Reason:x"
            ),
            Some("com.android.settings".into())
        );
        assert_eq!(
            anr_app(
                "E/ActivityManager(566): ANR in com.google.android.calendar (com.google.android.calendar/.Main)"
            ),
            Some("com.google.android.calendar".into())
        );
    }

    #[test]
    fn typed_text_survives_the_query_string() {
        assert_eq!(url_encode("E2E Seed Renamed"), "E2E%20Seed%20Renamed");
        assert_eq!(url_encode("Bach's (1)+é"), "Bach%27s%20%281%29%2B%C3%A9");
    }

    #[test]
    fn the_screen_size_follows_rotation() {
        let out = "  mDisplayId=0\n    init=2560x1600 320dpi base=2560x1600 320dpi cur=1600x2560 app=1600x2464 rng=1600x1504-2560x2464";
        assert_eq!(current_size(out), Some((1600.0, 2560.0)));
    }

    #[test]
    fn the_keyboard_region_is_its_touchable_region() {
        let out = "    mTouchableInsets=3 mGivenInsetsPending=false\n    touchable region=SkRegion((0,1824,1600,2560))";
        let region = touchable_region(out).unwrap();
        assert_eq!((region.y, region.height), (1824.0, 736.0));
    }

    #[test]
    fn a_drag_holds_then_moves() {
        assert_eq!(
            touch_script(&[(0.0, 0.0), (10.0, 0.0)], 500, 16),
            "d 0 0;w 500;d 10 0;w 16;u 10 0"
        );
    }
}
