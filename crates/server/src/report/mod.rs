//! The run's page: one view of the run for the live page and the export, so what's watched is what's kept.

pub mod view;

use crate::run::journal::Entry;
use crate::run::model::{Answer, Call, Final, Posted, ProblemKind, ProblemSource, Status};
use crate::run::usage;
use crate::run::verdict::{VerdictStatus, verdict};
use crate::run::{Row, RowRecord, Run};
use crate::serve::events;
use crate::suite::Suite;
use crate::suite::config::Platform;
use crate::suite::spec::Spec;
use base64::Engine;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

const CSS: &str = include_str!("report.css");
/// The page's app, built from `report-app/` by `npm run build` and checked in, so building soundcheck needs no Node.
pub const APP: &str = include_str!("app.js");
pub const ICON_180: &[u8] = include_bytes!("icon-180.png");
pub const ICON_512: &[u8] = include_bytes!("icon-512.png");
/// No service worker: a live report shown stale while offline is worse than one that says it can't connect.
pub const MANIFEST: &str = r##"{"name":"soundcheck","short_name":"soundcheck","start_url":"/","scope":"/","display":"standalone","background_color":"#f5f5f7","theme_color":"#f5f5f7","icons":[{"src":"/icon-180.png","sizes":"180x180","type":"image/png"},{"src":"/icon-512.png","sizes":"512x512","type":"image/png","purpose":"any maskable"}]}"##;
// The manifest is fetched without cookies unless asked, so behind --remote's sign-in it got the sign-in redirect.
const APP_HEAD: &str = r##"<link rel="manifest" href="/manifest.webmanifest" crossorigin="use-credentials"><link rel="apple-touch-icon" href="/icon-180.png"><meta name="mobile-web-app-capable" content="yes"><meta name="apple-mobile-web-app-capable" content="yes"><meta name="apple-mobile-web-app-title" content="soundcheck"><meta name="theme-color" content="#f5f5f7" media="(prefers-color-scheme: light)"><meta name="theme-color" content="#000000" media="(prefers-color-scheme: dark)">"##;
/// A check waiting this long for the coordinator turns the bar amber.
const CHECKING_WARN_MS: u64 = 5 * 60_000;
/// No journal event for this long while specs remain turns the bar amber.
const IDLE_WARN_MS: u64 = 15 * 60_000;

pub struct Page<'a> {
    pub suite: &'a Suite,
    pub run: &'a Run,
    /// The release under test, e.g. "v2.16.2 · candidate".
    pub label: &'a str,
    /// The live page follows the event stream; an export is a still copy.
    pub live: bool,
    /// Where the run keeps its evidence; screenshots are linked from it live and embedded in an export.
    pub run_dir: Option<&'a Path>,
    /// Set for an export: when it was made, and whether the run had finished.
    pub export: Option<Export>,
    /// Set while the run is starting: its checks are shown in place of what is running.
    pub starting: Option<&'a StartUp>,
}

/// A run that is starting, before any tester can go: the checks passed so far, the step under way and since when, or
/// why it stopped.
#[derive(Debug, Clone, Default)]
pub struct StartUp {
    pub passed: Vec<String>,
    pub doing: Option<(String, u64)>,
    pub failed: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Export {
    pub at: u64,
    pub finished: bool,
}

/// Screenshots and other evidence a tester or the server saved for a row.
pub const EVIDENCE_DIR: &str = "evidence";

/// A file name evidence can use: no folders, nothing hidden.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The prefix every evidence file of one attempt shares, e.g. `annotations-003-eraser-1-ios`.
pub fn evidence_base(spec: &str, attempt: u32, platform: Platform) -> String {
    format!("{}-{attempt}-{platform}", spec.replace('/', "-"))
}

/// Evidence is kept at what a tester's Read shows, which is detail enough for a person too, and as WebP: a full-size
/// PNG made a full run's report 340 MB.
const SHOT_MAX_SIDE: u32 = 2000;
const SHOT_QUALITY: f32 = 80.0;

/// A screenshot among evidence files: WebP, or PNG from older runs.
pub fn is_shot(name: &str) -> bool {
    name.ends_with(".webp") || name.ends_with(".png")
}

/// Stores the device screenshot at `png` as evidence at `to`, scaled down and as WebP. `png` stays.
pub fn store_shot(png: &Path, to: &Path) -> anyhow::Result<()> {
    let img = image::open(png)?.into_rgb8();
    let (w, h) = img.dimensions();
    let long = w.max(h);
    let img = if long > SHOT_MAX_SIDE {
        let scale = f64::from(SHOT_MAX_SIDE) / f64::from(long);
        let size = |v: u32| ((f64::from(v) * scale).round() as u32).max(1);
        image::imageops::resize(
            &img,
            size(w),
            size(h),
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    let (w, h) = img.dimensions();
    let encoded = webp::Encoder::from_rgb(&img, w, h).encode(SHOT_QUALITY);
    std::fs::write(to, &*encoded)?;
    Ok(())
}

/// Takes a screenshot through `capture` (which writes a PNG) and stores it as evidence at `to`.
pub async fn capture_shot<F, E>(to: &Path, capture: F) -> anyhow::Result<()>
where
    F: AsyncFnOnce(&Path) -> Result<(), E>,
    E: std::error::Error + Send + Sync + 'static,
{
    let png = to.with_extension("png");
    capture(&png).await?;
    let (from, dest) = (png.clone(), to.to_path_buf());
    let stored = tokio::task::spawn_blocking(move || store_shot(&from, &dest)).await?;
    let _ = std::fs::remove_file(&png);
    stored
}

/// The file a tester's screenshot `name` is saved as: under its attempt's prefix, ending `.webp`.
pub fn shot_file(base: &str, name: &str) -> String {
    let name = name.trim_end_matches(".webp").trim_end_matches(".png");
    let name = name.strip_prefix(&format!("{base}-")).unwrap_or(name);
    format!("{base}-{name}.webp")
}

/// The evidence file a tester's `name` refers to, if it was saved: its attempt's own file first, then the name as
/// given (or with `.webp` or `.png`), which covers the server's captures and older runs' unprefixed shots.
pub fn resolve_evidence(dir: &Path, base: &str, name: &str) -> Option<String> {
    let shot = shot_file(base, name);
    [
        shot.clone(),
        shot.replace(".webp", ".png"),
        name.to_string(),
        format!("{name}.webp"),
        format!("{name}.png"),
    ]
    .into_iter()
    .find(|f| safe_name(f) && dir.join(f).is_file())
}

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Applies the viewer's saved Light or Dark before the first paint, so a page never flashes the other theme.
const THEME_HEAD: &str = r#"<script>try{var t=localStorage.getItem("soundcheck-theme");if(t==="light"||t==="dark")document.documentElement.dataset.theme=t}catch(e){}</script>"#;

/// The whole document: the page's view as data, and the app that lays it out. A running run's page then follows
/// `updates`; a finished one's stays as loaded, as a run reopened by `queue` is seen on reload.
pub fn document(page: &Page) -> String {
    let view = view::json(page);
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1,viewport-fit=cover\">{APP_HEAD}<title>{} · soundcheck</title><style>{CSS}</style>{THEME_HEAD}</head><body><div id=\"live\" data-live=\"{}\"></div><script type=\"application/json\" id=\"view\">{}</script><script>{APP}</script></body></html>\n",
        esc(page.label),
        page.live && !page.run.state().finished,
        view::script_json(&view),
    )
}

fn platforms(page: &Page) -> Vec<Platform> {
    let mut all: Vec<Platform> = page
        .run
        .plan()
        .specs
        .iter()
        .flat_map(|s| s.platforms.iter().copied())
        .collect();
    all.sort();
    all.dedup();
    all
}

/// A platform as the report shows it: the suite's own name.
fn platform_name(p: Platform) -> &'static str {
    p.as_str()
}

fn feature_name(feature: &str) -> String {
    let words = feature.replace('-', " ");
    let mut chars = words.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn anchor(spec: &str) -> String {
    format!("spec-{}", spec.replace('/', "-"))
}

fn date_time(at: u64) -> String {
    let secs = at / 1000;
    format!(
        "<time data-at=\"{at}\" data-format=\"date\">{:02}:{:02} UTC</time>",
        (secs / 3600) % 24,
        (secs / 60) % 60
    )
}

/// The longest quiet spell that still counts as working: a spec runs for minutes between journal entries, but a run
/// left overnight and rerun the next morning shouldn't count the night.
const ACTIVE_GAP_MS: u64 = 30 * 60_000;

/// How long the run has been working: the time between its journal entries, leaving out quiet spells longer than
/// [`ACTIVE_GAP_MS`].
pub fn active_ms(entries: &[Entry]) -> u64 {
    entries
        .windows(2)
        .map(|w| w[1].at.saturating_sub(w[0].at))
        .filter(|gap| *gap <= ACTIVE_GAP_MS)
        .sum()
}

/// 724k, 1.2M: a header fact, where the exact count is noise.
pub fn thousands(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

#[derive(Default, Clone, Copy)]
struct Tally {
    total: u32,
    pass: u32,
    fail: u32,
    blocked: u32,
    checking: u32,
    running: u32,
}

impl Tally {
    fn add(&mut self, r: &RowRecord) {
        self.total += 1;
        match &r.row {
            Row::Filed { result, .. } => match result {
                Final::Pass => self.pass += 1,
                Final::Fail => self.fail += 1,
                Final::Blocked => self.blocked += 1,
            },
            Row::NeedsReview { .. } => self.checking += 1,
            Row::Running { .. } => self.running += 1,
            Row::NotRun | Row::Queued { .. } => {}
        }
    }
    fn pct(&self, n: u32) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            f64::from(n) * 100.0 / f64::from(self.total)
        }
    }
}

fn ring(t: &Tally) -> String {
    let mut out = String::from(
        "<svg class=\"ring\" viewBox=\"0 0 120 120\" aria-hidden=\"true\"><circle cx=\"60\" cy=\"60\" r=\"44\" class=\"ring-bg\"/>",
    );
    for s in view::ring_segments(t) {
        let _ = write!(
            out,
            "<circle cx=\"60\" cy=\"60\" r=\"44\" class=\"ring-seg {}\" stroke-dasharray=\"{}\" stroke-dashoffset=\"{}\"/>",
            s.class, s.dash, s.offset
        );
    }
    out.push_str("</svg>");
    out
}

fn findings_for<'p>(
    page: &'p Page,
    spec: &str,
    platform: Platform,
) -> Vec<&'p crate::run::model::Problem> {
    page.run
        .state()
        .problems
        .iter()
        .filter(|p| p.spec == spec && p.platform == platform && p.kind == ProblemKind::Finding)
        .collect()
}

/// Whether the row's current attempt has no result in yet.
fn waiting(r: &RowRecord) -> bool {
    matches!(
        r.row,
        Row::NotRun | Row::Queued { .. } | Row::Running { .. }
    )
}

/// An outcome's wording as the latest result judged it, so a spec edited since then doesn't reword the result.
fn judged_as<'a>(spec: &'a Spec, r: &'a RowRecord, i: usize) -> &'a str {
    r.judged_against
        .get(i)
        .or_else(|| spec.outcomes.get(i))
        .map_or("", String::as_str)
}

fn image(page: &Page, name: &str) -> Option<String> {
    let dir = page.run_dir?.join(EVIDENCE_DIR);
    // Older journals hold names as the tester typed them, sometimes without an extension.
    let name = [
        name.to_string(),
        format!("{name}.webp"),
        format!("{name}.png"),
    ]
    .into_iter()
    .find(|f| safe_name(f) && dir.join(f).is_file())?;
    let path = dir.join(&name);
    if page.export.is_some() {
        let bytes = std::fs::read(&path).ok()?;
        let kind = match path.extension().and_then(|e| e.to_str()) {
            Some("webp") => "image/webp",
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            _ => return None,
        };
        Some(format!(
            "data:{kind};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    } else {
        Some(format!("files/{EVIDENCE_DIR}/{name}"))
    }
}

/// What the runs index shows for a run, rewritten after every event.
pub fn run_summary(run: &Run, label: &str) -> serde_json::Value {
    let state = run.state();
    let mut t = Tally::default();
    for r in state.rows.values() {
        t.add(r);
    }
    let entries = run.entries_after(0);
    serde_json::json!({
        "label": label,
        "started": entries.first().map(|e| e.at),
        "updated": entries.last().map(|e| e.at),
        "finished": state.finished,
        "verdict": verdict(state),
        "counts": { "total": t.total, "pass": t.pass, "fail": t.fail, "blocked": t.blocked },
    })
}

/// The list of runs: each opens its live page (the current run) or its export.
pub fn index(runs: &[(String, serde_json::Value, Option<String>)], current: &str) -> String {
    let mut rows = String::new();
    for (name, s, link) in runs {
        let count = |k: &str| s["counts"][k].as_u64().unwrap_or(0) as u32;
        let t = Tally {
            total: count("total"),
            pass: count("pass"),
            fail: count("fail"),
            blocked: count("blocked"),
            ..Tally::default()
        };
        let status = s["verdict"]["status"].as_str().unwrap_or("not-ready");
        let (class, word) = match status {
            "failing" => ("failing", "Failing"),
            "ready" => ("", "Ready"),
            _ => ("", "Not ready"),
        };
        let reasons = s["verdict"]["reasons"]
            .as_array()
            .map(|r| {
                r.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(" · ")
            })
            .unwrap_or_default();
        let href = link.as_deref().map(esc);
        let when = s["started"].as_u64().map(date_time).unwrap_or_default();
        let live = if name == current && !s["finished"].as_bool().unwrap_or(false) {
            " <span class=\"chip ticket\">Live</span>"
        } else {
            ""
        };
        let inner = format!(
            "<span class=\"run-ring\">{}</span><span class=\"run-text\"><span class=\"run-title\"><b>{}</b>{live}</span><span class=\"muted\">{when}</span></span><span class=\"run-verdict\"><strong class=\"{class}\">{word}</strong><span class=\"muted\">{}</span></span>",
            ring(&t),
            esc(s["label"].as_str().unwrap_or(name)),
            esc(&reasons)
        );
        match href {
            Some(href) => {
                let _ = write!(rows, "<a class=\"run-row\" href=\"{href}\">{inner}</a>");
            }
            None => {
                let _ = write!(rows, "<div class=\"run-row\">{inner}</div>");
            }
        }
    }
    if rows.is_empty() {
        rows = "<p class=\"empty\">No runs yet.</p>".into();
    }
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1,viewport-fit=cover\">{APP_HEAD}<title>Runs · soundcheck</title><style>{CSS}</style>{THEME_HEAD}</head><body><div id=\"live\" data-live=\"false\"><main><header class=\"head\"><div class=\"head-text\"><span class=\"eyebrow\">Soundcheck · local regression</span><h1>Runs</h1></div></header><section class=\"card runs\">{rows}</section></main></div><script>{APP}</script></body></html>\n"
    )
}
