//! The web driver: its own headless Chrome over the DevTools protocol, reading Chrome's accessibility tree.

use crate::Platform;
use crate::device::{Device, DriverError, Result, SETTLE_MS, Stop, touch_path};
use crate::element::{Element, Rect};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::accessibility::{
    AxNode, AxPropertyName, AxValue, GetFullAxTreeParams,
};
use chromiumoxide::cdp::browser_protocol::dom::GetBoxModelParams;
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::network::{
    EmulateNetworkConditionsByRuleParams, EnableParams as NetworkEnable, NetworkConditions,
    OverrideNetworkStateParams,
};
use chromiumoxide::cdp::js_protocol::runtime::{EventConsoleApiCalled, EventExceptionThrown};
use chromiumoxide::page::{Page, ScreenshotParams};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::sleep;

/// Roles that only wrap or repeat what their parent already says.
const SKIPPED_ROLES: &[&str] = &[
    "InlineTextBox",
    "RootWebArea",
    "none",
    "generic",
    "LineBreak",
];

pub struct Web {
    browser: Browser,
    page: Page,
    base_url: String,
    errors: Arc<Mutex<Vec<String>>>,
    tasks: Vec<JoinHandle<()>>,
    /// Where a kept `touch` began and where it's held now, until `release`.
    held: Mutex<Option<Held>>,
    /// This launch's own profile, removed on close.
    profile: PathBuf,
}

/// A kept touch's start and where it is now.
type Held = ((f64, f64), (f64, f64));

/// Copies a profile, leaving out the running Chrome's locks (symlinks) and caches it rebuilds itself.
fn copy_profile(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let kind = entry.file_type()?;
        let skip = name.to_string_lossy().starts_with("Singleton")
            || matches!(
                name.to_str(),
                Some("Cache" | "Code Cache" | "GPUCache" | "GrShaderCache" | "ShaderCache")
            );
        if skip || kind.is_symlink() {
            continue;
        }
        let target = to.join(&name);
        if kind.is_dir() {
            copy_profile(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Opens Chrome on `profile` for a person to sign in to Google by hand, and returns once they quit it. It's the
/// Chrome runs use, with the same keychain switches, so runs can read the session; but without automation, which
/// Google's sign-in refuses.
pub fn open_profile(profile: &Path, url: &str) -> Result<()> {
    let chrome =
        chromiumoxide::detection::default_executable(Default::default()).map_err(device)?;
    std::fs::create_dir_all(profile).map_err(device)?;
    let status = std::process::Command::new(chrome)
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--use-mock-keychain",
            "--password-store=basic",
            "--no-first-run",
            "--disable-sync",
            url,
        ])
        .status()
        .map_err(device)?;
    if !status.success() {
        return Err(device(format!("Chrome exited with {status}")));
    }
    Ok(())
}

fn device(e: impl std::fmt::Display) -> DriverError {
    DriverError::Device(e.to_string())
}

fn text(v: &Option<AxValue>) -> Option<String> {
    let value = v.as_ref()?.value.as_ref()?;
    let s = match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => return None,
        other => other.to_string(),
    };
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn property(node: &AxNode, name: AxPropertyName) -> Option<String> {
    node.properties
        .as_ref()?
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| p.value.value.as_ref())
        .map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
}

async fn set_page_offline(page: &Page, offline: bool) -> Result<()> {
    let all = NetworkConditions::new("", 0.0, -1.0, -1.0);
    page.execute(EmulateNetworkConditionsByRuleParams::new(
        offline,
        vec![all],
    ))
    .await
    .map_err(|e| device(format!("emulateNetworkConditionsByRule: {e}")))?;
    page.execute(OverrideNetworkStateParams::new(offline, 0.0, -1.0, -1.0))
        .await
        .map_err(|e| device(format!("overrideNetworkState: {e}")))?;
    Ok(())
}

impl Web {
    async fn touch_down(&self, x: f64, y: f64) -> Result<()> {
        self.mouse(DispatchMouseEventType::MouseMoved, x, y, false)
            .await?;
        self.mouse(DispatchMouseEventType::MousePressed, x, y, true)
            .await
    }

    async fn touch_move(&self, x: f64, y: f64) -> Result<()> {
        self.mouse(DispatchMouseEventType::MouseMoved, x, y, true)
            .await
    }

    async fn touch_up(&self, x: f64, y: f64) -> Result<()> {
        self.mouse(DispatchMouseEventType::MouseReleased, x, y, false)
            .await
    }
    /// Starts Chrome with a fresh, temporary profile and opens `base_url`: headless, or in a window to watch with
    /// `headed`. Input goes through the DevTools protocol either way, but a click or resize in the window can steer
    /// a test, so a watched window is for looking only.
    pub async fn launch(base_url: &str, width: u32, height: u32, headed: bool) -> Result<Web> {
        Self::launch_from(base_url, width, height, headed, None).await
    }

    /// As [`Web::launch`], but the temporary profile starts as a copy of `profile` (one made with [`open_profile`]),
    /// so a Google session signed in there by hand is there too, and nothing the run does goes back into it.
    pub async fn launch_from(
        base_url: &str,
        width: u32,
        height: u32,
        headed: bool,
        profile: Option<&Path>,
    ) -> Result<Web> {
        let profile_dir = std::env::temp_dir().join(format!(
            "soundcheck-chrome-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        if let Some(from) = profile {
            if !from.is_dir() {
                return Err(device(format!(
                    "no Chrome profile at {}: make it with `soundcheck web-profile`",
                    from.display()
                )));
            }
            copy_profile(from, &profile_dir).map_err(device)?;
        }
        let mut builder = BrowserConfig::builder()
            .user_data_dir(&profile_dir)
            .window_size(width, height);
        if headed {
            builder = builder.with_head();
        }
        let config = builder.build().map_err(device)?;
        let (browser, mut handler) = Browser::launch(config).await.map_err(device)?;
        let mut tasks = vec![tokio::spawn(async move {
            while handler.next().await.is_some() {}
        })];
        let page = browser.new_page("about:blank").await.map_err(device)?;
        // Services treat "HeadlessChrome" as a bot (PostHog returns no feature flags to it), so it reads as the
        // Chrome it is.
        let agent = page
            .user_agent()
            .await
            .map_err(device)?
            .replace("HeadlessChrome", "Chrome");
        page.set_user_agent(agent).await.map_err(device)?;
        page.execute(SetDeviceMetricsOverrideParams::new(
            width, height, 1.0, false,
        ))
        .await
        .map_err(device)?;

        let errors = Arc::new(Mutex::new(Vec::new()));
        let mut exceptions = page
            .event_listener::<EventExceptionThrown>()
            .await
            .map_err(device)?;
        let sink = errors.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(e) = exceptions.next().await {
                let d = &e.exception_details;
                let line = d
                    .exception
                    .as_ref()
                    .and_then(|x| x.description.clone())
                    .unwrap_or_else(|| d.text.clone());
                sink.lock().unwrap_or_else(|e| e.into_inner()).push(line);
            }
        }));
        let mut console = page
            .event_listener::<EventConsoleApiCalled>()
            .await
            .map_err(device)?;
        let sink = errors.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(e) = console.next().await {
                if format!("{:?}", e.r#type).eq_ignore_ascii_case("error") {
                    let words: Vec<String> = e
                        .args
                        .iter()
                        .filter_map(|a| {
                            a.value
                                .as_ref()
                                .map(|v| v.to_string())
                                .or_else(|| a.description.clone())
                        })
                        .collect();
                    sink.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(format!("console.error: {}", words.join(" ")));
                }
            }
        }));
        let web = Web {
            browser,
            page,
            base_url: base_url.to_string(),
            errors,
            tasks,
            held: Mutex::new(None),
            profile: profile_dir,
        };
        web.launch().await?;
        Ok(web)
    }

    pub async fn close(mut self) {
        let _ = self.browser.close().await;
        let _ = self.browser.wait().await;
        let _ = std::fs::remove_dir_all(&self.profile);
        for t in self.tasks.drain(..) {
            t.abort();
        }
    }

    /// Scrolls a point into the viewport if it's outside, the way a user would before touching it, and returns
    /// where it is now. The whole page is in the tree, but a click outside the viewport lands on nothing.
    /// Where reading and input go: a window the app's page opened (Google's sign-in popup, a link to a new tab) while
    /// it's open, as a person would be looking at it; the app's own page otherwise.
    async fn current(&self) -> Page {
        let main = self.page.target_id();
        let pages = self.browser.pages().await.unwrap_or_default();
        for page in pages
            .into_iter()
            .filter(|p| p.opener_id().as_ref() == Some(main))
        {
            // A popup that just closed itself stays listed for a moment after its session has gone, and one mid-close
            // may never answer.
            let alive = tokio::time::timeout(Duration::from_secs(1), page.evaluate("1")).await;
            if matches!(alive, Ok(Ok(_))) {
                return page;
            }
        }
        self.page.clone()
    }

    async fn reveal(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        let page = self.current().await;
        let script = format!(
            "(() => {{ const x = {x}, y = {y}; \
               if (y >= 0 && y < innerHeight && x >= 0 && x < innerWidth) return [x, y]; \
               const before = [scrollX, scrollY]; \
               scrollBy(x < 0 || x >= innerWidth ? x - innerWidth / 2 : 0, y < 0 || y >= innerHeight ? y - innerHeight / 2 : 0); \
               return [x - (scrollX - before[0]), y - (scrollY - before[1])]; }})()"
        );
        let point: Vec<f64> = page
            .evaluate(script)
            .await
            .map_err(device)?
            .into_value()
            .map_err(device)?;
        Ok((point[0], point[1]))
    }

    async fn mouse(
        &self,
        kind: DispatchMouseEventType,
        x: f64,
        y: f64,
        pressed: bool,
    ) -> Result<()> {
        let page = self.current().await;
        let mut params = DispatchMouseEventParams::new(kind, x, y);
        params.button = Some(
            if pressed || params.r#type != DispatchMouseEventType::MouseMoved {
                MouseButton::Left
            } else {
                MouseButton::None
            },
        );
        params.buttons = Some(if pressed { 1 } else { 0 });
        params.click_count = Some(1);
        page.execute(params).await.map_err(device)?;
        Ok(())
    }

    /// Requests fail while offline, and the page's navigator.onLine agrees.
    async fn set_offline(&self, offline: bool) -> Result<()> {
        set_page_offline(&self.page, offline).await
    }

    async fn key(&self, key: &str, code: i64) -> Result<()> {
        let page = self.current().await;
        for kind in [DispatchKeyEventType::KeyDown, DispatchKeyEventType::KeyUp] {
            let mut params = DispatchKeyEventParams::new(kind);
            params.key = Some(key.into());
            params.code = Some(key.into());
            params.windows_virtual_key_code = Some(code);
            page.execute(params).await.map_err(device)?;
        }
        Ok(())
    }
}

impl Device for Web {
    fn platform(&self) -> Platform {
        Platform::Web
    }

    async fn read(&self, all: bool) -> Result<Vec<Element>> {
        let page = self.current().await;
        let tree = page
            .execute(GetFullAxTreeParams::default())
            .await
            .map_err(device)?;
        let mut elements: Vec<Element> = Vec::new();
        for node in &tree.result.nodes {
            if node.ignored && !all {
                continue;
            }
            let role = text(&node.role);
            if role.as_deref().is_some_and(|r| SKIPPED_ROLES.contains(&r)) {
                continue;
            }
            let name = text(&node.name);
            let value = text(&node.value);
            let Some(label) = name.clone().or_else(|| value.clone()) else {
                continue;
            };
            let Some(backend) = node.backend_dom_node_id else {
                continue;
            };
            let params = GetBoxModelParams {
                backend_node_id: Some(backend),
                ..Default::default()
            };
            let Ok(model) = page.execute(params).await else {
                continue;
            };
            let quad = model.result.model.border.inner();
            let xs = [quad[0], quad[2], quad[4], quad[6]];
            let ys = [quad[1], quad[3], quad[5], quad[7]];
            let x = xs.iter().copied().fold(f64::INFINITY, f64::min);
            let y = ys.iter().copied().fold(f64::INFINITY, f64::min);
            let frame = Rect {
                x,
                y,
                width: xs.iter().copied().fold(f64::NEG_INFINITY, f64::max) - x,
                height: ys.iter().copied().fold(f64::NEG_INFINITY, f64::max) - y,
            };
            if frame.width <= 0.0 || frame.height <= 0.0 {
                continue;
            }
            let flag = |name| property(node, name).map(|v| v == "true");
            let element = Element {
                label,
                value: value.filter(|v| Some(v) != name.as_ref()),
                id: None,
                role,
                frame,
                enabled: flag(AxPropertyName::Disabled) != Some(true),
                checked: property(node, AxPropertyName::Checked).map(|v| v == "true"),
                selected: flag(AxPropertyName::Selected),
                expanded: flag(AxPropertyName::Expanded),
                focused: flag(AxPropertyName::Focused) == Some(true),
                scroll: None,
            };
            // A text node repeats its parent's name in the same place: keep one.
            let duplicate = elements.iter().any(|e| {
                e.label == element.label
                    && (e.frame.x - element.frame.x).abs() < 1.0
                    && (e.frame.y - element.frame.y).abs() < 1.0
            });
            if !duplicate || all {
                elements.push(element);
            }
        }
        Ok(elements)
    }

    async fn tap_at(&self, x: f64, y: f64) -> Result<()> {
        let (x, y) = self.reveal(x, y).await?;
        self.mouse(DispatchMouseEventType::MouseMoved, x, y, false)
            .await?;
        self.mouse(DispatchMouseEventType::MousePressed, x, y, true)
            .await?;
        self.mouse(DispatchMouseEventType::MouseReleased, x, y, false)
            .await
    }

    async fn hold_at(&self, x: f64, y: f64, ms: u64) -> Result<()> {
        let (x, y) = self.reveal(x, y).await?;
        self.mouse(DispatchMouseEventType::MouseMoved, x, y, false)
            .await?;
        self.mouse(DispatchMouseEventType::MousePressed, x, y, true)
            .await?;
        sleep(Duration::from_millis(ms)).await;
        self.mouse(DispatchMouseEventType::MouseReleased, x, y, false)
            .await
    }

    async fn touch(&self, stops: &[Stop], keep: bool) -> Result<()> {
        if self.held.lock().unwrap().is_some() {
            self.release(true).await?;
        }
        let Some(first) = stops.first() else {
            return Ok(());
        };
        let revealed = self.reveal(first.x, first.y).await?;
        let (dx, dy) = (revealed.0 - first.x, revealed.1 - first.y);
        let mut at = revealed;
        self.touch_down(at.0, at.1).await?;
        sleep(Duration::from_millis(first.pause_ms)).await;
        for stop in &stops[1..] {
            let to = (stop.x + dx, stop.y + dy);
            for (x, y) in touch_path(at, to) {
                self.touch_move(x, y).await?;
                sleep(Duration::from_millis(16)).await;
            }
            sleep(Duration::from_millis(stop.pause_ms)).await;
            at = to;
        }
        if keep {
            *self.held.lock().unwrap() = Some((revealed, at));
            return Ok(());
        }
        if stops.len() > 1 {
            sleep(Duration::from_millis(SETTLE_MS)).await;
        }
        self.touch_up(at.0, at.1).await
    }

    async fn release(&self, back: bool) -> Result<()> {
        let held = self.held.lock().unwrap().take();
        let Some((origin, at)) = held else {
            return Err(DriverError::Invalid("no touch held".into()));
        };
        if !back {
            return self.touch_up(at.0, at.1).await;
        }
        for (x, y) in touch_path(at, origin) {
            self.touch_move(x, y).await?;
            sleep(Duration::from_millis(16)).await;
        }
        self.touch_up(origin.0, origin.1).await
    }

    async fn swipe(&self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        let steps = (ms / 16).clamp(2, 60);
        self.mouse(DispatchMouseEventType::MouseMoved, from.0, from.1, false)
            .await?;
        self.mouse(DispatchMouseEventType::MousePressed, from.0, from.1, true)
            .await?;
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let (x, y) = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
            self.mouse(DispatchMouseEventType::MouseMoved, x, y, true)
                .await?;
            sleep(Duration::from_millis(ms / steps)).await;
        }
        self.mouse(DispatchMouseEventType::MouseReleased, to.0, to.1, false)
            .await
    }

    async fn type_text(&self, text: &str) -> Result<()> {
        let page = self.current().await;
        page.execute(InsertTextParams::new(text))
            .await
            .map_err(device)?;
        Ok(())
    }

    async fn clear(&self, chars: usize) -> Result<()> {
        let page = self.current().await;
        // Select the field's text, then delete it with real key events so the app hears an edit.
        page
            .evaluate("document.activeElement && document.activeElement.select && document.activeElement.select()")
            .await
            .map_err(device)?;
        self.key("Backspace", 8).await?;
        // Fields that can't select (contenteditable) fall back to one Backspace per character.
        for _ in 0..chars.min(200) {
            let empty: bool = page
                .evaluate("!document.activeElement || !(document.activeElement.value ?? document.activeElement.textContent)")
                .await
                .map_err(device)?
                .into_value()
                .unwrap_or(true);
            if empty {
                break;
            }
            self.key("Backspace", 8).await?;
        }
        Ok(())
    }

    async fn launch(&self) -> Result<()> {
        // A window left open by the last spec would otherwise take every read and tap.
        let main = self.page.target_id().clone();
        for other in self.browser.pages().await.map_err(device)? {
            if *other.target_id() != main {
                let _ = other.close().await;
            }
        }
        self.page
            .goto(self.base_url.as_str())
            .await
            .map_err(device)?;
        self.page.wait_for_navigation().await.map_err(device)?;
        self.errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        Ok(())
    }

    async fn screenshot(&self, path: &Path) -> Result<()> {
        let page = self.current().await;
        page.save_screenshot(ScreenshotParams::builder().build(), path)
            .await
            .map_err(device)?;
        Ok(())
    }

    async fn errors(&self) -> Result<Vec<String>> {
        Ok(self
            .errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone())
    }

    async fn press(&self, key: &str) -> Result<()> {
        let code = match key {
            "Enter" => 13,
            "Escape" => 27,
            "Tab" => 9,
            "Backspace" => 8,
            "Space" | " " => 32,
            "PageUp" => 33,
            "PageDown" => 34,
            "End" => 35,
            "Home" => 36,
            "ArrowLeft" => 37,
            "ArrowUp" => 38,
            "ArrowRight" => 39,
            "ArrowDown" => 40,
            other => {
                return Err(DriverError::Invalid(format!(
                    "{other} isn't a key soundcheck knows; use web key names such as ArrowRight or Enter"
                )));
            }
        };
        self.key(key, code).await
    }

    async fn screen_size(&self) -> Result<(f64, f64)> {
        let page = self.current().await;
        let size: Vec<f64> = page
            .evaluate("[innerWidth, innerHeight]")
            .await
            .map_err(device)?
            .into_value()
            .map_err(device)?;
        Ok((size[0], size[1]))
    }

    async fn hover_at(&self, x: f64, y: f64) -> Result<()> {
        let (x, y) = self.reveal(x, y).await?;
        self.mouse(DispatchMouseEventType::MouseMoved, x, y, false)
            .await
    }

    async fn goto(&self, url: &str) -> Result<()> {
        match url {
            "back" => {
                self.page.evaluate("history.back()").await.map_err(device)?;
            }
            "forward" => {
                self.page
                    .evaluate("history.forward()")
                    .await
                    .map_err(device)?;
            }
            url => {
                self.page.goto(url).await.map_err(device)?;
            }
        }
        sleep(Duration::from_millis(100)).await;
        Ok(())
    }

    async fn viewport(&self, width: u32, height: u32) -> Result<()> {
        self.page
            .execute(SetDeviceMetricsOverrideParams::new(
                width, height, 1.0, false,
            ))
            .await
            .map_err(device)?;
        Ok(())
    }

    async fn offline(&self, on: bool) -> Result<()> {
        self.page
            .execute(NetworkEnable::default())
            .await
            .map_err(device)?;
        self.set_offline(on).await
    }

    async fn network_drop(&self, seconds: u64) -> Result<()> {
        self.page
            .execute(NetworkEnable::default())
            .await
            .map_err(device)?;
        self.set_offline(true).await?;
        // Restore on a timer, as on Android: the call returns once the network is down.
        let page = self.page.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs(seconds)).await;
            let _ = set_page_offline(&page, false).await;
        });
        Ok(())
    }
}
