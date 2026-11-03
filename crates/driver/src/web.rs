//! The web driver: its own headless Chrome over the DevTools protocol, reading Chrome's accessibility tree.

use crate::Platform;
use crate::device::{Device, DriverError, Result, SETTLE_MS, Stop, touch_path};
use crate::element::{Element, Rect};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::accessibility::{
    AxNode, AxPropertyName, AxValue, GetFullAxTreeParams,
};
use chromiumoxide::cdp::browser_protocol::browser::{
    Bounds, DownloadProgressState, EventDownloadProgress, EventDownloadWillBegin,
    GetWindowForTargetParams, SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
    SetWindowBoundsParams,
};
use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, GetBoxModelParams, SetFileInputFilesParams,
};
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::page::{
    DialogType, EventFileChooserOpened, EventJavascriptDialogClosed, EventJavascriptDialogOpening,
    FileChooserOpenedMode, HandleJavaScriptDialogParams, SetInterceptFileChooserDialogParams,
};
use chromiumoxide::cdp::browser_protocol::target::{
    FilterEntry, SetAutoAttachParams, TargetFilter,
};
use chromiumoxide::cdp::js_protocol::runtime::{EventConsoleApiCalled, EventExceptionThrown};
use chromiumoxide::page::{Page, ScreenshotParams};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// An alert, confirm or prompt the app's page has open: it blocks the page until it's answered.
    dialog: Arc<Mutex<Option<Dialog>>>,
    /// Set while `launch` leaves the page, so a "Leave site?" it raises is answered Leave, as closing the tab would.
    relaunching: Arc<AtomicBool>,
    /// What's been typed into an open prompt, sent when it's accepted.
    prompt: Mutex<String>,
    /// The viewport, which can't be asked while a dialog blocks the page.
    size: Mutex<(f64, f64)>,
    /// Headed, how much wider and taller the window is than the page it shows: its tab strip, address bar and edges.
    frame: Option<(i64, i64)>,
    /// Files `push` has put where the file picker finds them, in the order pushed.
    pushed: Mutex<Vec<String>>,
    /// A file picker the page has opened, held by soundcheck in place of the system's window until it's answered.
    chooser: Arc<Mutex<Option<Chooser>>>,
    /// What the page has downloaded since the app last launched.
    downloads: Arc<Mutex<Vec<Download>>>,
}

/// A kept touch's start and where it is now.
type Held = ((f64, f64), (f64, f64));

#[derive(Clone)]
struct Chooser {
    input: BackendNodeId,
    multiple: bool,
    ticked: Vec<String>,
}

#[derive(Clone)]
struct Download {
    guid: String,
    name: String,
    /// Where Chrome saved it, once it's complete.
    path: Option<PathBuf>,
    failed: bool,
}

#[derive(Clone)]
struct Dialog {
    message: String,
    /// Confirm and prompt have Cancel; alert has only OK.
    cancel: bool,
    prompt: bool,
    /// "Leave site?", raised by the page navigating away while it asks to be kept.
    leaving: bool,
}

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

/// `PDF, 34 KB` once saved: its kind from the file's own first bytes, since a name can say anything.
fn describe_download(d: &Download) -> String {
    if d.failed {
        return "failed".into();
    }

    let Some(path) = &d.path else {
        return "downloading".into();
    };
    let Ok(bytes) = std::fs::read(path) else {
        return "saved, but unreadable".into();
    };
    let kind = if bytes.starts_with(b"%PDF-") {
        "PDF"
    } else {
        "file"
    };
    let size = match bytes.len() {
        n if n < 1024 => format!("{n} bytes"),
        n if n < 1024 * 1024 => format!("{} KB", n.div_ceil(1024)),
        n => format!("{:.1} MB", n as f64 / (1024.0 * 1024.0)),
    };
    format!("{kind}, {size}")
}

impl Web {
    /// Sizes a headed window so it shows the whole viewport: Chrome counts its own toolbars in a window's size.
    async fn fit_window(&self, width: u32, height: u32) -> Result<()> {
        let Some((w, h)) = self.frame else {
            return Ok(());
        };
        let window = self
            .page
            .execute(GetWindowForTargetParams::default())
            .await
            .map_err(device)?;
        let bounds = Bounds {
            width: Some(i64::from(width) + w),
            height: Some(i64::from(height) + h),
            ..Default::default()
        };
        self.page
            .execute(SetWindowBoundsParams::new(window.result.window_id, bounds))
            .await
            .map_err(device)?;
        Ok(())
    }

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
        // chromiumoxide pauses attached targets for a debugger and detaches from service workers without resuming
        // them, so the app's service worker never runs: leave them out. Frames still need the pause.
        let skip = |kind: &str| FilterEntry::builder().r#type(kind).exclude(true).build();
        page.execute(
            SetAutoAttachParams::builder()
                .auto_attach(true)
                .wait_for_debugger_on_start(true)
                .flatten(true)
                .filter(TargetFilter::new(vec![
                    skip("service_worker"),
                    skip("browser"),
                    skip("tab"),
                    FilterEntry::default(),
                ]))
                .build()
                .map_err(device)?,
        )
        .await
        .map_err(device)?;
        // Services treat "HeadlessChrome" as a bot (PostHog returns no feature flags to it), so it reads as the
        // Chrome it is.
        let agent = page
            .user_agent()
            .await
            .map_err(device)?
            .replace("HeadlessChrome", "Chrome");
        page.set_user_agent(agent).await.map_err(device)?;
        let frame = if headed {
            let (w, h): (i64, i64) = page
                .evaluate("[outerWidth - innerWidth, outerHeight - innerHeight]")
                .await
                .map_err(device)?
                .into_value()
                .map_err(device)?;
            Some((w, h))
        } else {
            None
        };
        page.execute(SetDeviceMetricsOverrideParams::new(
            width, height, 1.0, false,
        ))
        .await
        .map_err(device)?;

        let dialog = Arc::new(Mutex::new(None));
        let relaunching = Arc::new(AtomicBool::new(false));
        let mut opening = page
            .event_listener::<EventJavascriptDialogOpening>()
            .await
            .map_err(device)?;
        let leaving_now = relaunching.clone();
        let leave_page = page.clone();
        let sink = dialog.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(e) = opening.next().await {
                if e.r#type == DialogType::Beforeunload && leaving_now.load(Ordering::SeqCst) {
                    let _ = leave_page
                        .execute(HandleJavaScriptDialogParams::new(true))
                        .await;
                    continue;
                }

                *sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(Dialog {
                    message: e.message.clone(),
                    cancel: e.r#type != DialogType::Alert,
                    leaving: e.r#type == DialogType::Beforeunload,
                    prompt: e.r#type == DialogType::Prompt,
                });
            }
        }));
        let mut closed = page
            .event_listener::<EventJavascriptDialogClosed>()
            .await
            .map_err(device)?;
        let sink = dialog.clone();
        tasks.push(tokio::spawn(async move {
            while closed.next().await.is_some() {
                *sink.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }));
        let chooser = Arc::new(Mutex::new(None));
        let mut choosing = page
            .event_listener::<EventFileChooserOpened>()
            .await
            .map_err(device)?;
        let sink = chooser.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(e) = choosing.next().await {
                // Only an `<input type=file>` can be given files; a picker opened any other way stays unanswered.
                if let Some(input) = e.backend_node_id {
                    *sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(Chooser {
                        input,
                        multiple: e.mode == FileChooserOpenedMode::SelectMultiple,
                        ticked: Vec::new(),
                    });
                }
            }
        }));
        page.execute(SetInterceptFileChooserDialogParams::new(true))
            .await
            .map_err(device)?;

        let downloads: Arc<Mutex<Vec<Download>>> = Arc::new(Mutex::new(Vec::new()));
        let mut starting = browser
            .event_listener::<EventDownloadWillBegin>()
            .await
            .map_err(device)?;
        let sink = downloads.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(e) = starting.next().await {
                sink.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(Download {
                        guid: e.guid.clone(),
                        name: e.suggested_filename.clone(),
                        path: None,
                        failed: false,
                    });
            }
        }));
        let mut progress = browser
            .event_listener::<EventDownloadProgress>()
            .await
            .map_err(device)?;
        let sink = downloads.clone();
        let saved_to = profile_dir.join("downloads");
        tasks.push(tokio::spawn(async move {
            while let Some(e) = progress.next().await {
                let mut all = sink.lock().unwrap_or_else(|e| e.into_inner());
                let Some(d) = all.iter_mut().find(|d| d.guid == e.guid) else {
                    continue;
                };
                match e.state {
                    DownloadProgressState::Completed => {
                        d.path = Some(
                            e.file_path
                                .as_ref()
                                .map_or_else(|| saved_to.join(&d.name), PathBuf::from),
                        );
                    }
                    DownloadProgressState::Canceled => d.failed = true,
                    DownloadProgressState::InProgress => {}
                }
            }
        }));
        let mut behaviour = SetDownloadBehaviorParams::new(SetDownloadBehaviorBehavior::Allow);
        behaviour.download_path = Some(profile_dir.join("downloads").display().to_string());
        behaviour.events_enabled = Some(true);
        browser.execute(behaviour).await.map_err(device)?;

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
            dialog,
            relaunching,
            prompt: Mutex::new(String::new()),
            size: Mutex::new((f64::from(width), f64::from(height))),
            frame,
            pushed: Mutex::new(Vec::new()),
            chooser,
            downloads,
        };
        web.fit_window(width, height).await?;
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

    fn open_dialog(&self) -> Option<Dialog> {
        self.dialog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// An open dialog as `see` shows it, in place of the page it blocks: its message, a prompt's field, and its
    /// buttons with Chrome's labels, centred as Chrome draws them.
    fn dialog_elements(&self, dialog: &Dialog) -> Vec<Element> {
        let (w, h) = *self.size.lock().unwrap_or_else(|e| e.into_inner());
        let (cx, cy) = (w / 2.0, h / 2.0);
        let mut elements = vec![Element {
            label: dialog.message.clone(),
            role: Some("alert".into()),
            frame: Rect {
                x: cx - 200.0,
                y: cy - 80.0,
                width: 400.0,
                height: 40.0,
            },
            enabled: true,
            ..Default::default()
        }];
        if dialog.prompt {
            let typed = self.prompt.lock().unwrap_or_else(|e| e.into_inner());
            elements.push(Element {
                label: "Prompt".into(),
                value: Some(typed.clone()),
                role: Some("textbox".into()),
                frame: Rect {
                    x: cx - 200.0,
                    y: cy - 30.0,
                    width: 400.0,
                    height: 32.0,
                },
                enabled: true,
                focused: true,
                ..Default::default()
            });
        }
        let button = |label: &str, x: f64| Element {
            label: label.into(),
            role: Some("button".into()),
            frame: Rect {
                x,
                y: cy + 20.0,
                width: 90.0,
                height: 32.0,
            },
            enabled: true,
            ..Default::default()
        };
        elements.push(button("OK", cx + 10.0));
        if dialog.cancel {
            elements.push(button("Cancel", cx - 100.0));
        }
        elements
    }

    /// Answers the open dialog: OK with what's been typed into a prompt, or Cancel.
    async fn answer_dialog(&self, accept: bool) -> Result<()> {
        let typed = std::mem::take(&mut *self.prompt.lock().unwrap_or_else(|e| e.into_inner()));
        let mut params = HandleJavaScriptDialogParams::new(accept);
        params.prompt_text = Some(typed);
        self.page.execute(params).await.map_err(device)?;
        *self.dialog.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }

    fn open_chooser(&self) -> Option<Chooser> {
        self.chooser
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// An open file picker as `see` shows it, in place of the page: a row for each pushed file (ticked in a picker
    /// that takes several), then Cancel, and Choose when it takes several.
    fn chooser_elements(&self, chooser: &Chooser) -> Vec<Element> {
        let (w, _) = *self.size.lock().unwrap_or_else(|e| e.into_inner());
        let x = w / 2.0 - 200.0;
        let row = |label: &str, role: &str, y: f64| Element {
            label: label.into(),
            role: Some(role.into()),
            frame: Rect {
                x,
                y,
                width: 400.0,
                height: 36.0,
            },
            enabled: true,
            ..Default::default()
        };
        let mut elements = vec![row("File picker", "heading", 80.0)];
        let pushed = self.pushed.lock().unwrap_or_else(|e| e.into_inner());
        if pushed.is_empty() {
            elements.push(row("No files: push one first", "text", 120.0));
        }
        for (i, name) in pushed.iter().enumerate() {
            let mut file = row(
                name,
                if chooser.multiple {
                    "checkbox"
                } else {
                    "button"
                },
                120.0 + 40.0 * i as f64,
            );
            if chooser.multiple {
                file.checked = Some(chooser.ticked.contains(name));
            }
            elements.push(file);
        }
        let y = 130.0 + 40.0 * pushed.len().max(1) as f64;
        elements.push(row("Cancel", "button", y));
        if chooser.multiple {
            let mut choose = row("Choose", "button", y + 40.0);
            choose.enabled = !chooser.ticked.is_empty();
            elements.push(choose);
        }
        elements
    }

    /// A tap in an open file picker: a file is picked (or ticked, when it takes several), Choose gives the page the
    /// ticked files, and Cancel closes it with none.
    async fn tap_chooser(&self, chooser: Chooser, x: f64, y: f64) -> Result<()> {
        let hit = self.chooser_elements(&chooser).into_iter().find(|e| {
            let f = &e.frame;
            matches!(e.role.as_deref(), Some("button" | "checkbox"))
                && x >= f.x
                && x <= f.x + f.width
                && y >= f.y
                && y <= f.y + f.height
        });
        let Some(hit) = hit else {
            return self.blocked();
        };
        let picked = match hit.label.as_str() {
            "Cancel" => Vec::new(),
            "Choose" if hit.enabled => chooser.ticked.clone(),
            "Choose" => return self.blocked(),
            name if chooser.multiple => {
                let mut open = self.chooser.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(open) = open.as_mut() {
                    if let Some(i) = open.ticked.iter().position(|t| t == name) {
                        open.ticked.remove(i);
                    } else {
                        open.ticked.push(name.to_string());
                    }
                }

                return Ok(());
            }
            name => vec![name.to_string()],
        };
        *self.chooser.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if picked.is_empty() {
            return Ok(());
        }

        let folder = self.profile.join("pushed");
        let params = SetFileInputFilesParams {
            files: picked
                .iter()
                .map(|name| folder.join(name).display().to_string())
                .collect(),
            node_id: None,
            backend_node_id: Some(chooser.input),
            object_id: None,
        };
        self.page.execute(params).await.map_err(device)?;
        Ok(())
    }

    /// Each download since launch as a line at the top of `see`: what it is and its size once it's saved.
    fn download_elements(&self) -> Vec<Element> {
        let downloads = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
        downloads
            .iter()
            .enumerate()
            .map(|(i, d)| Element {
                label: format!("Downloaded {}", d.name),
                value: Some(describe_download(d)),
                role: Some("status".into()),
                frame: Rect {
                    x: 0.0,
                    y: 24.0 * i as f64,
                    width: 400.0,
                    height: 24.0,
                },
                enabled: true,
                ..Default::default()
            })
            .collect()
    }

    /// While a dialog or file picker is open nothing else on the page answers, so anything but its own controls is
    /// refused clearly.
    fn blocked(&self) -> Result<()> {
        if let Some(d) = self.open_dialog() {
            return Err(DriverError::Invalid(format!(
                "a browser dialog is open (\"{}\"): tap {} first",
                d.message,
                if d.cancel { "OK or Cancel" } else { "OK" }
            )));
        }

        match self.open_chooser() {
            Some(_) => Err(DriverError::Invalid(
                "a file picker is open: tap a file or Cancel first".into(),
            )),
            None => Ok(()),
        }
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
        let Some(point) = self.unless_dialog(page.evaluate(script)).await? else {
            return self.blocked().map(|()| (x, y));
        };
        let point: Vec<f64> = point.into_value().map_err(device)?;
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
        self.until_dialog(page.execute(params)).await
    }

    /// Chrome answers an input event only once the page has handled it, and a handler that opens a dialog waits on
    /// it: so the event counts as done once a dialog opens, as a person sees the dialog and carries on.
    async fn until_dialog<T>(
        &self,
        sent: impl std::future::Future<Output = std::result::Result<T, chromiumoxide::error::CdpError>>,
    ) -> Result<()> {
        self.unless_dialog(sent).await.map(|_| ())
    }

    /// Any page call waits while a dialog is open, and a page can open one at any moment (after an await in a tap's
    /// handler, say), so a call in flight gives way to it: `None` once one opens.
    async fn unless_dialog<T>(
        &self,
        sent: impl std::future::Future<Output = std::result::Result<T, chromiumoxide::error::CdpError>>,
    ) -> Result<Option<T>> {
        tokio::pin!(sent);
        loop {
            tokio::select! {
                done = &mut sent => return done.map(Some).map_err(device),
                () = sleep(Duration::from_millis(50)) => {
                    if self.open_dialog().is_some() {
                        return Ok(None);
                    }
                }
            }
        }
    }

    async fn key(&self, key: &str, code: i64) -> Result<()> {
        let page = self.current().await;
        for kind in [DispatchKeyEventType::KeyDown, DispatchKeyEventType::KeyUp] {
            let mut params = DispatchKeyEventParams::new(kind);
            params.key = Some(key.into());
            params.code = Some(key.into());
            params.windows_virtual_key_code = Some(code);
            self.until_dialog(page.execute(params)).await?;
        }
        Ok(())
    }
}

impl Device for Web {
    fn platform(&self) -> Platform {
        Platform::Web
    }

    async fn read(&self, all: bool) -> Result<Vec<Element>> {
        if let Some(dialog) = self.open_dialog() {
            return Ok(self.dialog_elements(&dialog));
        }

        if let Some(chooser) = self.open_chooser() {
            return Ok(self.chooser_elements(&chooser));
        }

        let page = self.current().await;
        let Some(tree) = self
            .unless_dialog(page.execute(GetFullAxTreeParams::default()))
            .await?
        else {
            return Ok(self
                .open_dialog()
                .map_or_else(Vec::new, |d| self.dialog_elements(&d)));
        };
        let mut elements = self.download_elements();
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
            let model = match self.unless_dialog(page.execute(params)).await {
                Ok(Some(model)) => model,
                Ok(None) => {
                    return Ok(self
                        .open_dialog()
                        .map_or_else(Vec::new, |d| self.dialog_elements(&d)));
                }
                Err(_) => continue,
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
        if let Some(dialog) = self.open_dialog() {
            let hit = self
                .dialog_elements(&dialog)
                .into_iter()
                .filter(|e| e.role.as_deref() == Some("button"))
                .find(|e| {
                    let f = &e.frame;
                    x >= f.x && x <= f.x + f.width && y >= f.y && y <= f.y + f.height
                });
            return match hit {
                Some(button) => self.answer_dialog(button.label == "OK").await,
                None => self.blocked(),
            };
        }

        if let Some(chooser) = self.open_chooser() {
            return self.tap_chooser(chooser, x, y).await;
        }

        let (x, y) = self.reveal(x, y).await?;
        self.mouse(DispatchMouseEventType::MouseMoved, x, y, false)
            .await?;
        self.mouse(DispatchMouseEventType::MousePressed, x, y, true)
            .await?;
        self.mouse(DispatchMouseEventType::MouseReleased, x, y, false)
            .await
    }

    async fn hold_at(&self, x: f64, y: f64, ms: u64) -> Result<()> {
        self.blocked()?;
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
        self.blocked()?;
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
        self.blocked()?;
        // A desktop scrolls with the wheel or trackpad, never by dragging (a drag that starts on an image or a link
        // drags it out): so a swipe scrolls what's under its start, by as much as a finger would have moved it.
        let steps = (ms / 16).clamp(2, 60);
        self.mouse(DispatchMouseEventType::MouseMoved, from.0, from.1, false)
            .await?;
        let page = self.current().await;
        let (dx, dy) = (
            (from.0 - to.0) / steps as f64,
            (from.1 - to.1) / steps as f64,
        );
        for _ in 0..steps {
            let mut params =
                DispatchMouseEventParams::new(DispatchMouseEventType::MouseWheel, from.0, from.1);
            params.delta_x = Some(dx);
            params.delta_y = Some(dy);
            self.until_dialog(page.execute(params)).await?;
            sleep(Duration::from_millis(ms / steps)).await;
        }
        Ok(())
    }

    async fn type_text(&self, text: &str) -> Result<()> {
        if let Some(dialog) = self.open_dialog() {
            if !dialog.prompt {
                return self.blocked();
            }
            self.prompt
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_str(text);
            return Ok(());
        }
        let page = self.current().await;
        page.execute(InsertTextParams::new(text))
            .await
            .map_err(device)?;
        Ok(())
    }

    async fn clear(&self, chars: usize) -> Result<()> {
        self.blocked()?;
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
        // A relaunch leaves the page, as closing the tab would: "Leave site?" is answered Leave, anything else Cancel.
        if let Some(dialog) = self.open_dialog() {
            self.answer_dialog(dialog.leaving).await?;
        }

        *self.chooser.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        // A window left open by the last spec would otherwise take every read and tap.
        let main = self.page.target_id().clone();
        for other in self.browser.pages().await.map_err(device)? {
            if *other.target_id() != main {
                let _ = other.close().await;
            }
        }
        self.relaunching.store(true, Ordering::SeqCst);
        let went = async {
            self.page.goto(self.base_url.as_str()).await?;
            self.page.wait_for_navigation().await
        }
        .await;
        self.relaunching.store(false, Ordering::SeqCst);
        went.map_err(device)?;
        self.errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        Ok(())
    }

    async fn screenshot(&self, path: &Path) -> Result<()> {
        self.blocked()?;
        let page = self.current().await;
        page.save_screenshot(ScreenshotParams::builder().build(), path)
            .await
            .map_err(device)?;
        Ok(())
    }

    /// Staged for the file picker, which lists what's been pushed; a file pushed again under the same name replaces it.
    async fn push_file(&self, source: &Path, name: &str) -> Result<()> {
        let folder = self.profile.join("pushed");
        std::fs::create_dir_all(&folder).map_err(device)?;
        std::fs::copy(source, folder.join(name)).map_err(device)?;
        let mut pushed = self.pushed.lock().unwrap_or_else(|e| e.into_inner());
        if !pushed.iter().any(|p| p == name) {
            pushed.push(name.to_string());
        }

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
        if self.open_dialog().is_some() {
            return match key {
                "Enter" | "Return" => self.answer_dialog(true).await,
                "Escape" => self.answer_dialog(false).await,
                _ => self.blocked(),
            };
        }

        if self.open_chooser().is_some() {
            if key == "Escape" {
                *self.chooser.lock().unwrap_or_else(|e| e.into_inner()) = None;
                return Ok(());
            }

            return self.blocked();
        }

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
        let Some(size) = self
            .unless_dialog(page.evaluate("[innerWidth, innerHeight]"))
            .await?
        else {
            return Ok(*self.size.lock().unwrap_or_else(|e| e.into_inner()));
        };
        let size: Vec<f64> = size.into_value().map_err(device)?;
        Ok((size[0], size[1]))
    }

    async fn hover_at(&self, x: f64, y: f64) -> Result<()> {
        self.blocked()?;
        let (x, y) = self.reveal(x, y).await?;
        self.mouse(DispatchMouseEventType::MouseMoved, x, y, false)
            .await
    }

    async fn goto(&self, url: &str) -> Result<()> {
        self.blocked()?;
        // Back, forward and close act on the tab in front, as the browser's own buttons do.
        let front = self.current().await;
        match url {
            "back" => {
                front.evaluate("history.back()").await.map_err(device)?;
            }
            "forward" => {
                front.evaluate("history.forward()").await.map_err(device)?;
            }
            "close" => {
                if front.target_id() == self.page.target_id() {
                    return Err(DriverError::Invalid(
                        "close shuts a tab or window the app opened; the app's own tab stays"
                            .into(),
                    ));
                }
                front.close().await.map_err(device)?;
            }
            url => {
                self.page.goto(url).await.map_err(device)?;
            }
        }
        sleep(Duration::from_millis(100)).await;
        Ok(())
    }

    async fn viewport(&self, width: u32, height: u32) -> Result<()> {
        self.blocked()?;
        *self.size.lock().unwrap_or_else(|e| e.into_inner()) =
            (f64::from(width), f64::from(height));
        self.page
            .execute(SetDeviceMetricsOverrideParams::new(
                width, height, 1.0, false,
            ))
            .await
            .map_err(device)?;
        self.fit_window(width, height).await
    }
}
