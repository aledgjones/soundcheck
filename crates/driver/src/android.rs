//! The Android driver: an emulator over adb, read through a long-lived reader on the device.

use crate::Platform;
use crate::device::{Device, DriverError, Health, Result};
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
static ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"([\w-]+)="([^"]*)""#).unwrap());
static BOUNDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(-?\d+),(-?\d+)\]\[(-?\d+),(-?\d+)\]").unwrap());

pub struct Android {
    adb: PathBuf,
    serial: String,
    package: String,
    reader_apk: Option<PathBuf>,
    /// The freezing proxies in the `adb reverse` path while a spec may drop the network.
    proxies: tokio::sync::Mutex<Option<Proxies>>,
    /// The `adb reverse` forwards at connect, as (device port, host port): what a reboot drops and health restores.
    forwards_at_connect: Vec<(u16, u16)>,
    /// How long the last launch took, in milliseconds, as `am start -W` reports it.
    last_launch_ms: std::sync::Mutex<Option<u64>>,
}

/// Health limits. A degraded emulator on 2026-10-20 had 0.4 of 4 GB free, the system busy while idle, ANRs in
/// Google's own apps and a 59 s cold launch (normally about 1 s).
const MIN_FREE_MEMORY: f64 = 0.10;
const MAX_CPU_BUSY: f64 = 0.80;
const SETTLED_CPU_BUSY: f64 = 0.50;
const MAX_LAUNCH_MS: u64 = 15_000;

/// A proxy listens on the forwarded host port plus this, so a forward left pointing at one can always be restored.
const PROXY_OFFSET: u16 = 20_000;

/// TCP proxies for `adb reverse` forwards that can freeze the network the way a phone loses it in the background:
/// no bytes pass and nothing closes, so the app never learns the connection is dead. Frozen sockets stay frozen.
struct Proxies {
    /// (device port, host port)
    forwards: Vec<(u16, u16)>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    live: std::sync::Arc<std::sync::Mutex<Vec<std::sync::Arc<std::sync::atomic::AtomicBool>>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Copies one direction of a proxied connection until it closes, passing nothing while frozen.
async fn pipe(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    frozen: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match from.read(&mut buf).await {
            Ok(0) | Err(_) => {
                // A frozen connection never closes, so the far side doesn't learn either.
                if frozen.load(Ordering::SeqCst) {
                    std::future::pending::<()>().await;
                }
                let _ = to.shutdown().await;
                return;
            }
            Ok(n) => {
                if !frozen.load(Ordering::SeqCst) && to.write_all(&buf[..n]).await.is_err() {
                    return;
                }
            }
        }
    }
}

impl Proxies {
    async fn start(forwards: Vec<(u16, u16)>) -> std::io::Result<Proxies> {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let dropped = Arc::new(AtomicBool::new(false));
        let live: Arc<std::sync::Mutex<Vec<Arc<AtomicBool>>>> = Arc::default();
        let mut tasks = Vec::new();
        for &(_, host) in &forwards {
            let listener =
                tokio::net::TcpListener::bind(("127.0.0.1", host + PROXY_OFFSET)).await?;
            let (dropped, live) = (dropped.clone(), live.clone());
            tasks.push(tokio::spawn(async move {
                while let Ok((client, _)) = listener.accept().await {
                    let frozen = Arc::new(AtomicBool::new(dropped.load(Ordering::SeqCst)));
                    live.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(frozen.clone());
                    tokio::spawn(async move {
                        let Ok(upstream) =
                            tokio::net::TcpStream::connect(("127.0.0.1", host)).await
                        else {
                            return;
                        };
                        let (cr, cw) = client.into_split();
                        let (ur, uw) = upstream.into_split();
                        let up = pipe(cr, uw, frozen.clone());
                        let down = pipe(ur, cw, frozen);
                        tokio::join!(up, down);
                    });
                }
            }));
        }
        Ok(Proxies {
            forwards,
            dropped,
            live,
            tasks,
        })
    }

    fn drop_network(&self) {
        use std::sync::atomic::Ordering;
        self.dropped.store(true, Ordering::SeqCst);
        for conn in self.live.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            conn.store(true, Ordering::SeqCst);
        }
    }

    fn restore_network(&self) {
        self.dropped
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for Proxies {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

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
                    unescape(c.get(2).unwrap().as_str()),
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
fn input_text(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            ' ' => out.push_str("%s"),
            '\'' | '"' | '\\' | '&' | '|' | ';' | '<' | '>' | '(' | ')' | '$' | '`' | '*' | '?'
            | '[' | ']' | '#' | '~' | '!' | '{' | '}' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
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
            proxies: tokio::sync::Mutex::new(None),
            forwards_at_connect: Vec::new(),
            last_launch_ms: std::sync::Mutex::new(None),
        };
        // A forward left pointing at a proxy (a driver that stopped mid-drop) would leave the app offline.
        android.restore_forwards().await?;
        let forwards_at_connect = android.forwards().await?;
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
            self.adb(&["install", "-r", &apk.display().to_string()])
                .await?;
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

    async fn uiautomator_dump(&self) -> Result<String> {
        let _ = self.shell(&format!("am force-stop {READER_PACKAGE}")).await;
        let file = "/sdcard/soundcheck-ui.xml";
        self.shell(&format!("rm -f {file}")).await?;
        let out = self.shell(&format!("uiautomator dump {file}")).await?;
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
        self.uiautomator_dump().await
    }

    /// The screen as `uiautomator dump` reads it, for parity checks against the reader.
    pub async fn read_uiautomator(&self) -> Result<Vec<Element>> {
        Ok(parse(&self.uiautomator_dump().await?, true))
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
        self.shell(&format!("input tap {x:.0} {y:.0}"))
            .await
            .map(|_| ())
    }

    async fn hold_at(&self, x: f64, y: f64, ms: u64) -> Result<()> {
        self.shell(&format!(
            "input motionevent DOWN {x:.0} {y:.0}; sleep {}; input motionevent UP {x:.0} {y:.0}",
            ms as f64 / 1000.0
        ))
        .await
        .map(|_| ())
    }

    async fn drag(&self, from: (f64, f64), to: (f64, f64), hold_ms: u64) -> Result<()> {
        // One shell call, so adb round trips don't break up the gesture; `input swipe` never holds first.
        let steps = (((to.0 - from.0).abs().max((to.1 - from.1).abs())) / 10.0)
            .ceil()
            .max(1.0) as u32;
        let moves: Vec<String> = (1..=steps)
            .map(|i| {
                let t = f64::from(i) / f64::from(steps);
                format!(
                    "input motionevent MOVE {:.0} {:.0}",
                    from.0 + (to.0 - from.0) * t,
                    from.1 + (to.1 - from.1) * t
                )
            })
            .collect();
        self.shell(&format!(
            "input motionevent DOWN {:.0} {:.0}; sleep {}; {}; sleep 0.3; input motionevent UP {:.0} {:.0}",
            from.0,
            from.1,
            hold_ms as f64 / 1000.0,
            moves.join("; "),
            to.0,
            to.1
        ))
        .await
        .map(|_| ())
    }

    async fn swipe(&self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        self.shell(&format!(
            "input swipe {:.0} {:.0} {:.0} {:.0} {ms}",
            from.0, from.1, to.0, to.1
        ))
        .await
        .map(|_| ())
    }

    async fn type_text(&self, text: &str) -> Result<()> {
        self.shell(&format!("input text {}", input_text(text)))
            .await?;
        sleep(Duration::from_millis(300)).await;
        Ok(())
    }

    async fn clear(&self, _chars: usize) -> Result<()> {
        // Select all (ctrl+A), then delete.
        self.shell("input keycombination 113 29; input keyevent 67")
            .await
            .map(|_| ())
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
        self.shell(&format!("input keyevent {code}"))
            .await
            .map(|_| ())
    }

    async fn touch_down(&self, x: f64, y: f64) -> Result<()> {
        self.shell(&format!("input motionevent DOWN {x:.0} {y:.0}"))
            .await
            .map(|_| ())
    }

    async fn touch_move(&self, x: f64, y: f64) -> Result<()> {
        self.shell(&format!("input motionevent MOVE {x:.0} {y:.0}"))
            .await
            .map(|_| ())
    }

    async fn touch_up(&self, x: f64, y: f64) -> Result<()> {
        self.shell(&format!("input motionevent UP {x:.0} {y:.0}"))
            .await
            .map(|_| ())
    }

    async fn screen_size(&self) -> Result<(f64, f64)> {
        let out = self.shell("wm size").await?;
        // An override (Override size) wins over the physical size.
        let line = out
            .lines()
            .rev()
            .find(|l| l.contains("size:"))
            .ok_or_else(|| device(format!("wm size: {out}")))?;
        let size = line.rsplit(':').next().unwrap_or("").trim();
        let (w, h) = size
            .split_once('x')
            .ok_or_else(|| device(format!("wm size: {size}")))?;
        Ok((
            w.trim().parse().map_err(device)?,
            h.trim().parse().map_err(device)?,
        ))
    }

    async fn density(&self) -> f64 {
        let out = self.shell("wm density").await.unwrap_or_default();
        out.lines()
            .rev()
            .find_map(|l| l.rsplit(':').next()?.trim().parse::<f64>().ok())
            .map_or(1.0, |dpi| dpi / 160.0)
    }

    async fn tap_burst(&self, x: f64, y: f64, count: u32, interval_ms: u64) -> Result<()> {
        // One shell, so the device keeps the timing; each `input tap` itself takes a little time on the device.
        let taps: Vec<String> = (0..count)
            .map(|_| format!("input tap {x:.0} {y:.0}"))
            .collect();
        self.shell(&taps.join(&format!("; sleep {}; ", interval_ms as f64 / 1000.0)))
            .await
            .map(|_| ())
    }

    async fn intercept_network(&self, on: bool) -> Result<()> {
        let mut proxies = self.proxies.lock().await;
        if !on {
            if let Some(p) = proxies.take() {
                for (device, host) in &p.forwards {
                    self.adb(&["reverse", &format!("tcp:{device}"), &format!("tcp:{host}")])
                        .await?;
                }
            }
            return Ok(());
        }
        if proxies.is_some() {
            return Ok(());
        }
        let forwards: Vec<(u16, u16)> = self
            .forwards()
            .await?
            .into_iter()
            .filter(|&(_, host)| host < PROXY_OFFSET)
            .collect();
        if forwards.is_empty() {
            return Err(device(
                "no adb reverse forwards to put a proxy in (the app reaches nothing local)",
            ));
        }
        let started = Proxies::start(forwards.clone()).await.map_err(device)?;
        for (device_port, host) in &forwards {
            self.adb(&[
                "reverse",
                &format!("tcp:{device_port}"),
                &format!("tcp:{}", host + PROXY_OFFSET),
            ])
            .await?;
        }
        *proxies = Some(started);
        Ok(())
    }

    async fn network_drop(&self, seconds: u64) -> Result<()> {
        {
            let proxies = self.proxies.lock().await;
            let p = proxies.as_ref().ok_or_else(|| {
                DriverError::Invalid(
                    "network-drop needs the network intercepted before the app launched: list network-drop in the spec's tools"
                        .into(),
                )
            })?;
            p.drop_network();
        }
        sleep(Duration::from_secs(seconds)).await;
        if let Some(p) = self.proxies.lock().await.as_ref() {
            p.restore_network();
        }
        Ok(())
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

    async fn stress(&self, load: u32, seconds: u64) -> Result<()> {
        let cores: u32 = self.shell("nproc").await?.trim().parse().unwrap_or(4);
        let loops = (f64::from(cores) * f64::from(load) / 100.0).ceil().max(1.0) as u32;
        let busy: Vec<String> = (0..loops)
            .map(|_| format!("(timeout {seconds} sh -c 'while :; do :; done' &)"))
            .collect();
        self.shell(&busy.join("; ")).await?;
        sleep(Duration::from_secs(seconds)).await;
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
    fn typed_text_survives_the_shell() {
        assert_eq!(input_text("E2E Seed Renamed"), "E2E%sSeed%sRenamed");
        assert_eq!(input_text("Bach's (1)"), r"Bach\'s%s\(1\)");
    }
}
